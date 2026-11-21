//! zj-sidebar: a vertical, directory-grouped tab list (`mode "tabs"`), a toggleable
//! git + agent-session panel (`mode "panel"`), and two floating overlays: a side-by-side
//! diff (`mode "diff"`) and an IDEA-style Git Log window (`mode "log"`).

/// Trace to zellij's log (plugin stderr) when built with `--features debug-log`; compiled out
/// otherwise. Each line carries the plugin id and the client this copy of the plugin serves.
#[macro_export]
macro_rules! trace {
    ($shared:expr, $($arg:tt)*) => {
        if cfg!(feature = "debug-log") {
            let ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() % 100_000)
                .unwrap_or(0);
            eprintln!("ZJSB {:>5}.{:03} plugin={} client={} | {}", ms / 1000, ms % 1000, $shared.my_id, $shared.my_client, format!($($arg)*));
        }
    };
}

mod commit;
mod diff;
mod diffparse;
mod git;
mod gitctl;
mod gitlog;
mod gitwin;
mod highlight;
mod panel;
mod sessions;
mod tabs;
mod ui;
mod widgets;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use zellij_tile::prelude::*;

pub const TOGGLE_PIPE: &str = "toggle_panel";
/// focus the panel (open it if needed), or go back from it to the shell
pub const FOCUS_PIPE: &str = "focus_panel";
/// open the Git Log window for the focused shell's directory
pub const LOG_PIPE: &str = "git_log";

const CWD_RETRY_MS: u128 = 30_000;

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// zellij may report cwds with a trailing separator (`C:\dir\`); drop it.
fn clean_cwd(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy().into_owned();
    let t = s.trim_end_matches(['/', '\\']);
    if t.is_empty() || t.ends_with(':') { p } else { PathBuf::from(t) }
}

#[derive(Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Tabs,
    Panel,
    Diff,
    Log,
}

/// Session-wide facts both modes need.
#[derive(Default)]
pub struct Shared {
    pub config: BTreeMap<String, String>,
    pub permitted: bool,
    pub my_id: u32,
    /// zellij runs a separate copy of every plugin for each attached client; this is ours
    pub my_client: ClientId,
    /// Our client left the session. zellij keeps a departed client's plugin copies loaded and
    /// still sends them keybinding pipes and timers, but no more tab/pane updates, so this copy
    /// only sees a frozen session. It must not act on it (that left empty placeholder panes)
    /// and stops its refresh timer (the departed copies' polling piled up).
    pub client_gone: bool,
    last_alive_check: u128,
    pub tabs: Vec<TabInfo>,
    pub panes: PaneManifest,
    /// terminal pane id -> last known cwd
    pub cwds: HashMap<u32, PathBuf>,
    /// terminal pane id -> when the last cwd lookup for it failed (ms since epoch)
    pub cwd_failed: HashMap<u32, u128>,
}

impl Shared {
    pub fn my_tab(&self) -> Option<usize> {
        self.panes.panes.iter().find_map(|(pos, panes)| {
            panes
                .iter()
                .any(|p| p.is_plugin && p.id == self.my_id)
                .then_some(*pos)
        })
    }

    pub fn my_pane(&self) -> Option<&PaneInfo> {
        self.panes
            .panes
            .values()
            .flatten()
            .find(|p| p.is_plugin && p.id == self.my_id)
    }

    pub fn tab_at(&self, pos: usize) -> Option<&TabInfo> {
        self.tabs.iter().find(|t| t.position == pos)
    }

    pub fn my_tab_active(&self) -> bool {
        self.my_tab()
            .and_then(|p| self.tab_at(p))
            .is_some_and(|t| t.active)
    }

    /// Keybinding and CLI pipes reach this tab's copy once per attached client and don't
    /// say which client sent them. Of the clients looking at this tab, only the copy of the
    /// lowest client id acts, so a second client (another terminal, a web view) doesn't
    /// make every action happen twice.
    /// Whether zellij still knows our client. A client that left has no active tab, and
    /// zellij answers "Client … not found"; any other error (a timeout) proves nothing.
    pub fn client_alive() -> bool {
        match get_focused_pane_info() {
            Ok(_) => true,
            Err(e) => !e.contains("not found"),
        }
    }

    pub fn leads_my_tab(&self) -> bool {
        self.my_tab()
            .and_then(|p| self.tab_at(p))
            .is_none_or(|t| t.other_focused_clients.iter().all(|c| *c > self.my_client))
    }

    /// The tiled terminal pane that represents a tab (its focused one, else the first).
    pub fn main_pane(&self, pos: usize) -> Option<&PaneInfo> {
        let panes = self.panes.panes.get(&pos)?;
        let mut terms = panes
            .iter()
            .filter(|p| !p.is_plugin && !p.is_floating && !p.is_suppressed && p.is_selectable);
        let first = terms.clone().next();
        terms.find(|p| p.is_focused).or(first)
    }

    /// Cached cwd of a terminal pane, asking zellij when unknown (or when `fresh`). A lookup
    /// that fails (zellij answers GetPaneCwd with a 100 ms timeout when it is busy) is not
    /// repeated for CWD_RETRY_MS: every render asks for every tab, so retrying each time
    /// stalls this plugin and floods the server.
    pub fn cwd_of(&mut self, pane: u32, fresh: bool) -> Option<PathBuf> {
        self.lookup_cwd(pane, fresh, false)
    }

    /// Like `cwd_of(pane, true)` but for an action the user just asked for: always tries.
    pub fn cwd_now(&mut self, pane: u32) -> Option<PathBuf> {
        self.lookup_cwd(pane, true, true)
    }

    fn lookup_cwd(&mut self, pane: u32, fresh: bool, force: bool) -> Option<PathBuf> {
        if fresh || !self.cwds.contains_key(&pane) {
            if !self.permitted {
                return self.cwds.get(&pane).cloned();
            }
            let now = now_ms();
            let backing_off = self.cwd_failed.get(&pane).is_some_and(|t| now.saturating_sub(*t) < CWD_RETRY_MS);
            if force || !backing_off {
                match get_pane_cwd(PaneId::Terminal(pane)) {
                    Ok(cwd) => {
                        self.cwd_failed.remove(&pane);
                        self.cwds.insert(pane, clean_cwd(cwd));
                    }
                    Err(_) => {
                        self.cwd_failed.insert(pane, now);
                    }
                }
            }
        }
        self.cwds.get(&pane).cloned()
    }

    pub fn cfg(&self, key: &str) -> Option<&str> {
        self.config.get(key).map(String::as_str).filter(|s| !s.is_empty())
    }
}

#[derive(Default)]
struct State {
    mode: Mode,
    shared: Shared,
    tabs: tabs::TabsView,
    panel: panel::Panel,
    diff: diff::DiffView,
    log: gitwin::GitWin,
}

impl State {
    fn dispatch(&mut self, event: &Event) -> bool {
        match self.mode {
            Mode::Tabs => self.tabs.update(&mut self.shared, event),
            Mode::Panel => self.panel.update(&mut self.shared, event),
            Mode::Diff => self.diff.update(&mut self.shared, event),
            Mode::Log => self.log.update(&mut self.shared, event),
        }
    }
}

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        self.mode = match configuration.get("mode").map(String::as_str) {
            Some("panel") => Mode::Panel,
            Some("diff") => Mode::Diff,
            Some("log") => Mode::Log,
            _ => Mode::Tabs,
        };
        self.shared.config = configuration;
        let ids = get_plugin_ids();
        self.shared.my_id = ids.plugin_id;
        self.shared.my_client = ids.client_id;
        trace!(self.shared, "load mode={:?} target_pane={:?} placeholder={:?}",
            self.shared.config.get("mode"), self.shared.config.get("target_pane"), self.shared.config.get("placeholder"));
        // Same list in every mode so a single grant covers the plugin file.
        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ChangeApplicationState,
            PermissionType::RunCommands,
            PermissionType::FullHdAccess,
            PermissionType::WriteToStdin,
            PermissionType::OpenTerminalsOrPlugins,
            PermissionType::MessageAndLaunchOtherPlugins,
            PermissionType::ReadSessionEnvironmentVariables,
            PermissionType::ReadCliPipes,
            PermissionType::RunActionsAsUser,
            PermissionType::WriteToClipboard,
        ]);
        let mut events = vec![
            EventType::PermissionRequestResult,
            EventType::TabUpdate,
            EventType::PaneUpdate,
            EventType::Mouse,
            EventType::Timer,
            EventType::CwdChanged,
        ];
        if self.mode == Mode::Panel {
            events.extend([
                EventType::Key,
                EventType::RunCommandResult,
                EventType::HostFolderChanged,
                EventType::FailedToChangeHostFolder,
            ]);
            self.panel.load(&self.shared);
        }
        if self.mode == Mode::Diff {
            // the overlay only needs input and its own git output (TabUpdate: see `update`)
            events = vec![
                EventType::PermissionRequestResult,
                EventType::TabUpdate,
                EventType::Key,
                EventType::Mouse,
                EventType::RunCommandResult,
            ];
            self.diff.load(&self.shared);
        }
        if self.mode == Mode::Log {
            events = vec![
                EventType::PermissionRequestResult,
                EventType::TabUpdate,
                EventType::Key,
                EventType::Mouse,
                EventType::RunCommandResult,
            ];
            self.log.load(&self.shared);
        }
        subscribe(&events);
    }

    fn update(&mut self, event: Event) -> bool {
        // Some terminals (and ConPTY) deliver Enter as LF / CR with Ctrl: Ctrl+J / Ctrl+M.
        // (Alt+Enter, the commit message's new line, then arrives as Alt+Ctrl+J.)
        let event = match event {
            Event::Key(k)
                if matches!(k.bare_key, BareKey::Char('j') | BareKey::Char('m'))
                    && k.key_modifiers.contains(&KeyModifier::Ctrl)
                    && k.key_modifiers.iter().all(|m| matches!(m, KeyModifier::Ctrl | KeyModifier::Alt)) =>
            {
                let mut e = KeyWithModifier::new(BareKey::Enter);
                if k.key_modifiers.contains(&KeyModifier::Alt) {
                    e = e.with_alt_modifier();
                }
                Event::Key(e)
            }
            e => e,
        };
        let granted = matches!(event, Event::PermissionRequestResult(PermissionStatus::Granted));
        if self.shared.permitted {
            let now = now_ms();
            match &event {
                // A departed client's copy still gets its timer. Check at most every 5 s and,
                // once the client is gone, drop the tick so the timer is not re-armed.
                Event::Timer(_) if self.shared.client_gone => return false,
                Event::Timer(_) if now.saturating_sub(self.shared.last_alive_check) >= 5_000 => {
                    self.shared.last_alive_check = now;
                    if !Shared::client_alive() {
                        self.shared.client_gone = true;
                        trace!(self.shared, "client left the session: going dormant");
                        return false;
                    }
                }
                // updates only reach connected clients: zellij reused our client id, wake up
                Event::TabUpdate(_) | Event::PaneUpdate(_) if self.shared.client_gone && Shared::client_alive() => {
                    self.shared.client_gone = false;
                    self.shared.last_alive_check = now;
                    trace!(self.shared, "client is back: waking up");
                    set_timeout(2.0);
                }
                _ => {}
            }
        }
        match &event {
            Event::TabUpdate(tabs) => self.shared.tabs = tabs.clone(),
            Event::PaneUpdate(manifest) => self.shared.panes = manifest.clone(),
            Event::CwdChanged(PaneId::Terminal(id), cwd, _) => {
                self.shared.cwds.insert(*id, clean_cwd(cwd.clone()));
            }
            _ => {}
        }
        // The copies zellij starts for a client that attaches later can miss the Granted
        // event, which left them without cwds (one "…" group) and without the refresh timer.
        // zellij only delivers tab/pane updates once ReadApplicationState is granted, so
        // they prove the grant too. Run the one-time setup on whichever comes first, once.
        let mut render = false;
        if !self.shared.permitted && (granted || matches!(event, Event::TabUpdate(_) | Event::PaneUpdate(_))) {
            self.shared.permitted = true;
            render = self.dispatch(&Event::PermissionRequestResult(PermissionStatus::Granted));
        }
        if granted {
            return render; // handled above, or a repeat that must not start a second timer
        }
        self.dispatch(&event) || render
    }

    fn pipe(&mut self, msg: PipeMessage) -> bool {
        if let PipeSource::Cli(id) = &msg.source {
            unblock_cli_pipe_input(id);
        }
        if self.mode != Mode::Tabs {
            return false;
        }
        let action = [TOGGLE_PIPE, FOCUS_PIPE, LOG_PIPE].contains(&msg.name.as_str());
        if action {
            let tab = self.shared.my_tab();
            let others = tab.and_then(|p| self.shared.tab_at(p)).map(|t| t.other_focused_clients.clone());
            trace!(self.shared, "pipe {} from {:?}: my_tab={:?} tab_active={} other_clients_on_tab={:?} leads={}",
                msg.name, msg.source, tab, self.shared.my_tab_active(), others, self.shared.leads_my_tab());
        }
        if action && !self.shared.leads_my_tab() {
            return false;
        }
        // a departed client's copy still receives keybinding pipes; its view is frozen
        if action && (self.shared.client_gone || !Shared::client_alive()) {
            trace!(self.shared, "pipe {}: skip, our client left the session", msg.name);
            return false;
        }
        if msg.name == TOGGLE_PIPE {
            self.tabs.toggle_panel(&mut self.shared);
            return false;
        }
        if msg.name == FOCUS_PIPE {
            self.tabs.focus_panel(&mut self.shared);
            return false;
        }
        if msg.name == LOG_PIPE {
            self.tabs.open_git_log(&mut self.shared);
            return false;
        }
        let from = match msg.source {
            PipeSource::Plugin(id) => Some(id),
            _ => None,
        };
        self.tabs.on_message(from, &msg.name, msg.payload.as_deref())
    }

    fn render(&mut self, rows: usize, cols: usize) {
        match self.mode {
            Mode::Tabs => self.tabs.render(&mut self.shared, rows, cols),
            Mode::Panel => self.panel.render(&mut self.shared, rows, cols),
            Mode::Diff => self.diff.render(&mut self.shared, rows, cols),
            Mode::Log => self.log.render(&mut self.shared, rows, cols),
        }
    }
}
