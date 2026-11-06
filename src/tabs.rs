//! Vertical tab list grouped by each tab's working directory.

use std::collections::{BTreeMap, BTreeSet};

use zellij_tile::prelude::actions::Action;
use zellij_tile::prelude::*;
use zellij_utils::input::command::RunCommandAction;

use crate::ui::{self, Line, BOLD, DIM, MAGENTA, YELLOW};
use crate::Shared;

/// Every tab runs its own instance; folding is kept in sync through these messages.
pub const COLLAPSED_PIPE: &str = "zjsb_collapsed";
pub const COLLAPSED_REQUEST_PIPE: &str = "zjsb_collapsed_request";
const REFRESH_SECS: f64 = 2.0;
const OPEN_DELAY_SECS: f64 = 0.05;
const PANEL_TITLE: &str = "git · sessions";
/// Short-lived pane that reserves the panel's spot to the right of the focused pane.
const PLACEHOLDER_TITLE: &str = "zjsb-placeholder";
/// Waits on stdin forever: cheap, available wherever git is, and still alive
/// (so killable) when we close it.
const PLACEHOLDER_CMD: &[&str] = &["git", "hash-object", "--stdin"];

enum Click {
    Group(String),
    Tab(usize),
    NewTab,
    Panel,
}

struct TabRow {
    position: usize,
    label: String,
    active: bool,
    bell: bool,
}

struct Group {
    key: String,
    label: String,
    path: String,
    tabs: Vec<TabRow>,
}

#[derive(Default)]
pub struct TabsView {
    collapsed: BTreeSet<String>,
    clicks: Vec<Option<Click>>,
    scroll: usize,
    was_active: bool,
    last_toggle: u128,
    /// whether our folding state came from (or was shared with) other instances
    synced: bool,
    requested_sync: bool,
    /// panel open requested from a pipe, carried out on the next timer
    open_requested: Option<String>,
    /// panel waiting for its placeholder pane: (plugin url, config, updates waited)
    pending_panel: Option<(String, BTreeMap<String, String>, u8)>,
    /// terminal panes of the tab before the placeholder was requested
    known_terms: Vec<u32>,
    /// placeholder pane to close once it resurfaces, and whether it was seen suppressed
    placeholder: Option<(u32, bool)>,
    /// the tab's terminal pane that had focus last; focus returns there from the panel
    last_term: Option<u32>,
    /// focus change requested from a pipe, carried out on the next timer
    focus_requested: Option<PaneId>,
    /// Git Log window requested from a pipe: (plugin url, directory)
    log_requested: Option<(String, String)>,
    last_focus_pipe: u128,
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn tab_label(tab: &TabInfo, pane: Option<&PaneInfo>, all: &[PaneInfo]) -> String {
    let title = pane.map(|p| p.title.trim()).unwrap_or("");
    // Plugins see unnamed tabs titled after their focused pane, not "Tab #N".
    let auto_named =
        tab.name.starts_with("Tab #") || all.iter().any(|p| p.title.trim() == tab.name.trim());
    if !auto_named {
        return tab.name.clone();
    }
    if pane.is_none() {
        return "…".into();
    }
    if title.contains(" - PowerShell") || title.ends_with("PowerShell") {
        return "pwsh".into();
    }
    let looks_like_path = title.contains('\\') || title.contains('/') || title.ends_with(".exe");
    let t = if looks_like_path { ui::basename(title) } else { title };
    let t = t.strip_suffix(".exe").unwrap_or(t);
    if t.is_empty() { "shell".into() } else { t.to_string() }
}

impl TabsView {
    /// Send a message to every other instance of this plugin, addressed by pane id so
    /// zellij never launches a new instance for it.
    fn broadcast(shared: &Shared, name: &str, payload: String) {
        let Some(url) = shared.my_pane().and_then(|p| p.plugin_url.clone()) else { return };
        let peers = shared
            .panes
            .panes
            .values()
            .flatten()
            .filter(|p| p.is_plugin && p.id != shared.my_id && p.plugin_url.as_deref() == Some(&url));
        for p in peers {
            pipe_message_to_plugin(
                MessageToPlugin::new(name)
                    .with_destination_plugin_id(p.id)
                    .with_payload(payload.clone()),
            );
        }
    }

    fn collapsed_payload(&self) -> String {
        self.collapsed.iter().map(String::as_str).collect::<Vec<_>>().join("\n")
    }

    /// Handle folding-sync messages from other instances; true when a re-render is needed.
    pub fn on_message(&mut self, from: Option<u32>, name: &str, payload: Option<&str>) -> bool {
        match name {
            COLLAPSED_PIPE => {
                self.collapsed = payload
                    .unwrap_or_default()
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(String::from)
                    .collect();
                self.synced = true;
                true
            }
            COLLAPSED_REQUEST_PIPE if self.synced || !self.collapsed.is_empty() => {
                // reply to the asker directly: our pane list may not include it yet
                if let Some(id) = from {
                    pipe_message_to_plugin(
                        MessageToPlugin::new(COLLAPSED_PIPE)
                            .with_destination_plugin_id(id)
                            .with_payload(self.collapsed_payload()),
                    );
                }
                false
            }
            _ => false,
        }
    }

    fn refresh_cwds(&self, shared: &mut Shared) {
        let mains: Vec<u32> = shared
            .tabs
            .iter()
            .filter_map(|t| shared.main_pane(t.position).map(|p| p.id))
            .collect();
        for id in mains {
            shared.cwd_of(id, true);
        }
    }

    pub fn update(&mut self, shared: &mut Shared, event: &Event) -> bool {
        match event {
            Event::PermissionRequestResult(PermissionStatus::Granted) => {
                set_selectable(false);
                set_timeout(REFRESH_SECS);
                true
            }
            Event::TabUpdate(_) | Event::PaneUpdate(_) => {
                // a new tab asks the others for the current folding state (once we know our peers)
                if shared.permitted && !self.requested_sync && shared.my_pane().is_some() {
                    self.requested_sync = true;
                    Self::broadcast(shared, COLLAPSED_REQUEST_PIPE, String::new());
                }
                self.place_panel(shared);
                if let Some(t) = shared.my_tab().and_then(|pos| shared.panes.panes.get(&pos)).and_then(|ps| {
                    ps.iter().find(|p| p.is_focused && self.is_user_term(p)).map(|p| p.id)
                }) {
                    self.last_term = Some(t);
                }
                let active = shared.my_tab_active();
                if active && !self.was_active && shared.permitted {
                    self.refresh_cwds(shared);
                }
                self.was_active = active;
                true
            }
            Event::Timer(elapsed) => {
                if let Some(url) = self.open_requested.take() {
                    self.open_panel(shared, url);
                }
                if let Some(id) = self.focus_requested.take() {
                    focus_pane_with_id(id, false, false);
                }
                if let Some((url, cwd)) = self.log_requested.take() {
                    Self::launch_git_log(url, cwd);
                }
                if *elapsed < REFRESH_SECS / 2.0 {
                    return false; // the one-shot open timer, not the refresh loop
                }
                let active = shared.my_tab_active();
                if active {
                    self.refresh_cwds(shared);
                }
                set_timeout(REFRESH_SECS);
                active
            }
            Event::CwdChanged(..) => true,
            Event::Mouse(Mouse::LeftClick(line, _)) => {
                let Ok(line) = usize::try_from(*line) else { return false };
                match self.clicks.get(line) {
                    Some(Some(Click::Group(key))) => {
                        if !self.collapsed.remove(key) {
                            self.collapsed.insert(key.clone());
                        }
                        self.synced = true;
                        Self::broadcast(shared, COLLAPSED_PIPE, self.collapsed_payload());
                        true
                    }
                    Some(Some(Click::Tab(pos))) => {
                        switch_tab_to(*pos as u32 + 1);
                        false
                    }
                    Some(Some(Click::Panel)) => {
                        self.toggle_panel(shared);
                        false
                    }
                    Some(Some(Click::NewTab)) => {
                        let cwd = shared
                            .tabs
                            .iter()
                            .find(|t| t.active)
                            .and_then(|t| shared.main_pane(t.position).map(|p| p.id))
                            .and_then(|id| shared.cwd_of(id, false))
                            .map(|p| p.to_string_lossy().into_owned());
                        new_tab(None, cwd);
                        false
                    }
                    _ => false,
                }
            }
            Event::Mouse(Mouse::ScrollUp(_)) => {
                self.scroll = self.scroll.saturating_sub(1);
                true
            }
            Event::Mouse(Mouse::ScrollDown(_)) => {
                self.scroll += 1;
                true
            }
            _ => false,
        }
    }

    /// Open the panel to the right of the current tab, or close it if it is open.
    /// Pipes reach every tab's instance; only the one in the focused tab acts. Background
    /// instances can hold stale tab state, so ask zellij where the focus is right now.
    fn in_focused_tab(shared: &Shared) -> bool {
        match (get_focused_pane_info(), shared.my_tab()) {
            (Ok((tab, _)), Some(pos)) => tab == pos,
            _ => shared.my_tab_active(),
        }
    }

    /// The panel of this tab (tiled; floating instances of the plugin are overlays).
    fn find_panel<'a>(shared: &'a Shared, pos: usize, url: &str) -> Option<&'a PaneInfo> {
        shared.panes.panes.get(&pos)?.iter().find(|p| {
            p.is_plugin
                && p.id != shared.my_id
                && !p.is_floating
                && (p.title == PANEL_TITLE || p.plugin_url.as_deref() == Some(url))
        })
    }

    /// A shell of the user's: a tiled terminal, not the panel's throwaway placeholder.
    fn is_user_term(&self, p: &PaneInfo) -> bool {
        !p.is_plugin
            && !p.is_floating
            && !p.is_suppressed
            && !p.exited
            && p.title != PLACEHOLDER_TITLE
            && self.placeholder.map(|(id, _)| id) != Some(p.id)
    }

    /// Where focus goes back to from the panel: the shell used last, if it's still there.
    fn back_target(&self, shared: &Shared, pos: usize) -> Option<u32> {
        let panes = shared.panes.panes.get(&pos)?;
        let ok = |id: u32| panes.iter().any(|p| p.id == id && self.is_user_term(p));
        self.last_term
            .filter(|&id| ok(id))
            .or_else(|| panes.iter().find(|p| self.is_user_term(p)).map(|p| p.id))
    }

    fn opening(&self) -> bool {
        self.open_requested.is_some() || self.pending_panel.is_some() || matches!(self.placeholder, Some((_, false)))
    }

    /// `focus_panel` pipe: move the keyboard into the panel (opening it if needed), or
    /// from the panel back to the shell it belongs to.
    pub fn focus_panel(&mut self, shared: &mut Shared) {
        if !Self::in_focused_tab(shared) {
            return;
        }
        let now = now_ms();
        if now.saturating_sub(self.last_focus_pipe) < 300 || self.opening() {
            return; // delivered twice, or the panel is still being put in place
        }
        self.last_focus_pipe = now;
        let (Some(pos), Some(url)) = (shared.my_tab(), shared.my_pane().and_then(|p| p.plugin_url.clone())) else {
            return;
        };
        match Self::find_panel(shared, pos, &url).map(|p| (p.id, p.is_focused)) {
            Some((_, true)) => {
                let back = self.back_target(shared, pos);
                self.focus_requested = back.map(PaneId::Terminal);
            }
            Some((id, false)) => self.focus_requested = Some(PaneId::Plugin(id)),
            // no panel yet: open one (it takes the focus itself)
            None => self.open_requested = Some(url),
        }
        // actions run from inside a keybinding's pipe are dropped, so act on a timer
        set_timeout(OPEN_DELAY_SECS);
    }

    /// `git_log` pipe: open the Git Log window for the directory of the focused shell.
    pub fn open_git_log(&mut self, shared: &mut Shared) {
        if !Self::in_focused_tab(shared) {
            return;
        }
        let now = now_ms();
        if now.saturating_sub(self.last_focus_pipe) < 300 {
            return;
        }
        self.last_focus_pipe = now;
        let (Some(pos), Some(url)) = (shared.my_tab(), shared.my_pane().and_then(|p| p.plugin_url.clone())) else {
            return;
        };
        let term = self.back_target(shared, pos);
        let Some(cwd) = term.and_then(|t| shared.cwd_of(t, true)) else { return };
        self.log_requested = Some((url, cwd.to_string_lossy().into_owned()));
        set_timeout(OPEN_DELAY_SECS);
    }

    fn launch_git_log(url: String, cwd: String) {
        let config = BTreeMap::from([
            ("mode".to_string(), "log".to_string()),
            ("log_cwd".to_string(), cwd),
            ("plugin_url".to_string(), url.clone()),
            // unique per request, so every press opens a fresh window
            ("n".to_string(), format!("log{}", now_ms())),
        ]);
        let mut msg = MessageToPlugin::new("open")
            .with_plugin_url(url)
            .with_plugin_config(config)
            .new_plugin_instance_should_float(true)
            .new_plugin_instance_should_have_pane_title("Git Log")
            .new_plugin_instance_should_be_focused();
        if let Some(c) = FloatingPaneCoordinates::new(Some("2%".into()), Some("2%".into()), Some("96%".into()), Some("94%".into()), None, None) {
            msg = msg.with_floating_pane_coordinates(c);
        }
        pipe_message_to_plugin(msg);
    }

    pub fn toggle_panel(&mut self, shared: &mut Shared) {
        // Every tab's instance gets the message and background instances can hold
        // stale tab state, so ask zellij where the focus is right now.
        if !Self::in_focused_tab(shared) {
            return;
        }
        // A pipe can be delivered twice; don't undo ourselves or start a second
        // panel while the first one is still being put in place.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let opening = self.opening();
        let since = now.saturating_sub(self.last_toggle);
        if since < 400 || (opening && since < 3000) {
            return;
        }
        self.last_toggle = now;
        let (Some(pos), Some(url)) = (
            shared.my_tab(),
            shared.my_pane().and_then(|p| p.plugin_url.clone()),
        ) else {
            return;
        };
        let existing = Self::find_panel(shared, pos, &url).map(|p| p.id);
        if let Some(id) = existing {
            close_plugin_pane(id);
            return;
        }
        // Actions run from inside a keybinding's pipe are dropped, so open on a timer.
        self.open_requested = Some(url);
        set_timeout(OPEN_DELAY_SECS);
    }

    fn open_panel(&mut self, shared: &mut Shared, url: String) {
        let Some(pos) = shared.my_tab() else { return };
        let target = shared.main_pane(pos).map(|p| p.id);
        let cwd = target.and_then(|id| shared.cwd_of(id, true));
        let mut config: BTreeMap<String, String> = shared.config.clone();
        config.insert("mode".into(), "panel".into());
        if let Some(t) = target {
            config.insert("target_pane".into(), t.to_string());
        }
        if let Some(c) = &cwd {
            config.insert("target_cwd".into(), c.to_string_lossy().into_owned());
        }
        // zellij can't open a tiled plugin in a given direction, so split right with a
        // throwaway command pane first and swap the panel into its place.
        if let Some(t) = target {
            focus_pane_with_id(PaneId::Terminal(t), false, false);
        }
        self.known_terms = shared
            .panes
            .panes
            .get(&pos)
            .map(|ps| ps.iter().filter(|p| !p.is_plugin).map(|p| p.id).collect())
            .unwrap_or_default();
        let placeholder = RunCommandAction {
            command: PLACEHOLDER_CMD[0].into(),
            args: PLACEHOLDER_CMD[1..].iter().map(|a| a.to_string()).collect(),
            ..Default::default()
        };
        run_action(
            Action::NewTiledPane {
                direction: Some(Direction::Right),
                command: Some(placeholder),
                pane_name: Some(PLACEHOLDER_TITLE.into()),
                near_current_pane: false,
                no_focus: false,
                borderless: None,
                tab_id: None,
            },
            BTreeMap::new(),
        );
        self.pending_panel = Some((url, config, 0));
    }

    fn place_panel(&mut self, shared: &Shared) {
        let Some(pos) = shared.my_tab() else { return };
        let panes = shared.panes.panes.get(&pos);
        let fresh = panes.and_then(|ps| {
            ps.iter()
                .find(|p| !p.is_plugin && !self.known_terms.contains(&p.id))
                .cloned()
        });
        if let Some((url, config, waited)) = self.pending_panel.take() {
            match fresh {
                Some(p) if !p.is_suppressed => {
                    // `target_pane` makes the config unique, so this always launches a new panel
                    let mut config = config;
                    // the panel must never mistake this throwaway pane for the user's shell
                    config.insert("placeholder".into(), p.id.to_string());
                    pipe_message_to_plugin(
                        MessageToPlugin::new("open")
                            .with_plugin_url(url)
                            .with_plugin_config(config)
                            .new_plugin_instance_should_replace_pane(PaneId::Terminal(p.id))
                            .new_plugin_instance_should_have_pane_title(PANEL_TITLE)
                            .new_plugin_instance_should_be_focused(),
                    );
                    self.placeholder = Some((p.id, false));
                }
                _ if waited < 20 => self.pending_panel = Some((url, config, waited + 1)),
                _ => {}
            }
        }
        // The replaced pane is only suppressed and comes back when the panel closes
        // (closing it while suppressed leaves a dead pane behind), so close it then.
        if let Some((id, seen_suppressed)) = self.placeholder {
            match panes.and_then(|ps| ps.iter().find(|p| !p.is_plugin && p.id == id)) {
                Some(p) if p.is_suppressed => self.placeholder = Some((id, true)),
                Some(_) if seen_suppressed => close_terminal_pane(id),
                Some(_) => {}
                None => self.placeholder = None,
            }
        }
    }

    fn groups(&self, shared: &mut Shared) -> Vec<Group> {
        let mut groups: Vec<Group> = Vec::new();
        let mut tabs = shared.tabs.clone();
        tabs.sort_by_key(|t| t.position);
        for tab in &tabs {
            let pane = shared.main_pane(tab.position).cloned();
            let cwd = pane
                .as_ref()
                .and_then(|p| shared.cwd_of(p.id, false))
                .map(|p| p.to_string_lossy().into_owned());
            let key = cwd.as_deref().map(ui::norm_path).unwrap_or_else(|| "?".into());
            let row = TabRow {
                position: tab.position,
                label: tab_label(
                    tab,
                    pane.as_ref(),
                    shared.panes.panes.get(&tab.position).map(Vec::as_slice).unwrap_or(&[]),
                ),
                active: tab.active,
                bell: tab.has_bell_notification,
            };
            match groups.iter_mut().find(|g| g.key == key) {
                Some(g) => g.tabs.push(row),
                None => {
                    let path = cwd.unwrap_or_default();
                    groups.push(Group {
                        key,
                        label: if path.is_empty() { "…".into() } else { ui::basename(&path).to_string() },
                        path,
                        tabs: vec![row],
                    })
                }
            }
        }
        // Disambiguate equal folder names with their parent folder.
        let labels: Vec<String> = groups.iter().map(|g| g.label.clone()).collect();
        for g in &mut groups {
            if labels.iter().filter(|l| **l == g.label).count() > 1 {
                let parent = g.path.trim_end_matches(['/', '\\']);
                let parent = parent.rsplit_once(['/', '\\']).map(|(p, _)| ui::basename(p)).unwrap_or("");
                g.label = format!("{parent}/{}", g.label);
            }
        }
        groups
    }

    pub fn render(&mut self, shared: &mut Shared, rows: usize, cols: usize) {
        let cols = cols.saturating_sub(1);
        let mut lines: Vec<(Line, Option<Click>)> = Vec::new();
        let mut header = Line::new();
        header.push(&format!("{BOLD}{DIM}"), " TABS");
        header.right(DIM, &format!("{} ", shared.tabs.len()), cols);
        lines.push((header, None));

        for g in self.groups(shared) {
            let collapsed = self.collapsed.contains(&g.key);
            let has_active = g.tabs.iter().any(|t| t.active);
            let mut l = Line::new();
            l.push(DIM, if collapsed { " ▸ " } else { " ▾ " });
            let count = format!(" {}", g.tabs.len());
            let style = if has_active { format!("{BOLD}{MAGENTA}") } else { BOLD.to_string() };
            l.push(&style, &ui::truncate(&g.label, cols.saturating_sub(3 + count.len())));
            l.push(DIM, &count);
            if collapsed && g.tabs.iter().any(|t| t.bell) {
                l.push(YELLOW, " ●");
            }
            lines.push((l, Some(Click::Group(g.key.clone()))));
            // a folded group still shows the tab you're in
            for t in g.tabs.into_iter().filter(|t| !collapsed || t.active) {
                let mut l = Line::new();
                let num = format!("{} ", t.position + 1);
                let bell = if t.bell { " ●" } else { "" };
                let room = cols.saturating_sub(3 + num.len() + bell.len());
                if t.active {
                    l.push(MAGENTA, " ▌ ");
                    l.push(&format!("{BOLD}{MAGENTA}"), &num);
                    l.push(BOLD, &ui::truncate(&t.label, room));
                } else {
                    l.push("", "   ");
                    l.push(DIM, &num);
                    l.push("", &ui::truncate(&t.label, room));
                }
                l.push(YELLOW, bell);
                lines.push((l, Some(Click::Tab(t.position))));
            }
        }
        lines.push((Line::new(), None));
        let mut add = Line::new();
        add.push(DIM, " + new tab");
        lines.push((add, Some(Click::NewTab)));
        let mut panel = Line::new();
        panel.push(DIM, " ≡ git · sessions");
        lines.push((panel, Some(Click::Panel)));

        // Keep the header pinned; scroll the rest.
        let body = rows.saturating_sub(1);
        let max_scroll = lines.len().saturating_sub(1).saturating_sub(body);
        self.scroll = self.scroll.min(max_scroll);
        let mut iter = lines.into_iter();
        let mut shown = vec![iter.next().unwrap()];
        shown.extend(iter.skip(self.scroll).take(body));
        let (lines, clicks): (Vec<Line>, Vec<Option<Click>>) = shown.into_iter().unzip();
        self.clicks = clicks;
        ui::print_lines(&lines, rows, cols);
    }
}
