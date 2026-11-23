//! Floating "recent sessions" window (`mode "recent"`): the newest Claude Code and Codex
//! sessions of any directory, newest first, as `CX dir title`. Enter (or a click) jumps to the
//! pane that already runs the session, else resumes it in a new tab in its own directory.
//! The list comes from `zjsb-helper recent` (see the helper's `recent.rs`).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde_json::Value;
use zellij_tile::prelude::*;

use crate::sessions::now_secs;
use crate::ui::{self, Line, BOLD, CYAN, DIM, GREEN, RED, REVERSE, YELLOW};
use crate::widgets::plain;
use crate::Shared;

const COUNT: usize = 100;
const MAX_DIR_COLS: usize = 22;

struct Entry {
    codex: bool,
    id: String,
    title: String,
    cwd: String,
    /// unix seconds
    updated: u64,
}

impl Entry {
    fn dir(&self) -> &str {
        match ui::basename(&self.cwd) {
            "" => &self.cwd,
            b => b,
        }
    }
}

#[derive(Default)]
pub struct RecentWin {
    helper: String,
    entries: Vec<Entry>,
    /// indexes into `entries` that pass the filter
    shown: Vec<usize>,
    filter: String,
    typing: bool,
    sel: usize,
    scroll: usize,
    loading: bool,
    status: Option<(String, bool)>,
    /// screen row -> index into `shown`
    rows_at: Vec<(usize, usize)>,
    body_h: usize,
    /// pane id -> shell pid, as the helper's `running` wants them
    pane_pids: HashMap<u32, u32>,
    opened: bool,
}

fn parse(out: &str) -> Vec<Entry> {
    out.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| {
            let s = |k: &str| v.get(k).and_then(Value::as_str).map(String::from);
            Some(Entry {
                codex: s("kind")? == "codex",
                id: s("id")?,
                title: s("title")?,
                cwd: s("cwd")?,
                updated: v.get("updated_ms")?.as_u64()? / 1000,
            })
        })
        .collect()
}

impl RecentWin {
    pub fn load(&mut self, shared: &Shared) {
        self.helper = shared.cfg("helper").unwrap_or("").to_string();
        self.loading = true;
    }

    fn fetch(&self) {
        let n = COUNT.to_string();
        run_command(&[self.helper.as_str(), "recent", n.as_str()], BTreeMap::from([("recent".to_string(), "1".to_string())]));
    }

    fn refilter(&mut self) {
        let f = self.filter.to_lowercase();
        let keep = self.selected().map(|e| e.id.clone());
        self.shown = (0..self.entries.len())
            .filter(|&i| {
                let e = &self.entries[i];
                f.is_empty() || e.title.to_lowercase().contains(&f) || e.cwd.to_lowercase().contains(&f)
            })
            .collect();
        self.sel = keep.and_then(|id| self.shown.iter().position(|&i| self.entries[i].id == id)).unwrap_or(0);
    }

    fn selected(&self) -> Option<&Entry> {
        self.shown.get(self.sel).map(|&i| &self.entries[i])
    }

    fn error(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), true));
    }

    /// Ask the helper which panes run which sessions; the answer decides how to open `id`.
    fn open(&mut self, shared: &Shared) {
        let Some(e) = self.selected() else { return };
        let id = e.id.clone();
        let mut pids = Vec::new();
        for p in shared.panes.panes.values().flatten().filter(|p| !p.is_plugin && !p.exited) {
            let pid = self.pane_pids.get(&p.id).copied().or_else(|| {
                get_pane_pid(PaneId::Terminal(p.id)).ok().and_then(|pid| u32::try_from(pid).ok())
            });
            if let Some(pid) = pid {
                self.pane_pids.insert(p.id, pid);
                pids.push(pid.to_string());
            }
        }
        self.status = Some(("…".into(), false));
        let mut cmd = vec![self.helper.as_str(), "running"];
        cmd.extend(pids.iter().map(String::as_str));
        run_command(&cmd, BTreeMap::from([("running".to_string(), "1".to_string()), ("open".to_string(), id)]));
    }

    fn on_running(&mut self, shared: &Shared, stdout: &str, id: &str) {
        let pane_of: HashMap<u32, u32> = self.pane_pids.iter().map(|(pane, pid)| (*pid, *pane)).collect();
        let pane = stdout
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find(|v| v.get("id").and_then(Value::as_str) == Some(id))
            .and_then(|v| pane_of.get(&u32::try_from(v.get("root")?.as_u64()?).ok()?).copied());
        if let Some(pane) = pane {
            focus_pane_with_id(PaneId::Terminal(pane), false, false);
            close_self();
            return;
        }
        self.resume(shared, id);
    }

    /// A new tab in the session's directory that runs the resume command (and stays open).
    fn resume(&mut self, shared: &Shared, id: &str) {
        if self.opened {
            return; // a double click while the first request is carried out
        }
        let Some(e) = self.entries.iter().find(|e| e.id == id) else { return };
        let cmd = if e.codex {
            format!("{} resume {}", shared.cfg("codex_cmd").unwrap_or("codex"), e.id)
        } else {
            format!("{} --resume {}", shared.cfg("claude_cmd").unwrap_or("claude"), e.id)
        };
        let env = get_session_environment_variables();
        let shell = shared
            .cfg("shell")
            .map(String::from)
            .or_else(|| env.get("SHELL").filter(|s| !s.is_empty()).cloned())
            .unwrap_or_else(|| if env.get("OS").is_some_and(|os| os == "Windows_NT") { "pwsh".into() } else { "sh".into() });
        let (path, args) = ui::shell_command(&shell, &cmd);
        let cwd = PathBuf::from(&e.cwd).is_dir().then(|| PathBuf::from(&e.cwd));
        let (tab, _) = open_command_pane_in_new_tab(CommandToRun { path: path.into(), args, cwd }, BTreeMap::new());
        match tab {
            Some(_) => {
                self.opened = true;
                close_self();
            }
            None => self.error("failed to open a new tab"),
        }
    }

    fn move_sel(&mut self, delta: isize) {
        if self.shown.is_empty() {
            return;
        }
        self.sel = (self.sel as isize + delta).clamp(0, self.shown.len() as isize - 1) as usize;
    }

    pub fn update(&mut self, shared: &mut Shared, event: &Event) -> bool {
        match event {
            Event::PermissionRequestResult(PermissionStatus::Granted) => {
                if self.helper.is_empty() {
                    self.helper = crate::panel::default_helper(&get_session_environment_variables());
                }
                self.fetch();
                true
            }
            Event::RunCommandResult(code, stdout, stderr, ctx) => {
                let out = String::from_utf8_lossy(stdout);
                if ctx.contains_key("recent") {
                    self.loading = false;
                    if *code == Some(0) {
                        self.entries = parse(&out);
                        self.status = None;
                        self.refilter();
                    } else {
                        let e = String::from_utf8_lossy(stderr);
                        self.error(format!("helper: {}", e.lines().next().unwrap_or("failed")));
                    }
                    return true;
                }
                if ctx.contains_key("running") {
                    // a failed lookup only means we can't tell: resume anyway
                    let id = ctx.get("open").cloned().unwrap_or_default();
                    let out = if *code == Some(0) { out.into_owned() } else { String::new() };
                    self.on_running(shared, &out, &id);
                    return true;
                }
                false
            }
            Event::Key(key) => self.key(shared, key),
            Event::Mouse(m) => self.mouse(shared, m),
            _ => false,
        }
    }

    fn key(&mut self, shared: &Shared, key: &KeyWithModifier) -> bool {
        if !plain(key) {
            return false;
        }
        self.status = None;
        if self.typing {
            match key.bare_key {
                BareKey::Esc => {
                    self.typing = false;
                    self.filter.clear();
                    self.refilter();
                }
                BareKey::Enter => {
                    self.typing = false;
                    self.open(shared);
                }
                BareKey::Backspace => {
                    self.filter.pop();
                    self.refilter();
                }
                BareKey::Down => self.move_sel(1),
                BareKey::Up => self.move_sel(-1),
                BareKey::Char(c) => {
                    self.filter.push(c);
                    self.refilter();
                }
                _ => {}
            }
            return true;
        }
        let page = self.body_h.max(2) as isize - 1;
        match key.bare_key {
            BareKey::Down | BareKey::Char('j') => self.move_sel(1),
            BareKey::Up | BareKey::Char('k') => self.move_sel(-1),
            BareKey::PageDown | BareKey::Char(' ') => self.move_sel(page),
            BareKey::PageUp => self.move_sel(-page),
            BareKey::Home | BareKey::Char('g') => self.move_sel(isize::MIN / 2),
            BareKey::End | BareKey::Char('G') => self.move_sel(isize::MAX / 2),
            BareKey::Enter => self.open(shared),
            BareKey::Char('/') => self.typing = true,
            BareKey::Char('r') => {
                self.loading = true;
                self.fetch();
            }
            BareKey::Esc | BareKey::Char('q') => {
                if self.filter.is_empty() {
                    close_self();
                } else {
                    self.filter.clear();
                    self.refilter();
                }
            }
            _ => return false,
        }
        true
    }

    fn mouse(&mut self, shared: &Shared, m: &Mouse) -> bool {
        match *m {
            Mouse::LeftClick(line, _) => {
                let line = line.max(0) as usize;
                let Some(&(_, i)) = self.rows_at.iter().find(|(l, _)| *l == line) else { return false };
                self.sel = i;
                self.open(shared);
                true
            }
            Mouse::ScrollUp(_) => {
                self.move_sel(-3);
                true
            }
            Mouse::ScrollDown(_) => {
                self.move_sel(3);
                true
            }
            _ => false,
        }
    }

    pub fn render(&mut self, _shared: &mut Shared, rows: usize, cols: usize) {
        let w = cols.saturating_sub(1);
        let mut lines: Vec<Line> = Vec::new();
        let mut head = Line::new();
        head.push(&format!("{BOLD}{CYAN}"), " RECENT SESSIONS");
        head.push(DIM, &format!(" {}", self.shown.len()));
        if self.typing || !self.filter.is_empty() {
            head.push(DIM, "   ⌕ ");
            head.push(BOLD, &ui::truncate_left(&self.filter, 40));
            if self.typing {
                head.push(REVERSE, " ");
            }
        }
        lines.push(head.fit(w));

        let body_h = rows.saturating_sub(2);
        self.body_h = body_h;
        if self.sel < self.scroll {
            self.scroll = self.sel;
        } else if body_h > 0 && self.sel >= self.scroll + body_h {
            self.scroll = self.sel + 1 - body_h;
        }
        self.rows_at.clear();
        if self.shown.is_empty() {
            let msg = if self.loading { "  …" } else if self.filter.is_empty() { "  no sessions" } else { "  no match" };
            lines.push(ui::line(DIM, msg));
        }
        let dir_w = self.shown.iter().map(|&i| ui::width(self.entries[i].dir())).max().unwrap_or(0).min(MAX_DIR_COLS);
        let now = now_secs();
        for (row, &i) in self.shown.iter().enumerate().skip(self.scroll).take(body_h) {
            let e = &self.entries[i];
            let sel = row == self.sel;
            let (tag, style) = if e.codex { ("CX", CYAN) } else { ("CC", YELLOW) };
            let age = format!(" {} ", ui::age(now.saturating_sub(e.updated)));
            let mut l = Line::new();
            l.push(if sel { GREEN } else { "" }, if sel { "▌" } else { " " });
            l.push(style, tag);
            l.push("", " ");
            l.push(GREEN, &format!("{:<dir_w$}", ui::truncate(e.dir(), dir_w)));
            l.push("", "  ");
            l.push(if sel { BOLD } else { "" }, &ui::truncate(&e.title, w.saturating_sub(l.width + age.len())));
            l.right(DIM, &age, w);
            let mut l = l.fit(w);
            if sel {
                l.restyle(REVERSE);
            }
            self.rows_at.push((lines.len(), row));
            lines.push(l);
        }
        while lines.len() + 1 < rows {
            lines.push(Line::new());
        }
        let foot = match &self.status {
            Some((m, err)) => ui::line(if *err { RED } else { DIM }, &format!(" {}", ui::truncate(m, w.saturating_sub(2)))),
            None => ui::line(DIM, " j/k move  Enter open  / filter  r reload  q close   (click a row to open)"),
        };
        lines.push(foot);
        ui::print_lines(&lines, rows, cols);
    }
}
