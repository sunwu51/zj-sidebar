//! Popup widgets drawn over a mode's rows: context menus (also used as confirmations)
//! and a one-line text prompt.

use zellij_tile::prelude::{BareKey, KeyModifier, KeyWithModifier};

use crate::ui::{self, Line, BOLD, CYAN, DIM, REVERSE};

/// Plain keys and Shift+key only (typed capitals arrive with Shift).
pub fn plain(key: &KeyWithModifier) -> bool {
    key.key_modifiers.iter().all(|m| *m == KeyModifier::Shift)
}

#[derive(Clone)]
pub struct Item<A> {
    pub label: String,
    /// accelerator key shown on the right (also picks the item)
    pub key: Option<char>,
    /// `▸` marker: opens a submenu
    pub sub: bool,
    pub act: Option<A>,
    pub enabled: bool,
}

impl<A> Item<A> {
    pub fn new(label: impl Into<String>, act: A) -> Self {
        Item { label: label.into(), key: None, sub: false, act: Some(act), enabled: true }
    }
    pub fn sep() -> Self {
        Item { label: String::new(), key: None, sub: false, act: None, enabled: false }
    }
    pub fn key(mut self, c: char) -> Self {
        self.key = Some(c);
        self
    }
    pub fn sub(mut self) -> Self {
        self.sub = true;
        self
    }
    pub fn enabled(mut self, on: bool) -> Self {
        self.enabled = on;
        self
    }
    fn selectable(&self) -> bool {
        self.act.is_some() && self.enabled
    }
}

pub enum Outcome<A> {
    /// the key/click was used, nothing to do
    Handled,
    Close,
    /// go back to the parent menu
    Back,
    Pick(A),
}

pub struct Menu<A> {
    pub title: String,
    /// text shown above the items (questions of a confirmation)
    pub message: Vec<String>,
    pub items: Vec<Item<A>>,
    pub sel: usize,
    /// anchor (usually the click position); the box is moved to fit the screen
    pub anchor: (usize, usize),
    /// ignore the anchor: centered on the screen (dialogs)
    pub centered: bool,
    /// where the last render put it: x, y, w, h
    placed: (usize, usize, usize, usize),
    /// rows the message takes in the box
    msg_rows: usize,
}

impl<A: Clone> Menu<A> {
    pub fn new(title: impl Into<String>, items: Vec<Item<A>>) -> Self {
        let sel = items.iter().position(Item::selectable).unwrap_or(0);
        Menu {
            title: title.into(),
            message: Vec::new(),
            items,
            sel,
            anchor: (0, 0),
            centered: false,
            placed: (0, 0, 0, 0),
            msg_rows: 0,
        }
    }
    pub fn at(mut self, x: usize, y: usize) -> Self {
        self.anchor = (x, y);
        self
    }
    /// Where the box is on screen (after a render): its top-left corner.
    pub fn origin(&self) -> (usize, usize) {
        (self.placed.0, self.placed.1)
    }
    pub fn message(mut self, text: impl Into<String>) -> Self {
        self.message.push(text.into());
        self
    }

    fn step(&mut self, delta: isize) {
        let n = self.items.len() as isize;
        if !self.items.iter().any(Item::selectable) {
            return;
        }
        let mut i = self.sel as isize;
        loop {
            i = (i + delta).rem_euclid(n);
            if self.items[i as usize].selectable() {
                break;
            }
        }
        self.sel = i as usize;
    }

    fn pick(&self, i: usize) -> Outcome<A> {
        match self.items.get(i) {
            Some(it) if it.selectable() => Outcome::Pick(it.act.clone().unwrap()),
            _ => Outcome::Handled,
        }
    }

    pub fn key(&mut self, key: &KeyWithModifier) -> Outcome<A> {
        if !plain(key) {
            return Outcome::Handled;
        }
        // an item's accelerator wins over the vi-style navigation letters
        if let BareKey::Char(c) = key.bare_key {
            if let Some(i) = self.items.iter().position(|it| it.key == Some(c) && it.selectable()) {
                self.sel = i;
                return self.pick(i);
            }
        }
        match key.bare_key {
            BareKey::Down | BareKey::Char('j') | BareKey::Tab => self.step(1),
            BareKey::Up | BareKey::Char('k') => self.step(-1),
            BareKey::Enter | BareKey::Right | BareKey::Char('l') | BareKey::Char(' ') => return self.pick(self.sel),
            BareKey::Left | BareKey::Char('h') => return Outcome::Back,
            BareKey::Esc | BareKey::Char('q') => return Outcome::Close,
            _ => {}
        }
        Outcome::Handled
    }

    pub fn click(&mut self, line: usize, col: usize) -> Outcome<A> {
        let (x, y, w, h) = self.placed;
        if line < y || line >= y + h || col < x || col >= x + w {
            return Outcome::Close;
        }
        let row = line.saturating_sub(y + 1 + self.msg_rows);
        if line > y + self.msg_rows && row < self.items.len() {
            self.sel = row;
            return self.pick(row);
        }
        Outcome::Handled
    }

    pub fn hover(&mut self, line: usize, col: usize) -> bool {
        let (x, y, w, h) = self.placed;
        if line < y + 1 + self.msg_rows || line + 1 >= y + h || col < x || col >= x + w {
            return false;
        }
        let row = line - (y + 1 + self.msg_rows);
        if self.items.get(row).is_some_and(Item::selectable) && self.sel != row {
            self.sel = row;
            return true;
        }
        false
    }

    /// Draw over `lines` (a `cols` x `rows` screen).
    pub fn draw(&mut self, lines: &mut Vec<Line>, cols: usize, rows: usize) {
        let label_w = self.items.iter().map(|it| ui::width(&it.label)).max().unwrap_or(0);
        let right_w = self.items.iter().map(|it| if it.key.is_some() || it.sub { 3 } else { 0 }).max().unwrap_or(0);
        let msg_w = self.message.iter().map(|m| ui::width(m) + 2).max().unwrap_or(0);
        let w = (label_w + right_w + 4).max(ui::width(&self.title) + 6).max(msg_w.min(64) + 2).min(cols.saturating_sub(2)).max(12);
        let inner = w - 2;
        let mut body: Vec<Line> = Vec::new();
        for m in &self.message {
            for part in ui::wrap(m, inner.saturating_sub(2)) {
                body.push(ui::line("", &format!(" {part}")));
            }
        }
        if !self.message.is_empty() {
            body.push(Line::new());
        }
        self.msg_rows = body.len();
        for (i, it) in self.items.iter().enumerate() {
            let mut l = Line::new();
            if it.act.is_none() {
                l.push(DIM, &"─".repeat(inner));
                body.push(l);
                continue;
            }
            let style = if it.enabled { "" } else { DIM };
            l.push(style, &format!(" {}", ui::truncate(&it.label, inner.saturating_sub(right_w + 2))));
            let right = match (it.sub, it.key) {
                (true, _) => "▸ ".to_string(),
                (false, Some(k)) => format!("{k} "),
                _ => String::new(),
            };
            l.right(if it.enabled { CYAN } else { DIM }, &right, inner);
            let mut l = l.fit(inner);
            if i == self.sel && it.selectable() {
                l.restyle(REVERSE);
            }
            body.push(l);
        }
        let boxed = ui::boxed(&self.title, "", &body, w, CYAN);
        let h = boxed.len().min(rows);
        let (ax, ay) = if self.centered { (cols.saturating_sub(w) / 2, rows.saturating_sub(h) / 3) } else { self.anchor };
        let x = ax.min(cols.saturating_sub(w));
        let y = if ay + h <= rows { ay } else { rows.saturating_sub(h) };
        self.placed = (x, y, w, h);
        ui::overlay(lines, x, y, &boxed[..h]);
    }
}

pub enum PromptOutcome {
    Handled,
    Cancel,
    Submit(String),
}

/// One-line text input in a centered box.
pub struct Prompt<A> {
    pub title: String,
    pub label: String,
    pub text: String,
    /// cursor position in chars
    cursor: usize,
    pub act: A,
    pub error: Option<String>,
}

impl<A> Prompt<A> {
    pub fn new(title: impl Into<String>, label: impl Into<String>, initial: impl Into<String>, act: A) -> Self {
        let text: String = initial.into();
        Prompt { title: title.into(), label: label.into(), cursor: text.chars().count(), text, act, error: None }
    }

    fn byte(&self, ci: usize) -> usize {
        self.text.char_indices().nth(ci).map(|(b, _)| b).unwrap_or(self.text.len())
    }

    pub fn key(&mut self, key: &KeyWithModifier) -> PromptOutcome {
        let ctrl = key.key_modifiers.contains(&KeyModifier::Ctrl);
        match key.bare_key {
            BareKey::Esc => return PromptOutcome::Cancel,
            BareKey::Enter => return PromptOutcome::Submit(self.text.trim().to_string()),
            BareKey::Backspace if self.cursor > 0 => {
                let b = self.byte(self.cursor - 1);
                self.text.remove(b);
                self.cursor -= 1;
            }
            BareKey::Delete if self.cursor < self.text.chars().count() => {
                let b = self.byte(self.cursor);
                self.text.remove(b);
            }
            BareKey::Left => self.cursor = self.cursor.saturating_sub(1),
            BareKey::Right => self.cursor = (self.cursor + 1).min(self.text.chars().count()),
            BareKey::Home => self.cursor = 0,
            BareKey::End => self.cursor = self.text.chars().count(),
            BareKey::Char('u') if ctrl => {
                self.text.clear();
                self.cursor = 0;
            }
            BareKey::Char('a') if ctrl => self.cursor = 0,
            BareKey::Char('e') if ctrl => self.cursor = self.text.chars().count(),
            BareKey::Char(c) if plain(key) => {
                let b = self.byte(self.cursor);
                self.text.insert(b, c);
                self.cursor += 1;
            }
            _ => {}
        }
        self.error = None;
        PromptOutcome::Handled
    }

    pub fn draw(&self, lines: &mut Vec<Line>, cols: usize, rows: usize) {
        let w = (ui::width(&self.title) + 8).max(56).min(cols.saturating_sub(2)).max(16);
        let inner = w - 2;
        let field = inner.saturating_sub(2);
        // keep the cursor visible in a long text
        let chars: Vec<char> = self.text.chars().collect();
        let before: String = chars[..self.cursor].iter().collect();
        let skip = ui::width(&before).saturating_sub(field.saturating_sub(1));
        let mut input = Line::new();
        input.push("", " ");
        let shown = ui::slice_cols(&self.text, skip, field);
        let cur = ui::width(&before) - skip;
        let left = ui::slice_cols(&shown, 0, cur);
        let at: String = shown[left.len()..].chars().next().map(String::from).unwrap_or_else(|| " ".into());
        let rest = &shown[(left.len() + if shown.len() > left.len() { at.len() } else { 0 }).min(shown.len())..];
        input.push(BOLD, &left);
        input.push(REVERSE, &at);
        input.push(BOLD, rest);
        let mut body = vec![ui::line(DIM, &format!(" {}", self.label)), input];
        if let Some(e) = &self.error {
            body.push(ui::line(crate::ui::RED, &format!(" {}", ui::truncate(e, inner.saturating_sub(2)))));
        }
        let boxed = ui::boxed(&self.title, "Enter ok · Esc cancel", &body, w, CYAN);
        let x = cols.saturating_sub(w) / 2;
        let y = rows.saturating_sub(boxed.len()) / 3;
        ui::overlay(lines, x, y, &boxed);
    }
}
