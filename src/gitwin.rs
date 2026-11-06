//! Floating Git Log window (`mode "log"`), after IntelliJ IDEA's Log tab: the commit
//! graph with hash, refs, subject, author and date on the left; the changed files and
//! the details of the selected commit on the right. Right-click (or `m`) a commit or a
//! file for its actions, `b` (or click the branch) for the branch popup.

use std::collections::{BTreeMap, HashMap};

use zellij_tile::prelude::*;

use crate::gitctl::{run_git, short, Act, GitCtl, HostAct, Job, Out};
use crate::gitlog::{self, ChangedFile, Commit, RefKind};
use crate::ui::{self, Line, BLUE, BOLD, CYAN, DIM, GREEN, MAGENTA, RED, REVERSE, YELLOW};
use crate::widgets::{plain, Item, Menu};
use crate::Shared;

const LANE_COLORS: [&str; 6] = [BLUE, MAGENTA, CYAN, GREEN, YELLOW, RED];
const PAGE: usize = 1000;
const MAX_GRAPH_COLS: usize = 24;

#[derive(Clone, PartialEq, Eq, Default)]
enum Scope {
    /// all branches, remotes and tags
    #[default]
    All,
    /// the current branch only
    Head,
    /// one ref or range (`main`, `main..feature`)
    Ref(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Pane {
    #[default]
    Log,
    Files,
}

enum Files {
    Loading,
    Ready(Vec<ChangedFile>),
    Failed(String),
}

#[derive(Clone, PartialEq, Eq)]
enum Zone {
    Commit(usize),
    File(usize),
    Branch,
    Scope,
    Filter,
    Continue,
    Abort,
}

#[derive(Default)]
pub struct GitWin {
    root: String,
    url: Option<String>,
    ctl: GitCtl,
    commits: Vec<Commit>,
    /// per commit: its graph row and the connector row below it
    graph: Vec<gitlog::GraphRow>,
    loading: bool,
    error: Option<String>,
    limit: usize,
    generation: u64,
    scope: Scope,
    /// file history: only commits touching this path
    path: Option<String>,
    filter: String,
    typing: bool,
    /// indexes into `commits` that pass the filter
    shown: Vec<usize>,
    sel: usize,
    scroll: usize,
    /// sha of the selected commit, to find it again after a reload
    sel_sha: Option<String>,
    focus: Pane,
    fsel: usize,
    fscroll: usize,
    files: HashMap<String, Files>,
    zones: Vec<(usize, usize, usize, Zone)>,
    last_click: Option<(Zone, u128)>,
    body_h: usize,
    /// content width of the last render
    width: usize,
    n: u64,
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

impl GitWin {
    pub fn load(&mut self, shared: &Shared) {
        self.root = shared.cfg("log_cwd").unwrap_or("").to_string();
        self.loading = true;
        self.url = shared.cfg("plugin_url").map(String::from);
        self.scope = match shared.cfg("log_range") {
            Some(r) => Scope::Ref(r.to_string()),
            None => Scope::All,
        };
        self.limit = PAGE;
        self.ctl.set_cwd(&self.root);
    }

    fn reload(&mut self) {
        self.generation += 1;
        self.loading = true;
        let limit = self.limit.to_string();
        let mut a: Vec<&str> = vec!["log", "--date-order", "-n", &limit, gitlog::LOG_DATE, gitlog::LOG_FORMAT];
        match &self.scope {
            Scope::All => a.extend(["--branches", "--remotes", "--tags", "HEAD"]),
            Scope::Head => a.push("HEAD"),
            Scope::Ref(r) => a.push(r),
        }
        if let Some(p) = &self.path {
            a.extend(["--", p]);
        }
        let ctx = BTreeMap::from([("log".to_string(), self.generation.to_string())]);
        run_git(&self.root, &a, ctx);
    }

    fn refilter(&mut self) {
        let f = self.filter.to_lowercase();
        self.shown = if f.is_empty() {
            (0..self.commits.len()).collect()
        } else {
            (0..self.commits.len())
                .filter(|&i| {
                    let c = &self.commits[i];
                    c.subject.to_lowercase().contains(&f)
                        || c.author.to_lowercase().contains(&f)
                        || c.sha.starts_with(&f)
                        || c.decorations.iter().any(|d| d.to_lowercase().contains(&f))
                })
                .collect()
        };
        self.sel = self
            .sel_sha
            .as_ref()
            .and_then(|s| self.shown.iter().position(|&i| self.commits[i].sha == *s))
            .unwrap_or(0);
        self.after_move();
    }

    fn selected(&self) -> Option<&Commit> {
        self.shown.get(self.sel).map(|&i| &self.commits[i])
    }

    fn sel_files(&self) -> &[ChangedFile] {
        match self.selected().and_then(|c| self.files.get(&c.sha)) {
            Some(Files::Ready(v)) => v,
            _ => &[],
        }
    }

    /// The selection moved: load its files, page in more history near the end.
    fn after_move(&mut self) {
        self.sel_sha = self.selected().map(|c| c.sha.clone());
        let Some(c) = self.selected().cloned() else { return };
        if !self.files.contains_key(&c.sha) {
            self.files.insert(c.sha.clone(), Files::Loading);
            let ctx = BTreeMap::from([("files".to_string(), c.sha.clone())]);
            match c.parents.first() {
                Some(p) => run_git(&self.root, &["diff", "--raw", "--numstat", "-z", "-M", p, &c.sha], ctx),
                None => run_git(&self.root, &["show", "--format=", "--raw", "--numstat", "-z", "-M", &c.sha], ctx),
            }
        }
        if !self.loading && self.filter.is_empty() && self.commits.len() >= self.limit && self.sel + 50 >= self.shown.len() {
            self.limit += PAGE;
            self.reload();
        }
    }

    fn move_sel(&mut self, delta: isize) {
        match self.focus {
            Pane::Log => {
                if self.shown.is_empty() {
                    return;
                }
                let to = (self.sel as isize + delta).clamp(0, self.shown.len() as isize - 1) as usize;
                if to != self.sel {
                    self.sel = to;
                    self.fsel = 0;
                    self.fscroll = 0;
                    self.after_move();
                }
            }
            Pane::Files => {
                let n = self.sel_files().len();
                if n > 0 {
                    self.fsel = (self.fsel as isize + delta).clamp(0, n as isize - 1) as usize;
                }
            }
        }
    }

    // -- menus -------------------------------------------------------------------

    fn commit_menu(&mut self, x: usize, y: usize) {
        let Some(c) = self.selected().cloned() else { return };
        let is_head = self.ctl.head.as_deref() == Some(c.sha.as_str());
        let merge = c.parents.len() > 1;
        let has_upstream = self.ctl.current().is_some_and(|b| b.upstream.is_some());
        let cur = self.ctl.current_name();
        let mut items = vec![
            Item::new("Show Changed Files", Act::Host(HostAct::FocusFiles)).key('f').enabled(!self.sel_files().is_empty()),
            Item::sep(),
            Item::new("Copy Revision Number", Act::CopyText(c.sha.clone())).key('y'),
            Item::new("Copy Subject", Act::CopyText(c.subject.clone())),
            Item::sep(),
            Item::new("Checkout Revision", Act::CheckoutCommit(c.sha.clone())).key('o'),
            Item::new("New Branch…", Act::BranchAt(c.sha.clone())).key('n'),
            Item::new("New Tag…", Act::TagAt(c.sha.clone())).key('t'),
            Item::sep(),
            Item::new("Cherry-Pick", Act::CherryPick { sha: c.sha.clone(), merge }).key('p').enabled(!is_head),
            Item::new("Revert Commit", Act::Revert { sha: c.sha.clone(), merge }).key('v'),
            Item::new("Reset Current Branch to Here", Act::ResetMenu(c.sha.clone())).key('r').sub(),
            Item::sep(),
            Item::new("Undo Commit", Act::UndoCommit).key('u').enabled(is_head && c.parents.len() == 1),
            Item::new("Edit Commit Message…", Act::EditMessage { subject: c.subject.clone(), body: c.body.clone() }).key('e').enabled(is_head),
            Item::new("Interactively Rebase from Here…", Act::InteractiveRebase(c.parents.first().cloned()))
                .key('i')
                .enabled(!merge && self.ctl.current().is_some()),
            Item::new(format!("Push All up to Here ({})", ui::truncate(&cur, 16)), Act::PushUpTo(c.sha.clone())).enabled(has_upstream),
        ];
        let locals = self.ctl.locals();
        let branches: Vec<String> = gitlog::classify(&c.decorations, &locals)
            .into_iter()
            .filter(|r| matches!(r.kind, RefKind::Local | RefKind::Remote | RefKind::Current))
            .map(|r| r.name)
            .collect();
        if !branches.is_empty() {
            items.push(Item::sep());
            for b in branches {
                items.push(Item::new(format!("Branch '{}'", ui::truncate(&b, 30)), Act::BranchMenu(b)).sub());
            }
        }
        let title = format!("{} {}", short(&c.sha), ui::truncate(&c.subject, 30));
        self.ctl.open_menu(Menu::new(title, items).at(x, y));
    }

    fn file_menu(&mut self, x: usize, y: usize) {
        let Some(c) = self.selected().cloned() else { return };
        let Some(f) = self.sel_files().get(self.fsel).cloned() else { return };
        let i = self.fsel;
        let mut get = vec!["checkout", c.sha.as_str(), "--", f.path.as_str()];
        if f.code == "D" {
            // the file is gone in this commit: take it from the parent
            get[1] = c.parents.first().map(String::as_str).unwrap_or(c.sha.as_str());
        }
        let items = vec![
            Item::new("Show Diff", Act::Host(HostAct::DiffFile(i))).key('d'),
            Item::new("Compare with Local", Act::Host(HostAct::CompareWithLocal(i))).key('l').enabled(f.code != "D"),
            Item::new("Show History", Act::Host(HostAct::History(i))).key('h'),
            Item::sep(),
            Item::new(
                "Get from Revision",
                Act::Confirm {
                    title: "Get from Revision".into(),
                    message: format!("Replace '{}' in the working tree with its version at {}?", f.path, short(&c.sha)),
                    yes: "Get".into(),
                    job: Job::one(format!("get {} from {}", ui::basename(&f.path), short(&c.sha)), &get),
                },
            )
            .key('g'),
            Item::new("Copy Path", Act::CopyText(f.path.clone())).key('c'),
        ];
        self.ctl.open_menu(Menu::new(ui::truncate(ui::basename(&f.path), 40), items).at(x, y));
    }

    fn host(&mut self, h: HostAct) {
        match h {
            HostAct::ShowLog(r) => self.set_scope(Scope::Ref(r)),
            HostAct::OpenLog(r) => self.set_scope(r.map(Scope::Ref).unwrap_or(Scope::All)),
            HostAct::FocusFiles => {
                if !self.sel_files().is_empty() {
                    self.focus = Pane::Files;
                }
            }
            HostAct::DiffFile(i) => self.open_diff(i, false),
            HostAct::CompareWithLocal(i) => self.open_diff(i, true),
            HostAct::History(i) => {
                if let Some(f) = self.sel_files().get(i) {
                    self.path = Some(f.path.clone());
                    self.focus = Pane::Log;
                    self.reload();
                }
            }
        }
    }

    fn set_scope(&mut self, s: Scope) {
        self.scope = s;
        self.focus = Pane::Log;
        self.limit = PAGE;
        self.reload();
    }

    /// A side-by-side diff of file `i` of the selected commit (or against the work tree).
    fn open_diff(&mut self, i: usize, with_local: bool) {
        let Some(c) = self.selected().cloned() else { return };
        let files = self.sel_files().to_vec();
        let Some(f) = files.get(i) else { return };
        let Some(url) = self.url.clone() else {
            self.ctl.status = Some(("cannot find the plugin location".into(), true));
            return;
        };
        self.n += 1;
        let mut config = BTreeMap::from([
            ("mode".to_string(), "diff".to_string()),
            ("diff_cwd".to_string(), self.root.clone()),
            ("diff_path".to_string(), f.path.clone()),
            ("diff_code".to_string(), f.code.clone()),
            ("n".to_string(), format!("log{}-{}", now_ms(), self.n)),
        ]);
        if with_local {
            config.insert("diff_base".into(), c.sha.clone());
            config.insert("diff_files".into(), format!("{}\t{}", f.code, f.path));
            config.insert("diff_index".into(), "0".into());
        } else {
            config.insert("diff_rev".into(), c.sha.clone());
            match c.parents.first() {
                Some(p) => config.insert("diff_base".into(), p.clone()),
                None => config.insert("diff_root".into(), "1".into()),
            };
            config.insert("diff_index".into(), i.to_string());
            let list: Vec<String> = files
                .iter()
                .map(|x| match &x.old {
                    Some(o) => format!("{}\t{}\t{}", x.code, x.path, o),
                    None => format!("{}\t{}", x.code, x.path),
                })
                .collect();
            config.insert("diff_files".into(), list.join("\n"));
        }
        let title = if with_local { format!("{} @ {} ↔ local", f.path, short(&c.sha)) } else { format!("{} @ {}", f.path, short(&c.sha)) };
        let mut msg = MessageToPlugin::new("open")
            .with_plugin_url(url)
            .with_plugin_config(config)
            .new_plugin_instance_should_float(true)
            .new_plugin_instance_should_have_pane_title(title)
            .new_plugin_instance_should_be_focused();
        if let Some(co) = FloatingPaneCoordinates::new(Some("4%".into()), Some("4%".into()), Some("92%".into()), Some("90%".into()), None, None) {
            msg = msg.with_floating_pane_coordinates(co);
        }
        pipe_message_to_plugin(msg);
    }

    /// Screen rows per commit: the commit and its connector row (none without the graph).
    fn per(&self) -> usize {
        if self.filter.is_empty() { 2 } else { 1 }
    }

    /// How many commits fit in the log.
    fn visible(&self) -> usize {
        (self.body_h / self.per()).max(1)
    }

    fn sel_row_y(&self) -> usize {
        2 + self.sel.saturating_sub(self.scroll) * self.per()
    }

    // -- events -----------------------------------------------------------------

    pub fn update(&mut self, _shared: &mut Shared, event: &Event) -> bool {
        let r = self.update_inner(event);
        if self.ctl.dirty {
            self.ctl.dirty = false;
            self.reload();
        }
        r
    }

    fn update_inner(&mut self, event: &Event) -> bool {
        match event {
            Event::PermissionRequestResult(PermissionStatus::Granted) => {
                // opened on any directory of a repo (e.g. from a keybinding): find its root
                // first, since every path git prints is relative to it
                let ctx = BTreeMap::from([("toplevel".to_string(), "1".to_string())]);
                run_git(&self.root, &["rev-parse", "--show-toplevel"], ctx);
                true
            }
            Event::RunCommandResult(code, stdout, stderr, ctx) => {
                if self.ctl.on_result(ctx, *code, stdout, stderr) {
                    return true;
                }
                if ctx.get("cwd") != Some(&self.root) {
                    return false;
                }
                if ctx.contains_key("toplevel") {
                    let top = String::from_utf8_lossy(stdout).trim().to_string();
                    if *code == Some(0) && !top.is_empty() {
                        self.root = top;
                        self.ctl.set_cwd(&self.root);
                        self.ctl.refresh();
                        self.reload();
                    } else {
                        self.loading = false;
                        self.error = Some(format!("not a git repository: {}", self.root));
                    }
                    return true;
                }
                if let Some(g) = ctx.get("log") {
                    if g.parse::<u64>().ok() != Some(self.generation) {
                        return false;
                    }
                    self.loading = false;
                    if *code == Some(0) {
                        self.error = None;
                        self.commits = gitlog::parse_log(&String::from_utf8_lossy(stdout));
                        self.graph = gitlog::graph(&self.commits);
                    } else {
                        self.commits.clear();
                        self.graph.clear();
                        let e = String::from_utf8_lossy(stderr);
                        self.error = Some(e.lines().next().unwrap_or("git log failed").to_string());
                    }
                    self.refilter();
                    return true;
                }
                if let Some(sha) = ctx.get("files") {
                    let v = if *code == Some(0) {
                        Files::Ready(gitlog::parse_changes(&String::from_utf8_lossy(stdout)))
                    } else {
                        Files::Failed(String::from_utf8_lossy(stderr).lines().next().unwrap_or("failed").to_string())
                    };
                    self.files.insert(sha.clone(), v);
                    return true;
                }
                false
            }
            Event::Key(key) => self.key(key),
            Event::Mouse(m) => self.mouse(m),
            _ => false,
        }
    }

    fn key(&mut self, key: &KeyWithModifier) -> bool {
        match self.ctl.key(key) {
            Out::Unhandled => {}
            Out::Handled => return true,
            Out::Host(h) => {
                self.host(h);
                return true;
            }
        }
        if self.typing {
            match key.bare_key {
                BareKey::Esc => {
                    self.typing = false;
                    self.filter.clear();
                    self.refilter();
                }
                BareKey::Enter => self.typing = false,
                BareKey::Backspace => {
                    self.filter.pop();
                    self.refilter();
                }
                BareKey::Down => self.move_sel(1),
                BareKey::Up => self.move_sel(-1),
                BareKey::Char(c) if plain(key) => {
                    self.filter.push(c);
                    self.refilter();
                }
                _ => {}
            }
            return true;
        }
        if !plain(key) {
            return false;
        }
        let page = match self.focus {
            Pane::Log => self.visible().max(2) as isize - 1,
            Pane::Files => 10,
        };
        match key.bare_key {
            BareKey::Down | BareKey::Char('j') => self.move_sel(1),
            BareKey::Up | BareKey::Char('k') => self.move_sel(-1),
            BareKey::PageDown | BareKey::Char(' ') => self.move_sel(page),
            BareKey::PageUp => self.move_sel(-page),
            BareKey::Home | BareKey::Char('g') => self.move_sel(-(isize::MAX / 2)),
            BareKey::End | BareKey::Char('G') => self.move_sel(isize::MAX / 2),
            BareKey::Tab => {
                self.focus = if self.focus == Pane::Log && !self.sel_files().is_empty() { Pane::Files } else { Pane::Log };
            }
            BareKey::Right | BareKey::Char('l') if self.focus == Pane::Log => {
                if !self.sel_files().is_empty() {
                    self.focus = Pane::Files;
                }
            }
            BareKey::Left | BareKey::Char('h') if self.focus == Pane::Files => self.focus = Pane::Log,
            BareKey::Enter if self.focus == Pane::Files => self.open_diff(self.fsel, false),
            BareKey::Char('d') if self.focus == Pane::Files => self.open_diff(self.fsel, false),
            // Enter (on a commit), m or . : the actions menu, as a right-click would
            BareKey::Enter | BareKey::Char('m') | BareKey::Char('.') => {
                let y = self.sel_row_y();
                match self.focus {
                    Pane::Log => self.commit_menu(8, y + 1),
                    Pane::Files => {
                        let x = self.split().0 + 4;
                        self.file_menu(x, 4 + self.fsel.saturating_sub(self.fscroll));
                    }
                }
            }
            BareKey::Char('y') => {
                let text = match self.focus {
                    Pane::Log => self.selected().map(|c| c.sha.clone()),
                    Pane::Files => self.sel_files().get(self.fsel).map(|f| f.path.clone()),
                };
                if let Some(t) = text {
                    self.ctl.perform(Act::CopyText(t));
                }
            }
            BareKey::Char('b') => self.ctl.open_picker(),
            BareKey::Char('c') => self.ctl.open_commit(),
            BareKey::Char('/') => {
                self.typing = true;
                self.focus = Pane::Log;
            }
            BareKey::Char('a') => {
                let next = if self.scope == Scope::All { Scope::Head } else { Scope::All };
                self.set_scope(next);
            }
            BareKey::Char('r') | BareKey::F(5) => {
                self.files.clear();
                self.ctl.refresh();
                self.reload();
            }
            BareKey::Char('f') => {
                self.ctl.perform(Act::Fetch);
            }
            BareKey::Esc | BareKey::Char('q') => {
                if self.focus == Pane::Files {
                    self.focus = Pane::Log;
                } else if !self.filter.is_empty() {
                    self.filter.clear();
                    self.refilter();
                } else if self.path.is_some() {
                    self.path = None;
                    self.reload();
                } else if matches!(self.scope, Scope::Ref(_)) {
                    self.set_scope(Scope::All);
                } else {
                    close_self();
                }
            }
            _ => return false,
        }
        true
    }

    fn zone_at(&self, line: usize, col: usize) -> Option<Zone> {
        self.zones.iter().find(|(l, x0, x1, _)| *l == line && col >= *x0 && col < *x1).map(|z| z.3.clone())
    }

    fn mouse(&mut self, m: &Mouse) -> bool {
        match *m {
            Mouse::LeftClick(line, col) | Mouse::RightClick(line, col) => {
                let right = matches!(m, Mouse::RightClick(..));
                let (line, col) = (line.max(0) as usize, col);
                if self.ctl.modal() {
                    match self.ctl.click(line, col) {
                        Out::Host(h) => self.host(h),
                        _ => {}
                    }
                    if !right || self.ctl.modal() {
                        return true;
                    }
                }
                let Some(z) = self.zone_at(line, col) else { return false };
                let double = !right
                    && self.last_click.as_ref().is_some_and(|(lz, t)| *lz == z && now_ms().saturating_sub(*t) < 450);
                self.last_click = Some((z.clone(), now_ms()));
                match z {
                    Zone::Commit(i) => {
                        self.focus = Pane::Log;
                        if i != self.sel {
                            self.sel = i;
                            self.fsel = 0;
                            self.fscroll = 0;
                            self.after_move();
                        }
                        if right {
                            self.commit_menu(col, line + 1);
                        } else if double && !self.sel_files().is_empty() {
                            self.focus = Pane::Files;
                        }
                    }
                    Zone::File(i) => {
                        let again = self.focus == Pane::Files && self.fsel == i;
                        self.focus = Pane::Files;
                        self.fsel = i;
                        if right {
                            self.file_menu(col, line + 1);
                        } else if again || double {
                            self.open_diff(i, false);
                        }
                    }
                    Zone::Branch => self.ctl.open_picker(),
                    Zone::Scope => {
                        let next = if self.scope == Scope::All { Scope::Head } else { Scope::All };
                        self.path = None;
                        self.set_scope(next);
                    }
                    Zone::Filter => {
                        self.typing = true;
                        self.focus = Pane::Log;
                    }
                    Zone::Continue => {
                        if let Some(op) = self.ctl.state {
                            self.ctl.perform(Act::Continue(op));
                        }
                    }
                    Zone::Abort => {
                        if let Some(op) = self.ctl.state {
                            self.ctl.perform(Act::Abort(op));
                        }
                    }
                }
                true
            }
            Mouse::Hover(line, col) => self.ctl.hover(line.max(0) as usize, col),
            Mouse::ScrollUp(_) => {
                if !self.ctl.scroll(-1) {
                    self.move_sel(-3);
                }
                true
            }
            Mouse::ScrollDown(_) => {
                if !self.ctl.scroll(1) {
                    self.move_sel(3);
                }
                true
            }
            _ => false,
        }
    }

    // -- rendering --------------------------------------------------------------

    /// (log width, files/details width); 0 for the second when they don't fit side by side.
    fn split(&self) -> (usize, usize) {
        let w = self.width;
        if w >= 90 {
            let l = w * 58 / 100;
            (l, w - l - 1)
        } else {
            (w, 0)
        }
    }

    pub fn render(&mut self, _shared: &mut Shared, rows: usize, cols: usize) {
        let w = cols.saturating_sub(1);
        self.width = w;
        self.zones.clear();
        let mut lines: Vec<Line> = Vec::new();

        // -- header ---------------------------------------------------------------
        let mut head = Line::new();
        head.push("", " ");
        let x0 = head.width;
        head.push(&format!("{BOLD}{MAGENTA}"), &format!("⎇ {}", ui::truncate(&self.ctl.current_name(), 30)));
        if let Some(b) = self.ctl.current() {
            if b.ahead > 0 {
                head.push(GREEN, &format!(" ↑{}", b.ahead));
            }
            if b.behind > 0 {
                head.push(RED, &format!(" ↓{}", b.behind));
            }
        }
        head.push(DIM, " ▾");
        self.zones.push((0, x0, head.width, Zone::Branch));
        head.push(DIM, "  │  ");
        let x0 = head.width;
        let scope = match &self.scope {
            Scope::All => "all branches".to_string(),
            Scope::Head => "current branch".to_string(),
            Scope::Ref(r) => r.clone(),
        };
        head.push(CYAN, &format!("{} ▾", ui::truncate(&scope, 30)));
        if let Some(p) = &self.path {
            head.push(YELLOW, &format!("  ⌁ {}", ui::truncate_left(p, 30)));
        }
        self.zones.push((0, x0, head.width, Zone::Scope));
        head.push(DIM, "  │  ");
        let x0 = head.width;
        if self.typing || !self.filter.is_empty() {
            head.push(DIM, "⌕ ");
            head.push(BOLD, &ui::truncate(&self.filter, 24));
            if self.typing {
                head.push(REVERSE, " ");
            }
        } else {
            head.push(DIM, "⌕ filter (/)");
        }
        self.zones.push((0, x0, head.width, Zone::Filter));
        if let Some(op) = self.ctl.state {
            head.push("", "   ");
            head.push(&format!("{BOLD}{YELLOW}"), op.title());
            head.push("", " ");
            let x0 = head.width;
            head.push(GREEN, "[▶ continue]");
            self.zones.push((0, x0, head.width, Zone::Continue));
            head.push("", " ");
            let x0 = head.width;
            head.push(RED, "[■ abort]");
            self.zones.push((0, x0, head.width, Zone::Abort));
        }
        let count = if self.loading && self.commits.is_empty() {
            "loading… ".to_string()
        } else if self.filter.is_empty() {
            format!("{} commits ", self.shown.len())
        } else {
            format!("{}/{} commits ", self.shown.len(), self.commits.len())
        };
        head.right(DIM, &count, w);
        lines.push(head);

        let body_h = rows.saturating_sub(3);
        self.body_h = body_h;
        let (lw, rw) = self.split();
        let narrow = rw == 0;
        let mut sep = Line::new();
        if narrow {
            sep.push(DIM, &"─".repeat(w));
        } else {
            sep.push(DIM, &format!("{}┬{}", "─".repeat(lw), "─".repeat(rw)));
        }
        lines.push(sep);

        // -- log ------------------------------------------------------------------
        let vis = self.visible();
        if self.sel < self.scroll {
            self.scroll = self.sel;
        } else if self.sel >= self.scroll + vis {
            self.scroll = self.sel + 1 - vis;
        }
        self.scroll = self.scroll.min(self.shown.len().saturating_sub(vis));
        let left: Vec<Line> = if narrow && self.focus == Pane::Files { Vec::new() } else { self.log_lines(lw, body_h) };
        let right: Vec<Line> = if narrow && self.focus == Pane::Log { Vec::new() } else { self.side_lines(if narrow { w } else { rw }, body_h, if narrow { 0 } else { lw + 1 }) };
        for r in 0..body_h {
            let mut l = Line::new();
            if narrow {
                let src = if self.focus == Pane::Files { &right } else { &left };
                if let Some(x) = src.get(r) {
                    l.append(x);
                }
            } else {
                l.append(&left.get(r).cloned().unwrap_or_default().fit(lw));
                l.push(DIM, "│");
                if let Some(x) = right.get(r) {
                    l.append(&x.fit(rw));
                }
            }
            lines.push(l);
        }

        // -- footer ---------------------------------------------------------------
        let mut foot = Line::new();
        match &self.ctl.status {
            Some((msg, err)) => {
                foot.push(if *err { RED } else { GREEN }, &format!(" {}", ui::truncate(msg, w.saturating_sub(2))));
            }
            None => {
                let hint = match self.focus {
                    Pane::Log => " j/k move  Enter/m actions  l/Tab files  c commit  b branches  / filter  a scope  y copy hash  f fetch  r reload  q close",
                    Pane::Files => " j/k move  Enter diff  m actions  h/← back  y copy path  q back",
                };
                foot.push(DIM, &ui::truncate(hint, w));
            }
        }
        while lines.len() < rows.saturating_sub(1) {
            lines.push(Line::new());
        }
        lines.truncate(rows.saturating_sub(1));
        lines.push(foot);

        let pw = 58.min(w.saturating_sub(2));
        self.ctl.draw(&mut lines, w, rows, (1, 1, pw, body_h.min(rows.saturating_sub(2)).max(5)), true);
        ui::print_lines(&lines, rows, cols);
    }

    fn log_lines(&mut self, w: usize, h: usize) -> Vec<Line> {
        let mut out = Vec::new();
        if let Some(e) = &self.error {
            out.push(ui::line(RED, &format!(" {}", ui::truncate(e, w.saturating_sub(2)))));
            return out;
        }
        if self.shown.is_empty() {
            out.push(ui::line(DIM, if self.loading { " loading…" } else { " no commits" }));
            return out;
        }
        let locals = self.ctl.locals();
        let show_graph = self.filter.is_empty();
        let per = self.per();
        let vis = (h / per).max(1);
        let graph_w = if show_graph {
            self.shown
                .iter()
                .skip(self.scroll)
                .take(vis)
                .map(|&i| self.graph.get(i).map(|g| g.node.len().max(g.link.len())).unwrap_or(0))
                .max()
                .unwrap_or(0)
                .min(MAX_GRAPH_COLS)
        } else {
            0
        };
        let author_w = if w >= 70 { 14 } else { 0 };
        let date_w = if w >= 90 { 16 } else if w >= 60 { 10 } else { 0 };
        let head_sha = self.ctl.head.clone();
        let focused = self.focus == Pane::Log && !self.ctl.modal();
        for (row, &ci) in self.shown.iter().enumerate().skip(self.scroll).take(vis) {
            let c = &self.commits[ci];
            let sel = row == self.sel;
            let mut l = Line::new();
            l.push(if sel { MAGENTA } else { "" }, if sel && !focused { "▌" } else { " " });
            if show_graph {
                let cells = self.graph.get(ci).map(|g| g.node.as_slice()).unwrap_or(&[]);
                for &(ch, lane) in cells.iter().take(graph_w) {
                    let ch = if ch == '●' && head_sha.as_deref() == Some(c.sha.as_str()) { '◉' } else { ch };
                    l.push(LANE_COLORS[lane % LANE_COLORS.len()], &ch.to_string());
                }
                l.pad(1 + graph_w, "");
                l.push("", " ");
            }
            l.push(YELLOW, c.short());
            l.push("", " ");
            let mut tail = Line::new();
            if author_w > 0 {
                tail.push(CYAN, &format!(" {:<width$}", ui::truncate(&c.author, author_w), width = author_w));
            }
            if date_w > 0 {
                let d = if date_w >= 16 { c.date.clone() } else { c.date.chars().take(10).collect() };
                tail.push(DIM, &format!(" {d:>date_w$}"));
            }
            let room = w.saturating_sub(l.width + tail.width + 1);
            let mut mid = Line::new();
            for r in gitlog::classify(&c.decorations, &locals) {
                let (style, text) = match r.kind {
                    RefKind::Current => (format!("{BOLD}{GREEN}"), format!("[{}]", r.name)),
                    RefKind::Head => (format!("{BOLD}{CYAN}"), "[HEAD]".to_string()),
                    RefKind::Local => (GREEN.to_string(), format!("[{}]", r.name)),
                    RefKind::Remote => (MAGENTA.to_string(), format!("[{}]", r.name)),
                    RefKind::Tag => (YELLOW.to_string(), format!("◆{}", r.name)),
                };
                if mid.width + ui::width(&text) + 12 > room {
                    mid.push(DIM, "…");
                    break;
                }
                mid.push(&style, &text);
                mid.push("", " ");
            }
            let subj_style = if c.parents.len() > 1 { DIM } else if sel && focused { BOLD } else { "" };
            mid.push(subj_style, &ui::truncate(&c.subject, room.saturating_sub(mid.width)));
            l.append(&mid.fit(room));
            l.append(&tail);
            let mut l = l.fit(w);
            if sel && focused {
                l.restyle(REVERSE);
            }
            let y = 2 + out.len();
            self.zones.push((y, 0, w, Zone::Commit(row)));
            out.push(l);
            if per == 2 && out.len() < h {
                // the connector row: lanes running on to the next commit
                let mut link = Line::new();
                link.push("", " ");
                if let Some(g) = self.graph.get(ci) {
                    for &(ch, lane) in g.link.iter().take(graph_w) {
                        link.push(LANE_COLORS[lane % LANE_COLORS.len()], &ch.to_string());
                    }
                }
                self.zones.push((2 + out.len(), 0, w, Zone::Commit(row)));
                out.push(link);
            }
        }
        out
    }

    /// Files of the selected commit on top, its details below. `x` is the pane's column.
    fn side_lines(&mut self, w: usize, h: usize, x: usize) -> Vec<Line> {
        let mut out = Vec::new();
        let Some(c) = self.selected().cloned() else { return out };
        let focused = self.focus == Pane::Files && !self.ctl.modal();
        let files: Vec<ChangedFile> = self.sel_files().to_vec();
        let mut head = Line::new();
        match self.files.get(&c.sha) {
            Some(Files::Ready(v)) => {
                let (a, d) = v.iter().filter_map(|f| f.stats).fold((0, 0), |s, (a, d)| (s.0 + a, s.1 + d));
                head.push(BOLD, &format!(" {} file{}", v.len(), if v.len() == 1 { "" } else { "s" }));
                head.push("", "  ");
                head.push(GREEN, &format!("+{a}"));
                head.push("", " ");
                head.push(RED, &format!("-{d}"));
                if c.parents.len() > 1 {
                    head.push(DIM, "  (vs first parent)");
                }
                if !v.is_empty() {
                    head.right(DIM, &format!("{}/{} ", self.fsel + 1, v.len()), w);
                }
            }
            Some(Files::Failed(e)) => {
                head.push(RED, &format!(" {}", ui::truncate(e, w.saturating_sub(2))));
            }
            _ => {
                head.push(DIM, " loading files…");
            }
        }
        out.push(head);
        // files take what they need, up to ~60%; details get the rest (at least 6 rows)
        let files_h = files.len().min((h * 60 / 100).max(3)).min(h.saturating_sub(7));
        if self.fsel >= files.len() {
            self.fsel = files.len().saturating_sub(1);
        }
        if self.fsel < self.fscroll {
            self.fscroll = self.fsel;
        } else if files_h > 0 && self.fsel >= self.fscroll + files_h {
            self.fscroll = self.fsel + 1 - files_h;
        }
        for (i, f) in files.iter().enumerate().skip(self.fscroll).take(files_h) {
            let sel = i == self.fsel;
            let style = match f.code.as_str() {
                "A" => GREEN,
                "D" => RED,
                "R" | "C" => BLUE,
                _ => YELLOW,
            };
            let mut l = Line::new();
            l.push(if sel { MAGENTA } else { "" }, if sel && !focused { "▌" } else { " " });
            l.push(style, &format!("{} ", f.code));
            let stats = match f.stats {
                Some((a, d)) => format!("+{a} -{d} "),
                None => "bin ".into(),
            };
            let (dir, name) = match f.path.rfind('/') {
                Some(p) => (&f.path[..p + 1], &f.path[p + 1..]),
                None => ("", f.path.as_str()),
            };
            let room = w.saturating_sub(l.width + stats.len() + 1);
            l.push(if sel && focused { BOLD } else { "" }, &ui::truncate(name, room));
            let left = room.saturating_sub(ui::width(name) + 1);
            if left > 3 {
                let mut d = format!(" {}", dir.trim_end_matches('/'));
                if let Some(o) = &f.old {
                    d = format!(" ← {o}");
                }
                l.push(DIM, &ui::truncate_left(&d, left));
            }
            l.right(DIM, &stats, w);
            let mut l = l.fit(w);
            if sel && focused {
                l.restyle(REVERSE);
            }
            self.zones.push((2 + out.len(), x, x + w, Zone::File(i)));
            out.push(l);
        }
        // -- details -----------------------------------------------------------
        out.push(ui::line(DIM, &"─".repeat(w)));
        let locals = self.ctl.locals();
        let mut l = Line::new();
        l.push(YELLOW, &format!(" {}", c.sha));
        out.push(l);
        let mut l = Line::new();
        l.push(CYAN, &format!(" {}", c.author));
        l.push(DIM, &format!(" <{}>", c.email));
        out.push(l.fit(w));
        let mut l = Line::new();
        l.push(DIM, &format!(" {}", c.date));
        if c.parents.len() > 1 {
            l.push(DIM, &format!("  merge of {}", c.parents.iter().map(|p| short(p)).collect::<Vec<_>>().join(" + ")));
        } else if let Some(p) = c.parents.first() {
            l.push(DIM, &format!("  parent {}", short(p)));
        }
        out.push(l.fit(w));
        let refs = gitlog::classify(&c.decorations, &locals);
        if !refs.is_empty() {
            let mut l = Line::new();
            l.push("", " ");
            for r in refs {
                let style = match r.kind {
                    RefKind::Current | RefKind::Local => GREEN,
                    RefKind::Remote => MAGENTA,
                    RefKind::Tag => YELLOW,
                    RefKind::Head => CYAN,
                };
                l.push(style, &format!("{} ", r.name));
            }
            out.push(l.fit(w));
        }
        out.push(Line::new());
        for s in ui::wrap(&c.subject, w.saturating_sub(2)) {
            out.push(ui::line(BOLD, &format!(" {s}")));
        }
        if !c.body.is_empty() {
            out.push(Line::new());
            for s in ui::wrap(&c.body, w.saturating_sub(2)) {
                out.push(ui::line("", &format!(" {s}")));
            }
        }
        out.truncate(h);
        out
    }
}

