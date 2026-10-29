//! zj-sidebar: a vertical, directory-grouped tab list (`mode "tabs"`) and a
//! toggleable git + agent-session panel (`mode "panel"`) for zellij.

mod git;
mod panel;
mod sessions;
mod tabs;
mod ui;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use zellij_tile::prelude::*;

pub const TOGGLE_PIPE: &str = "toggle_panel";

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
}

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        self.mode = match configuration.get("mode").map(String::as_str) {
            Some("panel") => Mode::Panel,
            _ => Mode::Tabs,
        };
        self.shared.config = configuration;
        self.shared.my_id = get_plugin_ids().plugin_id;
        // Same list in both modes so a single grant covers the plugin file.
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
        subscribe(&events);
    }

    fn update(&mut self, event: Event) -> bool {
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
        }
    }
}
