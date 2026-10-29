//! Right-hand panel: git status of the current directory on top, Claude Code /
//! Codex sessions for that directory below.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use zellij_tile::prelude::*;

use crate::git::{GitFile, GitRaw, GitState};
use crate::sessions::{self, now_secs, Kind, Session, SessionScanner, MAX_SESSIONS};
use crate::ui::{self, Line, BLUE, BOLD, CYAN, DIM, GREEN, MAGENTA, RED, YELLOW};
use crate::Shared;

const TICK_SECS: f64 = 1.0;
const DEFAULT_WIDTH: usize = 44;

#[derive(Default)]
pub struct Panel {
    target: Option<u32>,
    cwd: Option<String>,
    home: Option<PathBuf>,
    host_ready: bool,
    git: GitRaw,
    scanner: SessionScanner,
    /// native helper that reads Codex's thread database
    helper: String,
    claude: Vec<Session>,
    codex: Vec<Session>,
    /// claude + codex, newest first
    sessions: Vec<Session>,
    /// shell pid of each terminal pane (stable for the pane's lifetime)
    pane_pids: HashMap<u32, u32>,
    /// session id -> terminal pane currently running it
    running: HashMap<String, u32>,
    /// changed files of the repo, in `git status` order
    files: Vec<GitFile>,
    /// repo root (`git rev-parse --show-toplevel`); `git status` paths are relative to it
    root: Option<String>,
    /// index over files followed by sessions
    /// which list the cursor is in
    focus: Focus,
    /// the user chose a list (otherwise: files if there are any, else sessions)
    user_focus: bool,
    /// cursor and scroll offset of the file list and of the session list, independently
    fsel: usize,
    ssel: usize,
    fscroll: usize,
    sscroll: usize,
    /// identity of the selected row ("path" / session id), to find it again after a refresh
    fkey: Option<String>,
    skey: Option<String>,
    /// the user moved that cursor; until then it stays on the first row
    fmoved: bool,
    smoved: bool,
    clicks: Vec<Option<(Focus, usize)>>,
    tick: u64,
    status: Option<String>,
    /// (tab id, command, give-up tick) for a session opened in a new tab
    pending_tab: Option<(usize, String, u64)>,
    /// (pane id, command, tick to type it anyway) waiting for the new shell's prompt
    pending_write: Option<(u32, String, u64)>,
    resize_steps: u32,
    /// our width when the last resize was requested
    resize_from: Option<usize>,
    focused_once: bool,
    last_resize: Option<Resize>,
}

/// Where the installers put the native helper, independent of zellij's config dir:
/// `%LOCALAPPDATA%\zj-sidebar\zjsb-helper.exe` or `$XDG_DATA_HOME/zj-sidebar/zjsb-helper`
/// (default `~/.local/share/zj-sidebar/zjsb-helper`).
fn default_helper(env: &BTreeMap<String, String>) -> String {
    let non_empty = |k: &str| env.get(k).filter(|v| !v.is_empty());
    if env.get("OS").is_some_and(|os| os == "Windows_NT") {
        if let Some(d) = non_empty("LOCALAPPDATA") {
            return format!(r"{d}\zj-sidebar\zjsb-helper.exe");
        }
    }
    let dir = match (non_empty("XDG_DATA_HOME"), non_empty("HOME")) {
        (Some(x), _) => format!("{x}/zj-sidebar"),
        (None, Some(h)) => format!("{h}/.local/share/zj-sidebar"),
        _ => return "zjsb-helper".into(),
    };
    format!("{dir}/zjsb-helper")
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Focus {
    #[default]
    Files,
    Sessions,
}

/// Where the cursor of a list goes after the list changed.
fn restored(keys: &[String], moved: bool, key: &Option<String>, sel: usize) -> usize {
    if !moved {
        return 0;
    }
    key.as_ref()
        .and_then(|k| keys.iter().position(|x| x == k))
        .unwrap_or_else(|| sel.min(keys.len().saturating_sub(1)))
}

fn git_ctx(kind: &str, cwd: &str) -> BTreeMap<String, String> {
    BTreeMap::from([("git".into(), kind.into()), ("cwd".into(), cwd.into())])
}

impl Panel {
    pub fn load(&mut self, shared: &Shared) {
        self.target = shared.cfg("target_pane").and_then(|s| s.parse().ok());
        self.cwd = shared.cfg("target_cwd").map(String::from);
        self.home = shared.cfg("home").map(PathBuf::from);
        self.helper = shared.cfg("helper").unwrap_or("").to_string();
    }

    fn on_granted(&mut self) {
        let env = get_session_environment_variables();
        if self.home.is_none() {
            self.home = env.get("USERPROFILE").or_else(|| env.get("HOME")).map(PathBuf::from);
        }
        if self.helper.is_empty() {
            self.helper = default_helper(&env);
        }
        match &self.home {
            Some(h) => change_host_folder(h.clone()),
            None => self.status = Some("cannot find home dir (set `home` in config)".into()),
        }
        self.refresh_git();
        set_timeout(TICK_SECS);
    }

    fn refresh_git(&mut self) {
        let Some(cwd) = self.cwd.clone() else { return };
        let dir = PathBuf::from(&cwd);
        let env = BTreeMap::new();
        let runs: [(&str, &[&str]); 4] = [
            ("status", &["git", "--no-optional-locks", "-c", "core.quotepath=false", "status", "--porcelain=v1", "-b", "-uall"]),
            ("root", &["git", "rev-parse", "--show-toplevel"]),
            ("unstaged", &["git", "--no-optional-locks", "-c", "core.quotepath=false", "diff", "--numstat"]),
            ("staged", &["git", "--no-optional-locks", "-c", "core.quotepath=false", "diff", "--cached", "--numstat"]),
        ];
        for (kind, cmd) in runs {
            run_command_with_env_variables_and_cwd(cmd, env.clone(), dir.clone(), git_ctx(kind, &cwd));
        }
    }

    fn refresh_sessions(&mut self) {
        let Some(cwd) = self.cwd.clone() else { return };
        let ctx = BTreeMap::from([("codex".into(), "1".into()), ("cwd".into(), cwd.clone())]);
        run_command(&[self.helper.as_str(), "codex", cwd.as_str()], ctx);
        if self.host_ready {
            self.claude = self.scanner.scan_claude(&cwd);
            self.merge_sessions();
        }
    }

    fn merge_sessions(&mut self) {
        let mut all: Vec<Session> = self.claude.iter().chain(&self.codex).cloned().collect();
        all.sort_by(|a, b| b.updated.cmp(&a.updated));
        all.truncate(MAX_SESSIONS);
        self.sessions = all;
        self.restore_sel();
    }

    fn len(&self, f: Focus) -> usize {
        match f {
            Focus::Files => self.files.len(),
            Focus::Sessions => self.sessions.len(),
        }
    }

    fn sel(&self, f: Focus) -> usize {
        match f {
            Focus::Files => self.fsel,
            Focus::Sessions => self.ssel,
        }
    }

    fn key_of(&self, f: Focus, i: usize) -> Option<String> {
        match f {
            Focus::Files => self.files.get(i).map(|x| x.path.clone()),
            Focus::Sessions => self.sessions.get(i).map(|x| x.id.clone()),
        }
    }

    /// The user put the cursor on row `i` of list `f`.
    fn select(&mut self, f: Focus, i: usize) {
        let i = i.min(self.len(f).saturating_sub(1));
        self.focus = f;
        self.user_focus = true;
        let key = self.key_of(f, i);
        match f {
            Focus::Files => (self.fsel, self.fmoved, self.fkey) = (i, true, key),
            Focus::Sessions => (self.ssel, self.smoved, self.skey) = (i, true, key),
        }
    }

    /// The lists changed under us: keep each cursor on the same file/session if the user moved it.
    fn restore_sel(&mut self) {
        let fkeys: Vec<String> = self.files.iter().map(|f| f.path.clone()).collect();
        let skeys: Vec<String> = self.sessions.iter().map(|s| s.id.clone()).collect();
        self.fsel = restored(&fkeys, self.fmoved, &self.fkey, self.fsel);
        self.ssel = restored(&skeys, self.smoved, &self.skey, self.ssel);
        self.fkey = self.key_of(Focus::Files, self.fsel);
        self.skey = self.key_of(Focus::Sessions, self.ssel);
        if !self.user_focus || self.len(self.focus) == 0 {
            self.focus = if !self.files.is_empty() || self.sessions.is_empty() { Focus::Files } else { Focus::Sessions };
        }
    }

    fn sync_files(&mut self) {
        self.files = match self.git.state() {
            GitState::Repo(info) => info.files,
            _ => Vec::new(),
        };
        self.restore_sel();
    }

    /// Follow the focused terminal pane of this tab (ignoring the panel itself).
    fn track_target(&mut self, shared: &Shared) {
        let Some(pos) = shared.my_tab() else { return };
        if let Some(panes) = shared.panes.panes.get(&pos) {
            if let Some(p) = panes
                .iter()
                .find(|p| !p.is_plugin && p.is_focused && !p.is_floating && !p.is_suppressed) {
                self.target = Some(p.id);
            }
        }
    }

    fn refresh_cwd(&mut self, shared: &mut Shared) -> bool {
        let Some(t) = self.target else { return false };
        let Some(cwd) = shared.cwd_of(t, true) else { return false };
        let cwd = cwd.to_string_lossy().into_owned();
        if self.cwd.as_deref() == Some(cwd.as_str()) {
            return false;
        }
        self.cwd = Some(cwd);
        self.git = GitRaw::default();
        self.files.clear();
        self.root = None;
        self.sessions.clear();
        self.claude.clear();
        self.codex.clear();
        (self.fsel, self.ssel, self.fscroll, self.sscroll) = (0, 0, 0, 0);
        (self.fmoved, self.smoved, self.user_focus) = (false, false, false);
        (self.fkey, self.skey) = (None, None);
        self.focus = Focus::Files;
        self.refresh_git();
        self.refresh_sessions();
        true
    }

    fn handle_pending(&mut self, shared: &Shared) {
        if let Some((tab_id, cmd, deadline)) = self.pending_tab.take() {
            let pane = shared
                .tabs
                .iter()
                .find(|t| t.tab_id == tab_id)
                .and_then(|t| shared.main_pane(t.position))
                .map(|p| p.id);
            match pane {
                Some(p) => self.pending_write = Some((p, cmd, self.tick + 10)),
                None if self.tick < deadline => self.pending_tab = Some((tab_id, cmd, deadline)),
                None => self.status = Some("new tab did not appear".into()),
            }
        }
        if let Some((pane, cmd, deadline)) = self.pending_write.take() {
            // Type once the shell has drawn its prompt (cursor moved off column 0);
            // keys sent while it is still starting up can be dropped.
            let ready = shared
                .panes
                .panes
                .values()
                .flatten()
                .find(|p| !p.is_plugin && p.id == pane)
                .and_then(|p| p.cursor_coordinates_in_pane)
                .is_some_and(|(x, _)| x > 0);
            if ready || self.tick >= deadline {
                write_chars_to_pane_id(&format!("{cmd}\r"), PaneId::Terminal(pane));
                self.status = Some("opened in a new tab".into());
            } else {
                self.pending_write = Some((pane, cmd, deadline));
            }
        }
    }

    fn target_is_idle(&self) -> bool {
        let Some(t) = self.target else { return false };
        match get_pane_running_command(PaneId::Terminal(t)) {
            Ok(argv) => argv.first().is_none_or(|a| ui::is_shell(a)),
            Err(_) => false,
        }
    }

    /// Ask the helper which panes run which agent sessions. With `open`, the answer
    /// decides how to open that session (see the `RunCommandResult` handler).
    fn refresh_running(&mut self, shared: &Shared, open: Option<&str>) {
        let live: Vec<u32> = shared.panes.panes.values().flatten().filter(|p| !p.is_plugin).map(|p| p.id).collect();
        self.pane_pids.retain(|pane, _| live.contains(pane));
        let mut pids = Vec::new();
        for p in shared.panes.panes.values().flatten().filter(|p| !p.is_plugin && !p.exited) {
            let pid = match self.pane_pids.get(&p.id) {
                Some(pid) => Some(*pid),
                None => get_pane_pid(PaneId::Terminal(p.id)).ok().and_then(|pid| u32::try_from(pid).ok()),
            };
            if let Some(pid) = pid {
                self.pane_pids.insert(p.id, pid);
                pids.push(pid.to_string());
            }
        }
        let mut ctx = BTreeMap::from([("running".to_string(), "1".to_string())]);
        if let Some(cwd) = &self.cwd {
            ctx.insert("cwd".into(), cwd.clone());
        }
        if let Some(id) = open {
            ctx.insert("open".into(), id.to_string());
        }
        let mut cmd = vec![self.helper.as_str(), "running"];
        cmd.extend(pids.iter().map(String::as_str));
        run_command(&cmd, ctx);
    }

    fn on_running(&mut self, stdout: &str) {
        let pane_of: HashMap<u32, u32> = self.pane_pids.iter().map(|(pane, pid)| (*pid, *pane)).collect();
        self.running = stdout
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter_map(|v| {
                let root = u32::try_from(v.get("root")?.as_u64()?).ok()?;
                Some((v.get("id")?.as_str()?.to_string(), *pane_of.get(&root)?))
            })
            .collect();
    }

    fn open_selected(&mut self, shared: &Shared) {
        match self.focus {
            Focus::Files => self.open_diff(shared, self.fsel),
            Focus::Sessions => self.open_session(shared, self.ssel),
        }
    }

    /// Enter on a session: jump to it if some pane already runs it, else launch it.
    fn open_session(&mut self, shared: &Shared, i: usize) {
        let Some(s) = self.sessions.get(i) else { return };
        let id = s.id.clone();
        self.status = Some("…".into());
        self.refresh_running(shared, Some(&id));
    }

    /// Enter on a file: a floating plugin pane (mode "diff") with a side-by-side diff.
    fn open_diff(&mut self, shared: &Shared, i: usize) {
        let Some(f) = self.files.get(i).cloned() else { return };
        let Some(cwd) = self.root.clone().or_else(|| self.cwd.clone()) else { return };
        let Some(url) = shared.my_pane().and_then(|p| p.plugin_url.clone()) else {
            self.status = Some("cannot find the plugin location".into());
            return;
        };
        let config = BTreeMap::from([
            ("mode".to_string(), "diff".to_string()),
            ("diff_cwd".to_string(), cwd),
            ("diff_path".to_string(), f.path.clone()),
            ("diff_code".to_string(), f.code.clone()),
            // the whole list, so the overlay can move on to the next/previous file
            ("diff_index".to_string(), i.to_string()),
            (
                "diff_files".to_string(),
                self.files.iter().map(|x| format!("{}\t{}", x.code, x.path)).collect::<Vec<_>>().join("\n"),
            ),
            // unique per request, so every Enter opens a fresh overlay
            ("n".to_string(), self.tick.to_string()),
        ]);
        let mut msg = MessageToPlugin::new("open")
            .with_plugin_url(url)
            .with_plugin_config(config)
            .new_plugin_instance_should_float(true)
            .new_plugin_instance_should_have_pane_title(f.path)
            .new_plugin_instance_should_be_focused();
        let coords = FloatingPaneCoordinates::new(
            Some("3%".into()),
            Some("3%".into()),
            Some("94%".into()),
            Some("92%".into()),
            None,
            None,
        );
        if let Some(c) = coords {
            msg = msg.with_floating_pane_coordinates(c);
        }
        pipe_message_to_plugin(msg);
    }

    fn launch(&mut self, shared: &Shared, s: Session) {
        let cmd = match s.kind {
            Kind::Claude => format!("{} --resume {}", shared.cfg("claude_cmd").unwrap_or("claude"), s.id),
            Kind::Codex => format!("{} resume {}", shared.cfg("codex_cmd").unwrap_or("codex"), s.id),
        };
        match self.target {
            Some(t) if self.target_is_idle() => {
                write_chars_to_pane_id(&format!("{cmd}\r"), PaneId::Terminal(t));
                focus_pane_with_id(PaneId::Terminal(t), false, false);
                self.status = Some("resumed in this tab".into());
            }
            _ => match new_tab(None, self.cwd.clone()) {
                Some(id) => {
                    self.pending_tab = Some((id, cmd, self.tick + 10));
                    self.status = Some("tab busy → opening a new tab…".into());
                }
                None => self.status = Some("failed to open a new tab".into()),
            },
        }
    }

    /// Move within the focused list only; stops at the first/last row.
    fn move_sel(&mut self, delta: isize) {
        let n = self.len(self.focus);
        if n == 0 {
            return;
        }
        let to = (self.sel(self.focus) as isize + delta).clamp(0, n as isize - 1) as usize;
        self.select(self.focus, to);
    }

    pub fn update(&mut self, shared: &mut Shared, event: &Event) -> bool {
        match event {
            Event::PermissionRequestResult(PermissionStatus::Granted) => {
                self.on_granted();
                true
            }
            Event::HostFolderChanged(_) => {
                self.host_ready = true;
                self.refresh_sessions();
                self.refresh_running(shared, None);
                true
            }
            Event::FailedToChangeHostFolder(e) => {
                self.status = Some(format!("home not readable: {}", e.clone().unwrap_or_default()));
                true
            }
            Event::TabUpdate(_) | Event::PaneUpdate(_) => {
                if !self.focused_once && shared.my_pane().is_some_and(|p| !p.is_suppressed) {
                    self.focused_once = true;
                    focus_pane_with_id(PaneId::Plugin(shared.my_id), false, false);
                }
                self.track_target(shared);
                self.handle_pending(shared);
                true
            }
            Event::Timer(_) => {
                self.tick += 1;
                self.handle_pending(shared);
                let changed = self.tick % 2 == 0 && self.refresh_cwd(shared);
                if !changed {
                    if self.tick % 3 == 0 {
                        self.refresh_git();
                    }
                    if self.tick % 5 == 0 {
                        self.refresh_sessions();
                        self.refresh_running(shared, None);
                    }
                }
                set_timeout(TICK_SECS);
                true
            }
            Event::RunCommandResult(code, stdout, stderr, ctx) => {
                if ctx.get("cwd") != self.cwd.as_ref() {
                    return false;
                }
                let out = String::from_utf8_lossy(stdout).into_owned();
                let ok = *code == Some(0);
                if ctx.contains_key("running") {
                    if ok {
                        self.on_running(&out);
                    }
                    if let Some(id) = ctx.get("open") {
                        match self.running.get(id) {
                            Some(&pane) => {
                                focus_pane_with_id(PaneId::Terminal(pane), false, false);
                                self.status = Some("already open → switched to it".into());
                            }
                            None => {
                                if let Some(s) = self.sessions.iter().find(|s| &s.id == id).cloned() {
                                    self.launch(shared, s);
                                }
                            }
                        }
                    }
                    return true;
                }
                if ctx.contains_key("codex") {
                    if ok {
                        self.codex = sessions::parse_codex(&out);
                        self.merge_sessions();
                    } else {
                        let err = String::from_utf8_lossy(stderr);
                        self.status = Some(format!("codex: {}", err.trim()));
                    }
                    return true;
                }
                match ctx.get("git").map(String::as_str) {
                    Some("status") => {
                        self.git.status = Some(if ok { Ok(out) } else { Err(String::from_utf8_lossy(stderr).into_owned()) })
                    }
                    Some("unstaged") => self.git.unstaged = ok.then_some(out),
                    Some("staged") => self.git.staged = ok.then_some(out),
                    Some("root") => {
                        self.root = ok.then(|| out.trim().to_string()).filter(|r| !r.is_empty());
                        return false;
                    }
                    _ => return false,
                }
                self.sync_files();
                true
            }
            Event::Key(key) if key.has_no_modifiers() => {
                match key.bare_key {
                    BareKey::Down | BareKey::Char('j') => self.move_sel(1),
                    BareKey::Up | BareKey::Char('k') => self.move_sel(-1),
                    BareKey::PageDown => self.move_sel(10),
                    BareKey::PageUp => self.move_sel(-10),
                    BareKey::Home | BareKey::Char('g') => self.select(self.focus, 0),
                    BareKey::Tab => {
                        // switch between the file list and the session list
                        let other = if self.focus == Focus::Files { Focus::Sessions } else { Focus::Files };
                        if self.len(other) > 0 {
                            self.select(other, self.sel(other));
                        }
                    }
                    BareKey::End | BareKey::Char('G') => self.move_sel(isize::MAX / 2),
                    BareKey::Enter => self.open_selected(shared),
                    BareKey::Char('r') => {
                        self.refresh_git();
                        self.refresh_sessions();
                    }
                    BareKey::Esc | BareKey::Char('q') => close_self(),
                    _ => return false,
                }
                true
            }
            Event::Key(key) if key.bare_key == BareKey::Char('G') => {
                self.move_sel(isize::MAX / 2);
                true
            }
            Event::Mouse(Mouse::LeftClick(line, _)) => {
                let Some(Some((f, i))) = usize::try_from(*line).ok().and_then(|l| self.clicks.get(l)).copied() else {
                    return false;
                };
                if f == self.focus && i == self.sel(f) {
                    self.open_selected(shared);
                } else {
                    self.select(f, i);
                }
                true
            }
            Event::Mouse(Mouse::ScrollUp(_)) => {
                self.move_sel(-1);
                true
            }
            Event::Mouse(Mouse::ScrollDown(_)) => {
                self.move_sel(1);
                true
            }
            _ => false,
        }
    }

    /// Nudge our own width toward `panel_width` columns.
    fn fit_width(&mut self, shared: &Shared, cols: usize) {
        // Renders keep coming while a resize is in flight; wait until it lands.
        if self.resize_from == Some(cols) || self.resize_steps >= 12 {
            return;
        }
        let want = shared.cfg("panel_width").and_then(|w| w.parse().ok()).unwrap_or(DEFAULT_WIDTH);
        let step = if cols > want + 4 {
            Resize::Decrease
        } else if cols + 4 < want {
            Resize::Increase
        } else {
            self.resize_steps = u32::MAX;
            return;
        };
        // Steps are coarse: after overshooting to narrow, step back once and stop.
        match (self.last_resize, step) {
            (Some(Resize::Increase), Resize::Decrease) => return,
            (Some(Resize::Decrease), Resize::Increase) => self.resize_steps = u32::MAX - 1,
            _ => {}
        }
        self.resize_steps += 1;
        self.last_resize = Some(step);
        self.resize_from = Some(cols);
        resize_pane_with_id(ResizeStrategy::new(step, Some(Direction::Left)), PaneId::Plugin(shared.my_id));
    }

    pub fn render(&mut self, shared: &mut Shared, rows: usize, cols: usize) {
        if shared.permitted {
            self.fit_width(shared, cols);
        }
        let cols = cols.saturating_sub(1);

        // -- pinned header: directory, branch, summary (with the file counter) -------
        let mut head: Vec<Line> = Vec::new();
        let cwd = self.cwd.clone().unwrap_or_default();
        head.push(note(&format!("{BOLD}{BLUE}"), &format!(" {}", ui::truncate(ui::basename(&cwd), cols.saturating_sub(2)))));
        match self.git.state() {
            GitState::Loading => head.push(note(DIM, "  …")),
            GitState::NotRepo => head.push(note(DIM, "  not a git repository")),
            GitState::Repo(info) => {
                let mut l = Line::new();
                l.push(MAGENTA, &format!("  {}", ui::truncate(&info.branch, cols.saturating_sub(14))));
                if info.ahead > 0 {
                    l.push(GREEN, &format!(" ↑{}", info.ahead));
                }
                if info.behind > 0 {
                    l.push(RED, &format!(" ↓{}", info.behind));
                }
                head.push(l);
                let mut l = Line::new();
                if info.files.is_empty() {
                    l.push(DIM, "  clean");
                } else {
                    l.push("", &format!("  {} changed ", info.files.len()));
                    l.push(GREEN, &format!("+{}", info.added));
                    l.push("", " ");
                    l.push(RED, &format!("-{}", info.deleted));
                    l.right(DIM, &format!("{}/{} ", self.fsel + 1, self.files.len()), cols);
                }
                head.push(l);
            }
        }

        // -- split the remaining rows between the two lists -------------------------
        let status_rows = usize::from(self.status.is_some());
        let body = rows.saturating_sub(head.len() + status_rows).max(1);
        let avail = body.saturating_sub(2); // separator + SESSIONS header
        let (want_f, want_s) = (self.files.len(), self.sessions.len().max(1));
        let f_h = if want_f + want_s <= avail {
            want_f
        } else {
            let half = avail / 2;
            if want_f <= half {
                want_f
            } else if want_s <= avail - half {
                avail - want_s
            } else {
                half
            }
        };
        let s_h = avail.saturating_sub(f_h);

        // each list keeps its own cursor in view; nothing else scrolls it
        fn keep_visible(sel: usize, scroll: &mut usize, h: usize, n: usize) {
            if sel < *scroll {
                *scroll = sel;
            } else if h > 0 && sel >= *scroll + h {
                *scroll = sel + 1 - h;
            }
            *scroll = (*scroll).min(n.saturating_sub(h));
        }
        keep_visible(self.fsel, &mut self.fscroll, f_h, self.files.len());
        keep_visible(self.ssel, &mut self.sscroll, s_h, self.sessions.len());

        let mut lines = head;
        let mut clicks: Vec<Option<(Focus, usize)>> = vec![None; lines.len()];
        let marker = |selected: bool, focused: bool| match (selected, focused) {
            (true, true) => (MAGENTA, " ▌"),
            (true, false) => (DIM, " ▎"),
            _ => ("", "  "),
        };

        // -- files -----------------------------------------------------------------
        for (i, f) in self.files.iter().enumerate().skip(self.fscroll).take(f_h) {
            let sel = i == self.fsel;
            let focused = self.focus == Focus::Files;
            let code_style = match f.code.chars().next() {
                Some('A') => GREEN,
                Some('D') => RED,
                Some('R') | Some('C') => BLUE,
                Some('?') => CYAN,
                _ => YELLOW,
            };
            let stats = match f.stats {
                Some((a, d)) => format!("+{a} -{d} "),
                None if f.code == "??" => "new ".into(),
                None => String::new(),
            };
            let (mstyle, mtext) = marker(sel, focused);
            let mut l = Line::new();
            l.push(mstyle, mtext);
            l.push(code_style, &format!("{:<2} ", f.code));
            l.push(if sel && focused { BOLD } else { "" }, &ui::truncate_left(&f.path, cols.saturating_sub(6 + stats.len() + 1)));
            l.right(DIM, &stats, cols);
            lines.push(l);
            clicks.push(Some((Focus::Files, i)));
        }

        // -- sessions --------------------------------------------------------------
        let mut sep = Line::new();
        sep.push(DIM, &"─".repeat(cols.saturating_sub(1)));
        lines.push(sep);
        clicks.push(None);
        let mut h = Line::new();
        h.push(BOLD, &format!(" SESSIONS {}", self.sessions.len()));
        if !self.sessions.is_empty() {
            h.right(DIM, &format!("{}/{} ", self.ssel + 1, self.sessions.len()), cols);
        }
        lines.push(h);
        clicks.push(None);
        if self.sessions.is_empty() && s_h > 0 {
            lines.push(note(DIM, if self.host_ready { "  no sessions for this directory" } else { "  …" }));
            clicks.push(None);
        }
        let now = now_secs();
        for (i, s) in self.sessions.iter().enumerate().skip(self.sscroll).take(s_h) {
            let sel = i == self.ssel;
            let focused = self.focus == Focus::Sessions;
            let (tag, tag_style) = match s.kind {
                Kind::Claude => ("CC", YELLOW),
                Kind::Codex => ("CX", CYAN),
            };
            let age = format!(" {} ", ui::age(now.saturating_sub(s.updated)));
            let (mstyle, mtext) = marker(sel, focused);
            let mut l = Line::new();
            l.push(mstyle, mtext);
            l.push(tag_style, tag);
            if self.running.contains_key(&s.id) {
                l.push(GREEN, "●");
            } else {
                l.push("", " ");
            }
            l.push("", " ");
            l.push(if sel && focused { BOLD } else { "" }, &ui::truncate(&s.title, cols.saturating_sub(7 + age.len())));
            l.right(DIM, &age, cols);
            lines.push(l);
            clicks.push(Some((Focus::Sessions, i)));
        }
        if let Some(st) = &self.status {
            while lines.len() + 1 < rows {
                lines.push(Line::new());
                clicks.push(None);
            }
            lines.push(note(DIM, &format!(" {}", ui::truncate(st, cols.saturating_sub(2)))));
        }
        self.clicks = clicks;
        ui::print_lines(&lines, rows, cols);
    }
}

fn note(style: &str, text: &str) -> Line {
    let mut l = Line::new();
    l.push(style, text);
    l
}
