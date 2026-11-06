//! zj-sidebar: a vertical, directory-grouped tab list (`mode "tabs"`), a toggleable
//! git + agent-session panel (`mode "panel"`), and two floating overlays: a side-by-side
//! diff (`mode "diff"`) and an IDEA-style Git Log window (`mode "log"`).

mod commit;
mod diff;
mod diffparse;
mod git;
mod gitctl;
mod gitlog;
mod gitwin;
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
    pub tabs: Vec<TabInfo>,
    pub panes: PaneManifest,
    /// terminal pane id -> last known cwd
    pub cwds: HashMap<u32, PathBuf>,
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

    /// The tiled terminal pane that represents a tab (its focused one, else the first).
    pub fn main_pane(&self, pos: usize) -> Option<&PaneInfo> {
        let panes = self.panes.panes.get(&pos)?;
        let mut terms = panes
            .iter()
            .filter(|p| !p.is_plugin && !p.is_floating && !p.is_suppressed && p.is_selectable);
        let first = terms.clone().next();
        terms.find(|p| p.is_focused).or(first)
    }

    /// Cached cwd of a terminal pane, asking zellij when unknown (or when `fresh`).
    pub fn cwd_of(&mut self, pane: u32, fresh: bool) -> Option<PathBuf> {
        if fresh || !self.cwds.contains_key(&pane) {
            if !self.permitted {
                return self.cwds.get(&pane).cloned();
            }
            if let Ok(cwd) = get_pane_cwd(PaneId::Terminal(pane)) {
                self.cwds.insert(pane, clean_cwd(cwd));
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
        self.shared.my_id = get_plugin_ids().plugin_id;
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
            // the overlay only needs input and its own git output
            events = vec![
                EventType::PermissionRequestResult,
                EventType::Key,
                EventType::Mouse,
                EventType::RunCommandResult,
            ];
            self.diff.load(&self.shared);
        }
        if self.mode == Mode::Log {
            events = vec![
                EventType::PermissionRequestResult,
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
        match &event {
            Event::PermissionRequestResult(PermissionStatus::Granted) => {
                self.shared.permitted = true;
            }
            Event::TabUpdate(tabs) => self.shared.tabs = tabs.clone(),
            Event::PaneUpdate(manifest) => self.shared.panes = manifest.clone(),
            Event::CwdChanged(PaneId::Terminal(id), cwd, _) => {
                self.shared.cwds.insert(*id, clean_cwd(cwd.clone()));
            }
            _ => {}
        }
        match self.mode {
            Mode::Tabs => self.tabs.update(&mut self.shared, &event),
            Mode::Panel => self.panel.update(&mut self.shared, &event),
            Mode::Diff => self.diff.update(&mut self.shared, &event),
            Mode::Log => self.log.update(&mut self.shared, &event),
        }
    }

    fn pipe(&mut self, msg: PipeMessage) -> bool {
        if let PipeSource::Cli(id) = &msg.source {
            unblock_cli_pipe_input(id);
        }
        if self.mode != Mode::Tabs {
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
