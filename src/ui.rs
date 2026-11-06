//! Small text/ANSI helpers shared by both modes.
//! Colors use the 16-color ANSI palette so they follow the terminal theme.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const RED: &str = "\x1b[31m";
pub const GREEN: &str = "\x1b[32m";
pub const YELLOW: &str = "\x1b[33m";
pub const BLUE: &str = "\x1b[34m";
pub const MAGENTA: &str = "\x1b[35m";
pub const CYAN: &str = "\x1b[36m";
pub const REVERSE: &str = "\x1b[7m";

pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Cut `s` to at most `max` columns, adding an ellipsis when cut.
pub fn truncate(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > max - 1 {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// The part of `s` that is `width` columns wide, starting `skip` columns in. A wide
/// character cut by either edge becomes a space.
pub fn slice_cols(s: &str, skip: usize, width: usize) -> String {
    let mut out = String::new();
    let (mut col, mut used) = (0usize, 0usize);
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        let start = col;
        col += cw;
        if col <= skip {
            continue;
        }
        if used + cw > width {
            break;
        }
        if start < skip {
            out.push_str(&" ".repeat(col - skip));
            used += col - skip;
        } else {
            out.push(c);
            used += cw;
        }
    }
    out
}

/// Cut `s` from the left, keeping the tail (useful for file paths).
pub fn truncate_left(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut tail: Vec<char> = Vec::new();
    let mut w = 0;
    for c in s.chars().rev() {
        let cw = c.width().unwrap_or(0);
        if w + cw > max - 1 {
            break;
        }
        tail.push(c);
        w += cw;
    }
    tail.reverse();
    let mut out = String::from("…");
    out.extend(tail);
    out
}

/// One rendered row: styled segments plus their visible (unstyled) width. Keeping the
/// segments (instead of one escaped string) lets rows be cut, padded and drawn over.
#[derive(Default, Clone)]
pub struct Line {
    segs: Vec<(String, String)>,
    pub width: usize,
}

impl Line {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn push(&mut self, style: &str, s: &str) -> &mut Self {
        if s.is_empty() {
            return self;
        }
        self.width += width(s);
        self.segs.push((style.to_string(), s.to_string()));
        self
    }
    /// Append `right` flush to the right edge of a `cols` wide row.
    pub fn right(&mut self, style: &str, right: &str, cols: usize) -> &mut Self {
        let rw = width(right);
        if self.width + rw + 1 <= cols {
            let gap = cols - self.width - rw;
            self.push("", &" ".repeat(gap));
            self.push(style, right);
        }
        self
    }
    pub fn append(&mut self, other: &Line) -> &mut Self {
        for (st, t) in &other.segs {
            self.push(st, t);
        }
        self
    }
    /// Pad with spaces (in `style`) up to `w` columns.
    pub fn pad(&mut self, w: usize, style: &str) -> &mut Self {
        if self.width < w {
            self.push(style, &" ".repeat(w - self.width));
        }
        self
    }
    /// `w` columns of this row starting `skip` columns in.
    pub fn slice(&self, skip: usize, w: usize) -> Line {
        let mut out = Line::new();
        let mut col = 0;
        for (st, t) in &self.segs {
            let tw = width(t);
            let (from, to) = (col, col + tw);
            col = to;
            if to <= skip {
                continue;
            }
            if out.width >= w {
                break;
            }
            let start = skip.saturating_sub(from);
            out.push(st, &slice_cols(t, start, w - out.width));
        }
        out
    }
    /// Exactly `w` columns: cut, then padded.
    pub fn fit(&self, w: usize) -> Line {
        let mut l = self.slice(0, w);
        l.pad(w, "");
        l
    }
    /// Draw `top` over this row starting at column `at`.
    pub fn overlay(&mut self, at: usize, top: &Line) {
        let mut out = self.slice(0, at);
        out.pad(at, "");
        out.append(top);
        out.append(&self.slice(at + top.width, usize::MAX / 2));
        *self = out;
    }
    /// Prefix every segment with `style` (e.g. reverse video for a selected row).
    pub fn restyle(&mut self, style: &str) -> &mut Self {
        for (st, _) in &mut self.segs {
            *st = format!("{style}{st}");
        }
        self
    }
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (st, t) in &self.segs {
            if st.is_empty() {
                out.push_str(t);
            } else {
                out.push_str(st);
                out.push_str(t);
                out.push_str(RESET);
            }
        }
        out
    }
}

pub fn line(style: &str, text: &str) -> Line {
    let mut l = Line::new();
    l.push(style, text);
    l
}

/// Wrap `text` into rows of at most `w` columns (breaking at spaces when possible).
pub fn wrap(text: &str, w: usize) -> Vec<String> {
    let w = w.max(1);
    let mut out = Vec::new();
    for para in text.lines() {
        let mut cur = String::new();
        for word in para.split(' ') {
            let ww = width(word);
            if !cur.is_empty() && width(&cur) + 1 + ww > w {
                out.push(std::mem::take(&mut cur));
            }
            if ww > w {
                // a single overlong word: hard-break it
                let mut rest = word.to_string();
                while width(&rest) > w {
                    let mut head = slice_cols(&rest, 0, w);
                    if head.is_empty() {
                        head = rest.chars().next().map(String::from).unwrap_or_default();
                    }
                    rest = rest[head.len()..].to_string();
                    out.push(head);
                }
                cur = rest;
                continue;
            }
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(word);
        }
        out.push(cur);
    }
    out
}

/// A bordered box `w` columns wide around `body` rows, with a title in the top border
/// and an optional hint in the bottom one.
pub fn boxed(title: &str, hint: &str, body: &[Line], w: usize, border: &str) -> Vec<Line> {
    let inner = w.saturating_sub(2);
    let mut out = Vec::with_capacity(body.len() + 2);
    let mut top = Line::new();
    let t = truncate(title, inner.saturating_sub(3));
    top.push(border, "╭─");
    if !t.is_empty() {
        top.push(&format!("{BOLD}"), &format!(" {t} "));
    }
    let used = top.width;
    top.push(border, &format!("{}╮", "─".repeat(w.saturating_sub(used + 1))));
    out.push(top);
    for b in body {
        let mut l = Line::new();
        l.push(border, "│");
        l.append(&b.fit(inner));
        l.push(border, "│");
        out.push(l);
    }
    let mut bot = Line::new();
    let h = truncate(hint, inner.saturating_sub(3));
    bot.push(border, "╰─");
    if !h.is_empty() {
        bot.push(DIM, &format!(" {h} "));
    }
    let used = bot.width;
    bot.push(border, &format!("{}╯", "─".repeat(w.saturating_sub(used + 1))));
    out.push(bot);
    out
}

/// Draw `popup` rows over `lines` at (`x`, `y`), growing `lines` if needed.
pub fn overlay(lines: &mut Vec<Line>, x: usize, y: usize, popup: &[Line]) {
    for (i, p) in popup.iter().enumerate() {
        while lines.len() <= y + i {
            lines.push(Line::new());
        }
        lines[y + i].overlay(x, p);
    }
}

/// Print exactly `rows` lines. Lines are cleared with EL instead of padded, and the
/// last column is never written, so a width disagreement can't wrap into scrollback.
pub fn print_lines(lines: &[Line], rows: usize, _cols: usize) {
    // drop scrollback left over from reflowing earlier, wider renders after a resize
    let mut out = String::from("\x1b[3J");
    for i in 0..rows {
        if let Some(l) = lines.get(i) {
            out.push_str(&l.render());
        }
        out.push_str("\x1b[K");
        if i + 1 < rows {
            out.push('\n');
        }
    }
    print!("{out}");
}

pub fn basename(path: &str) -> &str {
    let t = path.trim_end_matches(['/', '\\']);
    t.rsplit(['/', '\\']).next().filter(|s| !s.is_empty()).unwrap_or(t)
}

/// Normalize a path for comparison. Windows paths (`C:\..`, `\\..`) are case-insensitive
/// with unified separators; everything else is compared as is.
pub fn norm_path(path: &str) -> String {
    let windows = path.starts_with(r"\\") || path.as_bytes().get(1) == Some(&b':');
    if windows {
        let mut p = path.replace('/', "\\").to_lowercase();
        while p.len() > 3 && p.ends_with('\\') {
            p.pop();
        }
        p
    } else {
        let t = path.trim_end_matches('/');
        if t.is_empty() { "/".to_string() } else { t.to_string() }
    }
}

pub fn age(secs: u64) -> String {
    match secs {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        86_400..=2_591_999 => format!("{}d", secs / 86_400),
        _ => format!("{}mo", secs / 2_592_000),
    }
}

const SHELLS: &[&str] = &[
    "pwsh", "powershell", "cmd", "bash", "zsh", "fish", "nu", "sh", "dash", "ksh", "tcsh", "elvish",
    "xonsh", "login",
];

pub fn is_shell(argv0: &str) -> bool {
    let name = basename(argv0).trim_start_matches('-').to_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    SHELLS.contains(&name)
}

/// Program and arguments that run `cmd` through `shell` and then leave that shell open,
/// so a command pane started this way stays usable after the command exits.
pub fn shell_command(shell: &str, cmd: &str) -> (String, Vec<String>) {
    let name = basename(shell).trim_start_matches('-').to_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name).to_string();
    let args: Vec<String> = match name.as_str() {
        "pwsh" | "powershell" => vec!["-NoLogo".into(), "-NoExit".into(), "-Command".into(), cmd.into()],
        "cmd" => vec!["/k".into(), cmd.into()],
        "nu" => vec!["-e".into(), cmd.into()],
        // bash, zsh, fish, sh, ...: run it in an interactive shell, then stay in a new one
        _ => vec!["-ic".into(), format!("{cmd}; exec '{}'", shell.replace('\'', "'\\''"))],
    };
    (shell.to_string(), args)
}
