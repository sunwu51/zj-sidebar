//! Floating overlay (`mode "diff"`): side-by-side `git diff` of one file: the work tree
//! against HEAD (from the panel), a commit against its parent (`diff_rev` + `diff_base`,
//! or `diff_root` for a root commit), or a commit against the work tree (`diff_base` only).

use std::collections::BTreeMap;
use std::path::PathBuf;

use unicode_segmentation::UnicodeSegmentation;
use zellij_tile::prelude::*;

use crate::diffparse::{self, Folds, Item, Kind, Parsed, Row, Side, CONTEXT};
use crate::highlight::{self, Span, Tok};
use crate::panel::default_helper;
use crate::ui::{self, Line, BLUE, BOLD, CYAN, DIM, GREEN, MAGENTA, RED, REVERSE, YELLOW};
use crate::Shared;

const HSTEP: usize = 8;
/// rows one click / key press unfolds
const EXPAND: usize = 50;
/// background of removed / added lines (256-color: dark red / dark green)
const BG_REMOVED: &str = "\x1b[48;5;52m";
const BG_ADDED: &str = "\x1b[48;5;22m";
/// the whole file as context; what is shown of it is decided by `Folds`
/// larger than this (either version), a file is shown without its full context and colors
const BIG_BYTES: u64 = 2 << 20;
const FULL: &str = "-U99999999";
const GIT: &[&str] = &["git", "--no-optional-locks", "-c", "core.quotepath=false"];

enum State {
    Loading,
    Ready(Parsed),
    Failed(String),
}

impl Default for State {
    fn default() -> Self {
        State::Loading
    }
}

#[derive(Default)]
pub struct DiffView {
    cwd: String,
    path: String,
    untracked: bool,
    /// the source path of a renamed file
    old: Option<String>,
    /// commit to show (empty: the work tree)
    rev: String,
    /// what it is compared with (empty with a `rev`: a root commit; both empty: HEAD)
    base: String,
    root_commit: bool,
    /// (status code, path, old path) of every changed file, and the position of the current one
    files: Vec<(String, String, Option<String>)>,
    index: usize,
    state: State,
    scroll: usize,
    hscroll: usize,
    /// body height of the last render, for paging and clamping
    body: usize,
    /// syntax highlighting per line (index = line number - 1) of the old and the new version
    hl: [Vec<Vec<Span>>; 2],
    /// native helper (sizes, highlighting)
    helper: String,
    /// too big to show with all its context and colors: a plain `-U3` diff
    big: bool,
    folds: Folds,
    /// what is on screen (folds applied), and where each hunk starts in it
    items: Vec<Item>,
    /// where each diff point (run of changed lines) starts in `items`
    hunk_at: Vec<usize>,
    /// the diff point last jumped to, and the scroll it led to (reset when the user scrolls)
    point: Option<(usize, usize)>,
    /// search in this file: the text, whether it is being typed, the matching rows
    query: String,
    typing: bool,
    matches: Vec<usize>,
    cur: usize,
    /// (screen line, from column, to column, what a click does) of the last render
    zones: Vec<(usize, usize, usize, Click)>,
    /// text selection (the terminal's own can't work: zellij owns the mouse): where the
    /// press was, what is selected, and a message about the last copy
    anchor: Option<Pos>,
    sel: Option<Sel>,
    status: Option<String>,
    /// (gutter width, width of the left half) of the last render
    geom: (usize, usize),
}

/// A point in the text: the side (0 old, 1 new), the line on screen (index into `items`)
/// and the column in the line's text (before horizontal scrolling).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Pos {
    side: usize,
    item: usize,
    col: usize,
}

/// Selected from `.0` to `.1`, on one side (the one the drag started on).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Sel(Pos, Pos);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Click {
    /// previous / next diff point
    Diff(isize),
    Down(usize),
    Up(usize),
    All(usize),
}


impl DiffView {
    pub fn load(&mut self, shared: &Shared) {
        trace!(shared, "diff load path={:?} cwd={:?} rev={:?} base={:?}", shared.cfg("diff_path"), shared.cfg("diff_cwd"), shared.cfg("diff_rev"), shared.cfg("diff_base"));
        self.cwd = shared.cfg("diff_cwd").unwrap_or("").to_string();
        self.path = shared.cfg("diff_path").unwrap_or("").to_string();
        self.untracked = shared.cfg("diff_code") == Some("??");
        self.rev = shared.cfg("diff_rev").unwrap_or("").to_string();
        self.base = shared.cfg("diff_base").unwrap_or("").to_string();
        self.root_commit = shared.cfg("diff_root") == Some("1");
        self.helper = shared.cfg("helper").unwrap_or("").to_string();
        self.files = shared
            .cfg("diff_files")
            .unwrap_or("")
            .lines()
            .filter_map(|l| {
                let mut f = l.split('\t');
                let code = f.next()?.to_string();
                let path = f.next()?.to_string();
                Some((code, path, f.next().filter(|o| !o.is_empty()).map(String::from)))
            })
            .collect();
        self.index = shared
            .cfg("diff_index")
            .and_then(|i| i.parse().ok())
            .filter(|&i| self.files.get(i).is_some_and(|(_, p, _)| *p == self.path))
            .or_else(|| self.files.iter().position(|(_, p, _)| *p == self.path))
            .unwrap_or(0);
        self.old = self.files.get(self.index).and_then(|f| f.2.clone());
    }

    /// Move to the next/previous changed file (stops at the ends).
    fn step_file(&mut self, delta: isize) {
        let to = self.index as isize + delta;
        if to < 0 || to as usize >= self.files.len() || to as usize == self.index {
            return;
        }
        self.index = to as usize;
        let (code, path, old) = self.files[self.index].clone();
        self.untracked = code == "??";
        self.path = path;
        self.old = old;
        ztrace!("diff step_file -> {:?}", self.path);
        self.state = State::Loading;
        self.scroll = 0;
        self.hscroll = 0;
        self.folds = Folds::default();
        self.hl = Default::default();
        self.big = false;
        self.relayout();
        self.matches.clear();
        rename_plugin_pane(get_plugin_ids().plugin_id, &self.title());
        self.start();
    }

    fn run(&self, extra: &[&str], stage: &str) {
        let mut cmd: Vec<&str> = GIT.to_vec();
        cmd.extend_from_slice(extra);
        cmd.push("--");
        cmd.extend(self.path_args());
        run_command_with_env_variables_and_cwd(
            &cmd,
            BTreeMap::new(),
            PathBuf::from(&self.cwd),
            BTreeMap::from([
                ("diff".to_string(), stage.to_string()),
                // answers for a file we have already moved on from are ignored
                ("path".to_string(), self.path.clone()),
            ]),
        );
    }

    fn path_args(&self) -> Vec<&str> {
        if self.untracked {
            return vec!["/dev/null", self.path.as_str()];
        }
        match &self.old {
            // both sides of a rename, so git can pair them up
            Some(o) if *o != self.path => vec![o.as_str(), self.path.as_str()],
            _ => vec![self.path.as_str()],
        }
    }

    fn title(&self) -> String {
        match (self.rev.as_str(), self.base.as_str()) {
            ("", "") => self.path.clone(),
            ("", b) => format!("{} @ {} ↔ local", self.path, short(b)),
            (r, _) => format!("{} @ {}", self.path, short(r)),
        }
    }

    /// The two versions for the helper: `git:<rev>:<path>`, `file:<path>` or `none`.
    fn specs(&self) -> (String, String) {
        let old_path = self.old.as_deref().unwrap_or(&self.path);
        if !self.rev.is_empty() {
            let old = if self.root_commit { "none".to_string() } else { format!("git:{}:{old_path}", self.base) };
            (old, format!("git:{}:{}", self.rev, self.path))
        } else if !self.base.is_empty() {
            (format!("git:{}:{old_path}", self.base), format!("file:{}", self.path))
        } else if self.untracked {
            ("none".to_string(), format!("file:{}", self.path))
        } else {
            (format!("git:HEAD:{old_path}"), format!("file:{}", self.path))
        }
    }

    fn run_helper(&self, args: &[&str], stage: &str) {
        let mut cmd: Vec<&str> = vec![self.helper.as_str()];
        cmd.extend_from_slice(args);
        run_command_with_env_variables_and_cwd(
            &cmd,
            BTreeMap::new(),
            PathBuf::from(&self.cwd),
            BTreeMap::from([("diff".to_string(), stage.to_string()), ("path".to_string(), self.path.clone())]),
        );
    }

    /// First ask how big the file is: the diff holds the whole file in the plugin's 16 MiB.
    fn start(&mut self) {
        ztrace!("diff start path={:?} untracked={} old={:?}", self.path, self.untracked, self.old);
        if self.helper.is_empty() {
            self.helper = default_helper(&get_session_environment_variables());
        }
        let (old, new) = self.specs();
        self.run_helper(&["size", &self.cwd, &old, &new], "size");
    }

    /// The context lines of the diff: all of them, or just git's default.
    fn unified(&self) -> &'static str {
        if self.big { "-U3" } else { FULL }
    }

    fn fetch(&self) {
        let u = self.unified();
        if !self.rev.is_empty() {
            if self.root_commit {
                self.run(&["show", "--format=", "--no-color", u, "-M", &self.rev], "commit");
            } else {
                self.run(&["diff", "--no-color", u, "-M", &self.base, &self.rev], "commit");
            }
        } else if !self.base.is_empty() {
            self.run(&["diff", "--no-color", u, "-M", &self.base], "commit");
        } else if self.untracked {
            self.run(&["diff", "--no-color", "--no-index"], "main");
        } else {
            self.run(&["diff", "--no-color", u, "HEAD"], "main");
        }
        if !self.big {
            let (old, new) = self.specs();
            self.run_helper(&["highlight", &self.cwd, &self.path, &old, &new], "hl");
        }
    }

    pub fn update(&mut self, _shared: &mut Shared, event: &Event) -> bool {
        match event {
            Event::PermissionRequestResult(PermissionStatus::Granted) => {
                self.start();
                true
            }
            Event::RunCommandResult(code, stdout, stderr, c) => {
                let Some(stage) = c.get("diff") else { return false };
                if c.get("path") != Some(&self.path) {
                    ztrace!("diff result for another path {:?}, now {:?}", c.get("path"), self.path);
                    return false;
                }
                ztrace!("diff result stage={stage} code={code:?} stdout={}B", stdout.len());
                match stage.as_str() {
                    "size" => {
                        // both versions as bytes; without the helper, play safe
                        let text = String::from_utf8_lossy(stdout);
                        let sizes: Vec<u64> = text.split_whitespace().filter_map(|n| n.parse().ok()).collect();
                        self.big = *code != Some(0) || sizes.len() != 2 || sizes.iter().any(|&n| n > BIG_BYTES);
                        ztrace!("diff size {sizes:?} big={}", self.big);
                        self.fetch();
                        return true;
                    }
                    "hl" => {
                        if *code == Some(0) {
                            self.hl = highlight::parse(&String::from_utf8_lossy(stdout));
                        }
                        return true;
                    }
                    _ => {}
                }
                // `git diff --no-index` exits 1 when files differ
                let ok = matches!(code, Some(0) | Some(1));
                if ok {
                    let parsed = diffparse::parse(&String::from_utf8_lossy(stdout));
                    self.folds = diffparse::folds(&parsed, CONTEXT);
                    // everything is shown; `C` folds the unchanged stretches again
                    self.folds.gaps.iter_mut().for_each(|g| g.open_all());
                    self.state = State::Ready(parsed);
                    self.relayout();
                    // open on the first change, since the whole file is shown
                    self.scroll = self.hunk_at.first().map_or(0, |h| h.saturating_sub(2));
                    self.point = self.hunk_at.first().map(|_| (0, self.scroll));
                    self.search();
                } else if stage == "main" {
                    // no HEAD yet (fresh repo): fall back to the unstaged diff
                    self.run(&["diff", "--no-color", self.unified()], "fallback");
                } else {
                    self.state = State::Failed(String::from_utf8_lossy(stderr).trim().to_string());
                }
                true
            }
            Event::Key(key) => self.key(key),
            Event::Mouse(Mouse::LeftClick(line, col)) => {
                self.status = None;
                let (line, col) = ((*line).max(0) as usize, *col);
                self.sel = None;
                self.anchor = None;
                if self.click(line, col) {
                    return true;
                }
                self.anchor = self.pos_at(line, col, None);
                true
            }
            Event::Mouse(Mouse::Hold(line, col)) => self.drag((*line).max(0) as usize, *col),
            Event::Mouse(Mouse::Release(..)) => {
                self.anchor = None;
                match self.selected_text() {
                    Some(text) => {
                        let n = text.lines().count().max(1);
                        copy_to_clipboard(text);
                        self.status = Some(format!("copied {n} line{}", if n == 1 { "" } else { "s" }));
                        true
                    }
                    None => false,
                }
            }
            Event::Mouse(Mouse::ScrollUp(n)) => {
                self.scroll = self.scroll.saturating_sub(*n.max(&3));
                true
            }
            Event::Mouse(Mouse::ScrollDown(n)) => {
                self.scroll += *n.max(&3);
                true
            }
            _ => false,
        }
    }

    fn rows(&self) -> usize {
        self.items.len()
    }

    /// Rebuild what is on screen after the parse or a fold changed.
    fn relayout(&mut self) {
        self.anchor = None;
        self.sel = None;
        let (items, _) = diffparse::layout(&self.folds);
        // a diff point is a run of changed lines
        self.hunk_at.clear();
        if let State::Ready(p) = &self.state {
            let mut prev = false;
            for (i, it) in items.iter().enumerate() {
                let c = matches!(it, Item::Row(r) if p.rows[*r].changed());
                if c && !prev {
                    self.hunk_at.push(i);
                }
                prev = c;
            }
        }
        self.items = items;
        self.point = None;
    }

    /// The fold the keys act on: the first one on screen (or the next one below).
    fn active_fold(&self) -> Option<usize> {
        self.items.iter().skip(self.scroll).find_map(|it| match it {
            Item::Fold(g) => Some(*g),
            _ => None,
        })
    }

    fn unfold(&mut self, c: Click) {
        let (Click::Down(g) | Click::Up(g) | Click::All(g)) = c else {
            if let Click::Diff(d) = c {
                self.jump_diff(d > 0);
            }
            return;
        };
        let Some(gap) = self.folds.gaps.get_mut(g) else { return };
        match c {
            Click::Down(_) => gap.open_down(EXPAND),
            Click::Up(_) => gap.open_up(EXPAND),
            _ => gap.open_all(),
        }
        self.relayout();
    }

    // -- selection --------------------------------------------------------------------

    /// The point under the screen position (`side`: the side to measure against, else the
    /// one the position is on; None in the gutter between the two).
    fn pos_at(&self, line: usize, col: usize, side: Option<usize>) -> Option<Pos> {
        let (gutter, half) = self.geom;
        let side = side.or(if col < half { Some(0) } else if col >= half + 3 { Some(1) } else { None })?;
        let item = (self.scroll + line.checked_sub(2)?).min(self.items.len().checked_sub(1)?);
        let x0 = if side == 0 { 0 } else { half + 3 };
        let col = col.saturating_sub(x0 + gutter + 2) + self.hscroll;
        Some(Pos { side, item, col })
    }

    /// The mouse moved with the button down: extend the selection, scrolling at the edges.
    fn drag(&mut self, line: usize, col: usize) -> bool {
        let Some(a) = self.anchor else { return false };
        if line < 2 {
            self.scroll = self.scroll.saturating_sub(1);
        } else if line >= 2 + self.body {
            self.scroll += 1;
        }
        let line = line.clamp(2, 1 + self.body.max(1));
        let Some(head) = self.pos_at(line, col, Some(a.side)) else { return false };
        self.sel = Some(Sel(a, head));
        true
    }

    /// The selection with its ends in reading order.
    fn ordered(&self) -> Option<(Pos, Pos)> {
        let Sel(a, b) = self.sel?;
        Some(if (a.item, a.col) <= (b.item, b.col) { (a, b) } else { (b, a) })
    }

    /// The byte range of the selection in the text of `side` on item `n`.
    fn selected_bytes(&self, n: usize, side: usize, text: &str) -> Option<(usize, usize)> {
        let (a, b) = self.ordered()?;
        if a.side != side || n < a.item || n > b.item {
            return None;
        }
        let from = if n == a.item { a.col } else { 0 };
        let to = if n == b.item { b.col + 1 } else { usize::MAX };
        byte_range(text, from, to)
    }

    fn selected_text(&self) -> Option<String> {
        let (a, b) = self.ordered()?;
        let State::Ready(p) = &self.state else { return None };
        let mut out: Vec<&str> = Vec::new();
        for n in a.item..=b.item {
            let Some(Item::Row(r)) = self.items.get(n) else { continue };
            let Row::Pair { left, right } = &p.rows[*r] else { continue };
            let side = if a.side == 0 { left } else { right };
            if side.kind == Kind::Empty {
                continue;
            }
            out.push(self.selected_bytes(n, a.side, &side.text).map_or("", |(x, y)| &side.text[x..y]));
        }
        let text = out.join("\n");
        (!text.is_empty()).then_some(text)
    }

    fn click(&mut self, line: usize, col: usize) -> bool {
        let hit = self.zones.iter().find(|(l, a, b, _)| *l == line && col >= *a && col < *b).map(|z| z.3);
        match hit {
            Some(c) => {
                self.unfold(c);
                true
            }
            None => false,
        }
    }

    // -- search ---------------------------------------------------------------------

    /// Recompute the matches of the query (rows of both sides, hidden ones too) and
    /// go to the first one at or after the top of the screen.
    fn search(&mut self) {
        self.matches = match &self.state {
            State::Ready(p) => diffparse::search(p, &self.query),
            _ => Vec::new(),
        };
        self.cur = 0;
        if self.matches.is_empty() {
            return;
        }
        let top = self.items.get(self.scroll).and_then(|it| match it {
            Item::Row(r) => Some(*r),
            _ => None,
        });
        if let Some(top) = top {
            self.cur = self.matches.iter().position(|&m| m >= top).unwrap_or(0);
        }
        self.goto_match();
    }

    fn step_match(&mut self, delta: isize) {
        let n = self.matches.len() as isize;
        if n == 0 {
            return;
        }
        self.cur = (self.cur as isize + delta).rem_euclid(n) as usize;
        self.goto_match();
    }

    /// Scroll to the current match, unfolding the lines around it if they are hidden.
    fn goto_match(&mut self) {
        let Some(&row) = self.matches.get(self.cur) else { return };
        if let Some(pos) = self.folds.order.iter().position(|&r| r == row) {
            if let Some(g) = self.folds.gaps.iter_mut().find(|g| g.start <= pos && pos < g.end) {
                let (a, b) = (g.start + g.top, g.end - g.bottom);
                if pos >= a && pos < b {
                    g.reveal(pos, CONTEXT);
                    self.relayout();
                }
            }
        }
        if let Some(i) = self.items.iter().position(|it| *it == Item::Row(row)) {
            let off = (self.body / 3).min(i);
            if i < self.scroll || i >= self.scroll + self.body.max(1) {
                self.scroll = i - off;
            }
        }
    }

    fn type_key(&mut self, key: &KeyWithModifier) -> bool {
        match key.bare_key {
            BareKey::Esc => {
                self.typing = false;
                self.query.clear();
                self.search();
            }
            BareKey::Enter => {
                self.typing = false;
                self.step_match(1);
            }
            BareKey::Backspace => {
                self.query.pop();
                self.search();
            }
            BareKey::Char(c) => {
                self.query.push(c);
                self.search();
            }
            _ => {}
        }
        true
    }

    fn key(&mut self, key: &KeyWithModifier) -> bool {
        if key.key_modifiers.iter().any(|m| *m != KeyModifier::Shift) {
            return false;
        }
        self.status = None;
        if self.typing {
            return self.type_key(key);
        }
        let page = self.body.max(2) - 1;
        let shift = key.key_modifiers.contains(&KeyModifier::Shift);
        match key.bare_key {
            BareKey::Char('n') if !self.query.is_empty() => self.step_match(1),
            BareKey::Char('N') if !self.query.is_empty() => self.step_match(-1),
            BareKey::Char(']') | BareKey::Char('N') => self.step_file(1),
            BareKey::Char('[') | BareKey::Char('P') => self.step_file(-1),
            BareKey::Tab => self.step_file(if shift { -1 } else { 1 }),
            BareKey::Char('/') => {
                self.typing = true;
                self.query.clear();
                self.search();
            }
            BareKey::Char('u') => {
                if let Some(g) = self.active_fold() {
                    self.unfold(Click::Up(g));
                }
            }
            BareKey::Char('d') => {
                if let Some(g) = self.active_fold() {
                    self.unfold(Click::Down(g));
                }
            }
            BareKey::Char('a') => {
                if let Some(g) = self.active_fold() {
                    self.unfold(Click::All(g));
                }
            }
            BareKey::Char('E') => {
                self.folds.gaps.iter_mut().for_each(|g| g.open_all());
                self.relayout();
            }
            BareKey::Char('C') => {
                self.folds.gaps.iter_mut().for_each(|g| (g.top, g.bottom) = (0, 0));
                self.relayout();
            }
            BareKey::Esc if self.sel.is_some() => self.sel = None,
            BareKey::Char('y') => {
                if let Some(text) = self.selected_text() {
                    copy_to_clipboard(text);
                    self.status = Some("copied".to_string());
                }
            }
            BareKey::Esc if !self.query.is_empty() => {
                self.query.clear();
                self.search();
            }
            BareKey::Esc | BareKey::Char('q') => close_self(),
            BareKey::Down | BareKey::Char('j') => self.scroll += 1,
            BareKey::Up | BareKey::Char('k') => self.scroll = self.scroll.saturating_sub(1),
            BareKey::PageDown | BareKey::Char(' ') | BareKey::Char('f') => self.scroll += page,
            BareKey::PageUp | BareKey::Char('b') => self.scroll = self.scroll.saturating_sub(page),
            BareKey::Home | BareKey::Char('g') => self.scroll = 0,
            BareKey::End | BareKey::Char('G') => self.scroll = usize::MAX / 2,
            BareKey::Right | BareKey::Char('l') => self.hscroll += HSTEP,
            BareKey::Left | BareKey::Char('h') => self.hscroll = self.hscroll.saturating_sub(HSTEP),
            BareKey::Char('n') | BareKey::Char('}') => self.jump_diff(true),
            BareKey::Char('p') | BareKey::Char('{') => self.jump_diff(false),
            _ => return false,
        }
        true
    }

    /// Scroll to the next / previous diff point (a few lines of context above it).
    fn jump_diff(&mut self, forward: bool) {
        let n = self.hunk_at.len();
        if n == 0 {
            return;
        }
        let at = |i: usize| self.hunk_at[i].saturating_sub(2);
        let cur = self.point.map(|(i, _)| i).or_else(|| {
            // after scrolling by hand: the last point at or above the top of the screen
            self.hunk_at.iter().rposition(|&h| h.saturating_sub(2) <= self.scroll)
        });
        let to = match (cur, forward) {
            (Some(i), true) => (i + 1).min(n - 1),
            (None, true) => 0,
            (Some(i), false) if self.point.is_none() && at(i) < self.scroll => i,
            (Some(i), false) => i.saturating_sub(1),
            (None, false) => 0,
        };
        self.scroll = at(to);
        self.point = Some((to, self.scroll));
    }

    pub fn render(&mut self, _shared: &mut Shared, rows: usize, cols: usize) {
        let cols = cols.saturating_sub(1);
        let body = rows.saturating_sub(3);
        self.body = body;
        let total = self.rows();
        self.scroll = self.scroll.min(total.saturating_sub(body));
        if self.point.is_some_and(|(_, s)| s.min(total.saturating_sub(body)) != self.scroll) {
            self.point = None;
        }

        let mut lines: Vec<Line> = Vec::new();
        let mut zones = Vec::new();
        let mut head = Line::new();
        head.push(&format!("{BOLD}{CYAN}"), &format!(" {}", ui::truncate_left(&self.path, cols.saturating_sub(40))));
        match (self.rev.as_str(), self.base.as_str()) {
            ("", "") => {}
            ("", b) => {
                head.push(DIM, &format!("  {} ↔ work tree", short(b)));
            }
            (r, _) => {
                head.push(DIM, &format!("  @ {}", short(r)));
            }
        }
        if let State::Ready(p) = &self.state {
            head.push("", "  ");
            head.push(GREEN, &format!("+{}", p.added));
            head.push("", " ");
            head.push(RED, &format!("-{}", p.removed));
            let mut info = String::new();
            if self.big {
                info.push_str("large file: changes only  ");
            }
            if self.files.len() > 1 {
                info.push_str(&format!("file {}/{}  ", self.index + 1, self.files.len()));
            }
            let mut tail = Line::new();
            tail.push(DIM, &info);
            let n = self.hunk_at.len();
            if n > 0 {
                let at = match self.point {
                    Some((i, _)) => i + 1,
                    None => self.hunk_at.iter().filter(|&&h| h.saturating_sub(2) <= self.scroll).count().max(1),
                };
                tail.push(BOLD, &format!("diff {at}/{n}  "));
                let x0 = tail.width;
                tail.push(&format!("{BOLD}{CYAN}"), "[▲ prev]");
                zones.push((0, x0, tail.width, Click::Diff(-1)));
                tail.push("", " ");
                let x0 = tail.width;
                tail.push(&format!("{BOLD}{CYAN}"), "[▼ next]");
                zones.push((0, x0, tail.width, Click::Diff(1)));
                tail.push("", " ");
            } else {
                tail.push(DIM, "no changes ");
            }
            let x = cols.saturating_sub(tail.width);
            for z in zones.iter_mut().filter(|z| z.0 == 0) {
                z.1 += x;
                z.2 += x;
            }
            head.pad(x, "");
            head.append(&tail);
        } else if self.files.len() > 1 {
            head.right(DIM, &format!("file {}/{} ", self.index + 1, self.files.len()), cols);
        }
        lines.push(head);
        let mut sep = Line::new();
        sep.push(DIM, &"─".repeat(cols));
        lines.push(sep);

        match &self.state {
            State::Loading => lines.push(note(DIM, " loading…")),
            State::Failed(e) => lines.push(note(RED, &format!(" git diff failed: {}", ui::truncate(e, cols.saturating_sub(20))))),
            State::Ready(p) if p.rows.is_empty() => lines.push(note(DIM, " no textual changes")),
            State::Ready(p) => {
                let max_no = p.rows.iter().filter_map(|r| match r {
                    Row::Pair { left, right } => left.no.max(right.no),
                    _ => None,
                }).max().unwrap_or(0);
                let gutter = max_no.to_string().len().max(3);
                // " │ " between the two halves
                let half = cols.saturating_sub(3) / 2;
                self.geom = (gutter, half);
                let active = self.active_fold();
                let current = self.matches.get(self.cur).copied();
                let spans = |s: &Side, side: usize| s.no.and_then(|n| self.hl[side].get(n - 1)).map_or(&[][..], |v| v.as_slice());
                let marks = if self.query.is_empty() { None } else { Some((self.query.as_str(), current)) };
                for (n, item) in self.items.iter().enumerate().skip(self.scroll).take(body) {
                    let y = lines.len();
                    lines.push(match *item {
                        Item::Fold(g) => {
                            let gap = &self.folds.gaps[g];
                            let mut l = Line::new();
                            l.push(CYAN, if active == Some(g) { " ▸" } else { "  " });
                            l.push(&format!("{DIM}{CYAN}"), &format!(" ⋯ {} unchanged line{}  ", gap.hidden(), if gap.hidden() == 1 { "" } else { "s" }));
                            let step = EXPAND.min(gap.hidden());
                            let mut button = |l: &mut Line, text: String, c: Click| {
                                let x0 = l.width;
                                l.push(&format!("{BOLD}{CYAN}"), &text);
                                zones.push((y, x0, l.width, c));
                                l.push("", " ");
                            };
                            if gap.above {
                                button(&mut l, format!("[↓ {step}]"), Click::Down(g));
                            }
                            if gap.below {
                                button(&mut l, format!("[↑ {step}]"), Click::Up(g));
                            }
                            button(&mut l, "[all]".to_string(), Click::All(g));
                            l
                        }
                        Item::Row(r) => match &p.rows[r] {
                            Row::Meta(text) => {
                                let mut l = Line::new();
                                l.push(&format!("{DIM}{CYAN}"), &ui::truncate(&format!(" {text}"), cols));
                                l
                            }
                            Row::Pair { left, right } => {
                                let marks = marks.map(|(q, cur)| (q, cur == Some(r)));
                                let mut l = Line::new();
                                let (ls, rs) = (self.selected_bytes(n, 0, &left.text), self.selected_bytes(n, 1, &right.text));
                                self.cell(&mut l, left, spans(left, 0), gutter, half, marks, ls);
                                l.push(DIM, " │ ");
                                self.cell(&mut l, right, spans(right, 1), gutter, cols.saturating_sub(half + 3), marks, rs);
                                l
                            }
                        },
                    });
                }
            }
        }
        self.zones = zones;
        while lines.len() < rows.saturating_sub(1) {
            lines.push(Line::new());
        }
        let mut foot = Line::new();
        if self.typing || !self.query.is_empty() {
            foot.push(DIM, " / ");
            foot.push(BOLD, &ui::truncate_left(&self.query, cols.saturating_sub(40)));
            if self.typing {
                foot.push(REVERSE, " ");
            }
            let found = if self.matches.is_empty() {
                if self.query.is_empty() { String::new() } else { "  no match".to_string() }
            } else {
                format!("  {}/{}", self.cur + 1, self.matches.len())
            };
            foot.push(if self.matches.is_empty() && !self.query.is_empty() { RED } else { YELLOW }, &found);
            foot.push(DIM, if self.typing { "   Enter next  Esc cancel" } else { "   n/N next/prev match  Esc clear" });
        } else if let Some(msg) = &self.status {
            foot.push(GREEN, &format!(" {msg}"));
        } else {
            foot.push(DIM, " q close  j/k scroll  space/b page  n/p diff point  ]/[ file  h/l side  / search  u/d/a unfold  E/C all/none  drag select+copy");
        }
        lines.push(foot);
        ui::print_lines(&lines, rows, cols);
    }

    /// One half of a row: `<no><sign> <text>`, `width` columns wide.
    /// `marks`: the search text, and whether this row is the current match.
    fn cell(&self, l: &mut Line, side: &Side, spans: &[Span], gutter: usize, width: usize, marks: Option<(&str, bool)>, sel: Option<(usize, usize)>) {
        // a tinted background marks the changed lines; the sign keeps the red/green
        let (sign_style, sign, bg) = match side.kind {
            Kind::Removed => (RED, '-', BG_REMOVED),
            Kind::Added => (GREEN, '+', BG_ADDED),
            Kind::Context => ("", ' ', ""),
            Kind::Empty => (DIM, ' ', ""),
        };
        let no = side.no.map(|n| n.to_string()).unwrap_or_default();
        let text_w = width.saturating_sub(gutter + 2);
        l.push(&format!("{bg}{DIM}"), &format!("{no:>gutter$}"));
        l.push(&format!("{bg}{BOLD}{sign_style}"), &format!("{sign} "));
        let hits = marks.map(|(q, _)| diffparse::find_all(&side.text, q)).unwrap_or_default();
        let hot = marks.is_some_and(|m| m.1);
        let mut used = 0;
        for (tok, hit, picked, piece) in pieces(&side.text, spans, &hits, sel, self.hscroll, text_w) {
            used += ui::width(&piece);
            let fg = match tok {
                Tok::Keyword => MAGENTA,
                Tok::String => YELLOW,
                Tok::Number => CYAN,
                Tok::Type | Tok::Function => BLUE,
                Tok::Comment => DIM,
                Tok::Plain => "",
            };
            let mut st = format!("{bg}{fg}");
            if picked {
                st.push_str(REVERSE);
            } else if hit {
                if hot {
                    st.push_str(BOLD);
                    st.push_str(YELLOW);
                }
                st.push_str(REVERSE);
            }
            l.push(&st, &piece);
        }
        l.push(bg, &" ".repeat(text_w.saturating_sub(used)));
    }
}

/// The part of `text` that is `width` columns wide, starting `skip` columns in, cut into
/// pieces of one syntax token and search-hit state each.
fn pieces(text: &str, spans: &[Span], hits: &[(usize, usize)], sel: Option<(usize, usize)>, skip: usize, width: usize) -> Vec<(Tok, bool, bool, String)> {
    let mut out: Vec<(Tok, bool, bool, String)> = Vec::new();
    let (mut col, mut used) = (0usize, 0usize);
    let (mut si, mut hi) = (0usize, 0usize);
    for (at, g) in text.grapheme_indices(true) {
        let cw = ui::width(g);
        let start = col;
        col += cw;
        if col <= skip {
            continue;
        }
        if used + cw > width {
            break;
        }
        while si < spans.len() && spans[si].end <= at {
            si += 1;
        }
        while hi < hits.len() && hits[hi].1 <= at {
            hi += 1;
        }
        let tok = spans.get(si).filter(|s| s.start <= at).map_or(Tok::Plain, |s| s.tok);
        let hit = hits.get(hi).is_some_and(|h| h.0 <= at);
        let picked = sel.is_some_and(|(a, b)| a <= at && at < b);
        // a wide character cut by the left edge
        let piece = if start < skip { " ".repeat(col - skip) } else { g.to_string() };
        used += ui::width(&piece);
        match out.last_mut() {
            Some((t, h, p, s)) if *t == tok && *h == hit && *p == picked => s.push_str(&piece),
            _ => out.push((tok, hit, picked, piece)),
        }
    }
    out
}

/// The bytes of `text` in the columns `from..to` (a wide character counts if any of its
/// columns is inside).
fn byte_range(text: &str, from: usize, to: usize) -> Option<(usize, usize)> {
    let (mut col, mut range) = (0usize, None::<(usize, usize)>);
    for (at, g) in text.grapheme_indices(true) {
        let start = col;
        col += ui::width(g);
        if col <= from {
            continue;
        }
        if start >= to {
            break;
        }
        range = Some((range.map_or(at, |r| r.0), at + g.len()));
    }
    range
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

fn note(style: &str, text: &str) -> Line {
    let mut l = Line::new();
    l.push(style, text);
    l
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_columns_to_bytes() {
        assert_eq!(byte_range("hello", 1, 3), Some((1, 3)));
        assert_eq!(byte_range("hello", 3, usize::MAX), Some((3, 5)));
        assert_eq!(byte_range("hello", 9, 12), None);
        assert_eq!(byte_range("", 0, 5), None);
        // a wide character covers two columns: either one selects it
        assert_eq!(byte_range("a中b", 1, 2), Some((1, 4)));
        assert_eq!(byte_range("a中b", 2, 3), Some((1, 4)));
        assert_eq!(byte_range("a中b", 3, 4), Some((4, 5)));
    }

    #[test]
    fn pieces_mark_the_selection() {
        let p = pieces("abcdef", &[], &[], Some((1, 3)), 0, 10);
        let got: Vec<(bool, &str)> = p.iter().map(|(_, _, picked, s)| (*picked, s.as_str())).collect();
        assert_eq!(got, vec![(false, "a"), (true, "bc"), (false, "def")]);
    }
}
