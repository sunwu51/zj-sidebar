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

/// One rendered row: styled text plus its visible (unstyled) width.
#[derive(Default, Clone)]
pub struct Line {
    pub text: String,
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
        self.text.push_str(style);
        self.text.push_str(s);
        self.text.push_str(RESET);
        self.width += width(s);
        self
    }
    /// Append `right` flush to the right edge of a `cols` wide row.
    pub fn right(&mut self, style: &str, right: &str, cols: usize) -> &mut Self {
        let rw = width(right);
        if self.width + rw + 1 <= cols {
            let gap = cols - self.width - rw;
            self.text.push_str(&" ".repeat(gap));
            self.width += gap;
            self.push(style, right);
        }
        self
    }
}

/// Print exactly `rows` lines. Lines are cleared with EL instead of padded, and the
/// last column is never written, so a width disagreement can't wrap into scrollback.
pub fn print_lines(lines: &[Line], rows: usize, _cols: usize) {
    // drop scrollback left over from reflowing earlier, wider renders after a resize
    let mut out = String::from("\x1b[3J");
    for i in 0..rows {
        if let Some(l) = lines.get(i) {
            out.push_str(&l.text);
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
