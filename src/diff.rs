//! Floating overlay (`mode "diff"`): side-by-side `git diff` of one file.

use std::collections::BTreeMap;
use std::path::PathBuf;

use zellij_tile::prelude::*;

use crate::diffparse::{self, Kind, Parsed, Row, Side};
use crate::ui::{self, Line, BOLD, CYAN, DIM, GREEN, RED};
use crate::Shared;

const HSTEP: usize = 8;
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
    /// (status code, path) of every changed file, and the position of the current one
    files: Vec<(String, String)>,
    index: usize,
    state: State,
    scroll: usize,
    hscroll: usize,
    /// body height of the last render, for paging and clamping
    body: usize,
}


impl DiffView {
    pub fn load(&mut self, shared: &Shared) {
        self.cwd = shared.cfg("diff_cwd").unwrap_or("").to_string();
        self.path = shared.cfg("diff_path").unwrap_or("").to_string();
        self.untracked = shared.cfg("diff_code") == Some("??");
        self.files = shared
            .cfg("diff_files")
            .unwrap_or("")
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .map(|(c, p)| (c.to_string(), p.to_string()))
            .collect();
        self.index = shared
            .cfg("diff_index")
            .and_then(|i| i.parse().ok())
            .filter(|&i| self.files.get(i).is_some_and(|(_, p)| *p == self.path))
            .or_else(|| self.files.iter().position(|(_, p)| *p == self.path))
            .unwrap_or(0);
    }

    /// Move to the next/previous changed file (stops at the ends).
    fn step_file(&mut self, delta: isize) {
        let to = self.index as isize + delta;
        if to < 0 || to as usize >= self.files.len() || to as usize == self.index {
            return;
        }
        self.index = to as usize;
        let (code, path) = self.files[self.index].clone();
        self.untracked = code == "??";
        self.path = path;
        self.state = State::Loading;
        self.scroll = 0;
        self.hscroll = 0;
        rename_plugin_pane(get_plugin_ids().plugin_id, &self.path);
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
        if self.untracked { vec!["/dev/null", self.path.as_str()] } else { vec![self.path.as_str()] }
    }

    fn start(&self) {
        if self.untracked {
            self.run(&["diff", "--no-color", "--no-index"], "main");
        } else {
            self.run(&["diff", "--no-color", "-U3", "HEAD"], "main");
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
                    return false;
                }
                // `git diff --no-index` exits 1 when files differ
                let ok = matches!(code, Some(0) | Some(1));
                if ok {
                    self.state = State::Ready(diffparse::parse(&String::from_utf8_lossy(stdout)));
                } else if stage == "main" {
                    // no HEAD yet (fresh repo): fall back to the unstaged diff
                    self.run(&["diff", "--no-color", "-U3"], "fallback");
                } else {
                    self.state = State::Failed(String::from_utf8_lossy(stderr).trim().to_string());
                }
                true
            }
            Event::Key(key) => self.key(key),
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
        match &self.state {
            State::Ready(p) => p.rows.len(),
            _ => 0,
        }
    }

    fn key(&mut self, key: &KeyWithModifier) -> bool {
        if key.key_modifiers.iter().any(|m| *m != KeyModifier::Shift) {
            return false;
        }
        let page = self.body.max(2) - 1;
        let shift = key.key_modifiers.contains(&KeyModifier::Shift);
        match key.bare_key {
            BareKey::Char(']') | BareKey::Char('N') => self.step_file(1),
            BareKey::Char('[') | BareKey::Char('P') => self.step_file(-1),
            BareKey::Tab => self.step_file(if shift { -1 } else { 1 }),
            BareKey::Esc | BareKey::Char('q') => close_self(),
            BareKey::Down | BareKey::Char('j') => self.scroll += 1,
            BareKey::Up | BareKey::Char('k') => self.scroll = self.scroll.saturating_sub(1),
            BareKey::PageDown | BareKey::Char(' ') | BareKey::Char('f') => self.scroll += page,
            BareKey::PageUp | BareKey::Char('b') => self.scroll = self.scroll.saturating_sub(page),
            BareKey::Home | BareKey::Char('g') => self.scroll = 0,
            BareKey::End | BareKey::Char('G') => self.scroll = usize::MAX / 2,
            BareKey::Right | BareKey::Char('l') => self.hscroll += HSTEP,
            BareKey::Left | BareKey::Char('h') => self.hscroll = self.hscroll.saturating_sub(HSTEP),
            BareKey::Char('n') => self.jump_hunk(true),
            BareKey::Char('p') => self.jump_hunk(false),
            _ => return false,
        }
        true
    }

    fn jump_hunk(&mut self, forward: bool) {
        let State::Ready(p) = &self.state else { return };
        let target = if forward {
            p.hunks.iter().find(|&&h| h > self.scroll)
        } else {
            p.hunks.iter().rev().find(|&&h| h < self.scroll)
        };
        if let Some(&h) = target {
            self.scroll = h;
        }
    }

    pub fn render(&mut self, _shared: &mut Shared, rows: usize, cols: usize) {
        let cols = cols.saturating_sub(1);
        let body = rows.saturating_sub(3);
        self.body = body;
        let total = self.rows();
        self.scroll = self.scroll.min(total.saturating_sub(body));

        let mut lines: Vec<Line> = Vec::new();
        let mut head = Line::new();
        head.push(&format!("{BOLD}{CYAN}"), &format!(" {}", ui::truncate_left(&self.path, cols.saturating_sub(24))));
        if let State::Ready(p) = &self.state {
            head.push("", "  ");
            head.push(GREEN, &format!("+{}", p.added));
            head.push("", " ");
            head.push(RED, &format!("-{}", p.removed));
            let mut info = String::new();
            if self.files.len() > 1 {
                info.push_str(&format!("file {}/{}  ", self.index + 1, self.files.len()));
            }
            if !p.hunks.is_empty() {
                let at = p.hunks.iter().filter(|&&h| h <= self.scroll).count().max(1);
                info.push_str(&format!("hunk {at}/{} ", p.hunks.len()));
            }
            head.right(DIM, &info, cols);
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
                for row in p.rows.iter().skip(self.scroll).take(body) {
                    lines.push(match row {
                        Row::Meta(text) => {
                            let mut l = Line::new();
                            l.push(&format!("{DIM}{CYAN}"), &ui::truncate(&format!(" {text}"), cols));
                            l
                        }
                        Row::Pair { left, right } => {
                            let mut l = Line::new();
                            self.cell(&mut l, left, gutter, half);
                            l.push(DIM, " │ ");
                            self.cell(&mut l, right, gutter, cols.saturating_sub(half + 3));
                            l
                        }
                    });
                }
            }
        }
        while lines.len() < rows.saturating_sub(1) {
            lines.push(Line::new());
        }
        let mut foot = Line::new();
        foot.push(DIM, " q close  j/k scroll  space/b page  n/p hunk  ]/[ next/prev file  h/l side  g/G top/end");
        lines.push(foot);
        ui::print_lines(&lines, rows, cols);
    }

    /// One half of a row: `<no><sign> <text>`, `width` columns wide.
    fn cell(&self, l: &mut Line, side: &Side, gutter: usize, width: usize) {
        let (style, sign) = match side.kind {
            Kind::Removed => (RED, '-'),
            Kind::Added => (GREEN, '+'),
            Kind::Context => ("", ' '),
            Kind::Empty => (DIM, ' '),
        };
        let no = side.no.map(|n| n.to_string()).unwrap_or_default();
        let prefix = format!("{no:>gutter$}{sign} ");
        let text_w = width.saturating_sub(ui::width(&prefix));
        let text = ui::slice_cols(&side.text, self.hscroll, text_w);
        let pad = text_w.saturating_sub(ui::width(&text));
        l.push(DIM, &prefix);
        l.push(style, &format!("{text}{}", " ".repeat(pad)));
    }
}

fn note(style: &str, text: &str) -> Line {
    let mut l = Line::new();
    l.push(style, text);
    l
}
