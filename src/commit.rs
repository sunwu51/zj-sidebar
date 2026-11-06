//! Commit dialog (after IntelliJ IDEA's Commit tool window): the changed files with
//! checkboxes, "amend" and "push after commit" options, and a multi-line message.
//! `GitCtl` owns it and turns a submit into git commands.

use zellij_tile::prelude::{BareKey, KeyModifier, KeyWithModifier};

use crate::git::GitFile;
use crate::ui::{self, Line, BLUE, BOLD, CYAN, DIM, GREEN, RED, REVERSE, YELLOW};
use crate::widgets::plain;

pub struct Entry {
    pub file: GitFile,
    pub on: bool,
}

pub enum CommitOut {
    Handled,
    Close,
    Submit,
}

#[derive(Default)]
pub struct CommitDlg {
    pub branch: String,
    pub entries: Vec<Entry>,
    pub loaded: bool,
    pub msg: String,
    /// cursor in the message, in chars
    cursor: usize,
    /// the keyboard is in the message (else in the list of files and options)
    in_msg: bool,
    /// row of the list: the files, then "amend", then "push"
    sel: usize,
    scroll: usize,
    pub amend: bool,
    pub push: bool,
    /// what "push" would push to, for the label
    pub upstream: Option<String>,
    /// HEAD's message, offered when amending
    pub head_msg: Option<String>,
    pub error: Option<String>,
    pub busy: bool,
    /// where the last render put the box, the screen line of each list row, and
    /// the screen lines of the message
    placed: (usize, usize, usize, usize),
    row_lines: Vec<(usize, usize)>,
    msg_lines: (usize, usize),
}

impl CommitDlg {
    pub fn new(branch: String, draft: String, upstream: Option<String>) -> Self {
        let cursor = draft.chars().count();
        CommitDlg { branch, msg: draft, cursor, in_msg: true, upstream, ..Default::default() }
    }

    pub fn set_files(&mut self, files: Vec<GitFile>) {
        self.entries = files.into_iter().map(|file| Entry { file, on: true }).collect();
        self.loaded = true;
        self.sel = self.sel.min(self.rows().saturating_sub(1));
    }

    fn rows(&self) -> usize {
        self.entries.len() + 2
    }

    pub fn selected_files(&self) -> Vec<&GitFile> {
        self.entries.iter().filter(|e| e.on).map(|e| &e.file).collect()
    }

    fn toggle(&mut self, row: usize) {
        let n = self.entries.len();
        if row < n {
            self.entries[row].on = !self.entries[row].on;
        } else if row == n {
            self.amend = !self.amend;
            // amending starts from the commit's message, as in IDEA
            if self.amend && self.msg.trim().is_empty() {
                if let Some(m) = &self.head_msg {
                    self.msg = m.clone();
                    self.cursor = self.msg.chars().count();
                }
            }
        } else if row == n + 1 {
            self.push = !self.push;
        }
        self.error = None;
    }

    fn toggle_all(&mut self) {
        let on = !self.entries.iter().all(|e| e.on);
        for e in &mut self.entries {
            e.on = on;
        }
    }

    // -- the message editor ---------------------------------------------------------

    fn byte(&self, ci: usize) -> usize {
        self.msg.char_indices().nth(ci).map(|(b, _)| b).unwrap_or(self.msg.len())
    }

    fn insert(&mut self, c: char) {
        let b = self.byte(self.cursor);
        self.msg.insert(b, c);
        self.cursor += 1;
        self.error = None;
    }

    /// (line, column) of the cursor, in chars.
    fn cursor_pos(&self) -> (usize, usize) {
        let before: String = self.msg.chars().take(self.cursor).collect();
        let line = before.matches('\n').count();
        let col = before.rsplit('\n').next().map(|l| l.chars().count()).unwrap_or(0);
        (line, col)
    }

    /// Move the cursor to (line, column), clamped to the text.
    fn set_cursor(&mut self, line: usize, col: usize) {
        let mut ci = 0;
        for (i, l) in self.msg.split('\n').enumerate() {
            let len = l.chars().count();
            if i == line {
                self.cursor = ci + col.min(len);
                return;
            }
            ci += len + 1;
        }
        self.cursor = self.msg.chars().count();
    }

    fn msg_key(&mut self, key: &KeyWithModifier) -> CommitOut {
        let alt = key.key_modifiers.contains(&KeyModifier::Alt);
        let (line, col) = self.cursor_pos();
        let lines = self.msg.split('\n').count();
        match key.bare_key {
            BareKey::Enter if alt => self.insert('\n'),
            // no Alt/Option key (macOS default terminals): a trailing `\` + Enter is a new line
            BareKey::Enter if self.cursor > 0 && self.msg.chars().nth(self.cursor - 1) == Some('\\') => {
                let b = self.byte(self.cursor - 1);
                self.msg.remove(b);
                self.cursor -= 1;
                self.insert('\n');
            }
            BareKey::Enter => return CommitOut::Submit,
            BareKey::Backspace if self.cursor > 0 => {
                let b = self.byte(self.cursor - 1);
                self.msg.remove(b);
                self.cursor -= 1;
            }
            BareKey::Delete if self.cursor < self.msg.chars().count() => {
                let b = self.byte(self.cursor);
                self.msg.remove(b);
            }
            BareKey::Left => self.cursor = self.cursor.saturating_sub(1),
            BareKey::Right => self.cursor = (self.cursor + 1).min(self.msg.chars().count()),
            BareKey::Home => self.set_cursor(line, 0),
            BareKey::End => self.set_cursor(line, usize::MAX),
            BareKey::Up if line == 0 => {
                // above the message: the options and files
                self.in_msg = false;
                self.sel = self.rows().saturating_sub(1);
            }
            BareKey::Up => self.set_cursor(line - 1, col),
            BareKey::Down if line + 1 < lines => self.set_cursor(line + 1, col),
            BareKey::Char(c) if plain(key) => self.insert(c),
            _ => {}
        }
        CommitOut::Handled
    }

    fn list_key(&mut self, key: &KeyWithModifier) -> CommitOut {
        if !plain(key) {
            return CommitOut::Handled;
        }
        let n = self.rows();
        match key.bare_key {
            BareKey::Up | BareKey::Char('k') => self.sel = self.sel.saturating_sub(1),
            BareKey::Down | BareKey::Char('j') if self.sel + 1 >= n => self.in_msg = true,
            BareKey::Down | BareKey::Char('j') => self.sel += 1,
            BareKey::Char(' ') => self.toggle(self.sel),
            BareKey::Char('a') => self.toggle_all(),
            BareKey::Enter => return CommitOut::Submit,
            // anything else typed goes to the message
            BareKey::Char(c) => {
                self.in_msg = true;
                self.cursor = self.msg.chars().count();
                self.insert(c);
            }
            _ => {}
        }
        CommitOut::Handled
    }

    pub fn key(&mut self, key: &KeyWithModifier) -> CommitOut {
        if self.busy {
            return CommitOut::Handled;
        }
        let ctrl = key.key_modifiers.contains(&KeyModifier::Ctrl);
        match key.bare_key {
            BareKey::Esc => return CommitOut::Close,
            BareKey::Tab => {
                self.in_msg = !self.in_msg;
                return CommitOut::Handled;
            }
            // clear the whole message, wherever the keyboard is
            BareKey::Char('u') if ctrl => {
                self.msg.clear();
                self.cursor = 0;
                self.in_msg = true;
                self.error = None;
                return CommitOut::Handled;
            }
            _ => {}
        }
        if self.in_msg { self.msg_key(key) } else { self.list_key(key) }
    }

    pub fn click(&mut self, line: usize, col: usize) -> CommitOut {
        let (x, y, w, h) = self.placed;
        if self.busy || line < y || line >= y + h || col < x || col >= x + w {
            return CommitOut::Handled;
        }
        if let Some(&(_, row)) = self.row_lines.iter().find(|(l, _)| *l == line) {
            self.in_msg = false;
            self.sel = row;
            self.toggle(row);
        } else if line >= self.msg_lines.0 && line <= self.msg_lines.1 {
            self.in_msg = true;
        }
        CommitOut::Handled
    }

    pub fn scroll(&mut self, delta: isize) {
        if !self.in_msg {
            self.sel = (self.sel as isize + delta).clamp(0, self.rows() as isize - 1) as usize;
        }
    }

    // -- drawing ---------------------------------------------------------------

    pub fn draw(&mut self, lines: &mut Vec<Line>, cols: usize, rows: usize) {
        let w = cols.saturating_sub(2).min(78).max(24);
        let inner = w - 2;
        let n = self.entries.len();
        let msg_rows: Vec<&str> = self.msg.split('\n').collect();
        let msg_h = msg_rows.len().clamp(3, 8);
        // border 2, header 1, separators 2, options 2, message label 1, message, status 1
        let fixed = 2 + 1 + 2 + 2 + 1 + msg_h + 1;
        let files_h = n.max(1).min(rows.saturating_sub(fixed + 1)).max(1);

        let mut body: Vec<Line> = Vec::new();
        let mut row_at: Vec<(usize, usize)> = Vec::new(); // (body index, list row)
        let on = self.entries.iter().filter(|e| e.on).count();
        let mut head = Line::new();
        let active = format!("{BOLD}{CYAN}");
        head.push(if self.in_msg { DIM } else { &active }, " Changes");
        head.push(DIM, &format!("  {on}/{n} selected"));
        head.right(DIM, "Space toggle · a all ", inner);
        body.push(head);

        if !self.loaded {
            body.push(ui::line(DIM, "  loading…"));
        } else if n == 0 {
            body.push(ui::line(DIM, "  no local changes"));
        }
        if self.sel < n {
            if self.sel < self.scroll {
                self.scroll = self.sel;
            } else if self.sel >= self.scroll + files_h {
                self.scroll = self.sel + 1 - files_h;
            }
        }
        self.scroll = self.scroll.min(n.saturating_sub(files_h));
        for (i, e) in self.entries.iter().enumerate().skip(self.scroll).take(files_h) {
            let style = match e.file.code.chars().next() {
                Some('A') => GREEN,
                Some('D') => RED,
                Some('R') | Some('C') => BLUE,
                Some('?') => CYAN,
                _ => YELLOW,
            };
            let mut l = Line::new();
            l.push(if e.on { GREEN } else { DIM }, if e.on { " [x] " } else { " [ ] " });
            l.push(style, &format!("{:<2} ", e.file.code));
            l.push(if e.on { "" } else { DIM }, &ui::truncate_left(&e.file.path, inner.saturating_sub(9)));
            let mut l = l.fit(inner);
            if !self.in_msg && self.sel == i {
                l.restyle(REVERSE);
            }
            row_at.push((body.len(), i));
            body.push(l);
        }
        body.push(ui::line(DIM, &"─".repeat(inner)));
        let push_label = match &self.upstream {
            Some(u) => format!("Push after commit (→ {u})"),
            None => "Push after commit".to_string(),
        };
        for (k, (label, val)) in [("Amend last commit".to_string(), self.amend), (push_label, self.push)].into_iter().enumerate() {
            let mut l = Line::new();
            l.push(if val { GREEN } else { DIM }, if val { " [x] " } else { " [ ] " });
            l.push("", &ui::truncate(&label, inner.saturating_sub(6)));
            let mut l = l.fit(inner);
            if !self.in_msg && self.sel == n + k {
                l.restyle(REVERSE);
            }
            row_at.push((body.len(), n + k));
            body.push(l);
        }
        body.push(ui::line(DIM, &"─".repeat(inner)));
        let mut label = Line::new();
        label.push(if self.in_msg { &active } else { DIM }, " Commit message");
        label.right(DIM, "Alt+Enter or \\+Enter: new line ", inner);
        body.push(label);

        // the message, scrolled so the cursor's line shows; the cursor in reverse video
        let (cl, cc) = self.cursor_pos();
        let first = cl.saturating_sub(msg_h - 1);
        let msg_start = body.len();
        let field = inner.saturating_sub(2);
        for (i, text) in msg_rows.iter().enumerate().skip(first).take(msg_h) {
            let mut l = Line::new();
            l.push("", " ");
            if self.in_msg && i == cl {
                let chars: Vec<char> = text.chars().collect();
                let before: String = chars[..cc.min(chars.len())].iter().collect();
                let skip = ui::width(&before).saturating_sub(field.saturating_sub(1));
                let shown = ui::slice_cols(text, skip, field);
                let cur = ui::width(&before) - skip;
                let left = ui::slice_cols(&shown, 0, cur);
                let rest = &shown[left.len()..];
                let mut it = rest.chars();
                let at = it.next().map(String::from).unwrap_or_else(|| " ".into());
                l.push("", &left);
                l.push(REVERSE, &at);
                l.push("", it.as_str());
            } else {
                l.push(if i == 0 { BOLD } else { "" }, &ui::slice_cols(text, 0, field));
            }
            body.push(l);
        }
        while body.len() < msg_start + msg_h {
            body.push(Line::new());
        }
        let msg_end = body.len() - 1;
        let status = if self.busy {
            ui::line(DIM, " committing…")
        } else if let Some(e) = &self.error {
            ui::line(RED, &format!(" {}", ui::truncate(e, inner.saturating_sub(2))))
        } else if self.msg.trim().is_empty() && self.amend {
            ui::line(DIM, " empty message: keep the commit's message")
        } else if !self.msg.is_empty() {
            ui::line(DIM, " Ctrl+U clears the message")
        } else {
            Line::new()
        };
        body.push(status);

        let title = format!("Commit to '{}'", ui::truncate(&self.branch, 40));
        let boxed = ui::boxed(&title, "Tab files/message · Enter commit · Esc close", &body, w, CYAN);
        let h = boxed.len().min(rows);
        let x = cols.saturating_sub(w) / 2;
        let y = rows.saturating_sub(h) / 3;
        self.placed = (x, y, w, h);
        self.row_lines = row_at.into_iter().map(|(bi, r)| (y + 1 + bi, r)).collect();
        self.msg_lines = (y + 1 + msg_start, y + 1 + msg_end);
        ui::overlay(lines, x, y, &boxed[..h]);
    }
}
