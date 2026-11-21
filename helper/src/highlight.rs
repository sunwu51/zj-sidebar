//! `zjsb-helper size|highlight`: what the diff overlay needs about the two versions of a
//! file. A wasm plugin may not use more than 16 MiB, which is too little to compile the
//! regexes of a syntax like TypeScript (~80 MB), so the highlighting happens here.
//!
//! A version is given as `git:<rev>:<path>` (`git show`), `file:<path>` (the work tree,
//! relative to the directory) or `none`.
//!
//! `size <cwd> <old> <new>` prints the byte sizes of both versions: `<old> <new>`.
//!
//! `highlight <cwd> <path> <old> <new>` prints one line per highlighted source line:
//! `<L|R>\t<line number>\t<from>-<to>:<kind> ...` (byte offsets into the line as the
//! plugin shows it: no `\r`, tabs as 4 spaces; kinds: k keyword, s string, c comment,
//! n number, t type, f function). L is the old version, R the new one.

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

/// Lines per version that get highlighted (the parser is sequential, so it is the head
/// of the file that is accurate).
const MAX_LINES: usize = 8000;
/// Bytes of output; the rest is left out.
const MAX_OUTPUT: usize = 1 << 20;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tok {
    Plain,
    Keyword,
    String,
    Comment,
    Number,
    Type,
    Function,
}

impl Tok {
    fn code(self) -> char {
        match self {
            Tok::Plain => ' ',
            Tok::Keyword => 'k',
            Tok::String => 's',
            Tok::Comment => 'c',
            Tok::Number => 'n',
            Tok::Type => 't',
            Tok::Function => 'f',
        }
    }
}

struct Engine {
    set: SyntaxSet,
    /// innermost scope that matches a rule decides; `Plain` stops the search (operators)
    rules: Vec<(Scope, Tok)>,
}

fn engine() -> &'static Engine {
    static E: OnceLock<Engine> = OnceLock::new();
    E.get_or_init(|| {
        use Tok::*;
        let table: &[(&str, Tok)] = &[
            ("comment", Comment),
            ("string", String),
            ("constant.numeric", Number),
            ("constant.language", Number),
            ("constant.character", String),
            ("constant", Number),
            ("keyword.operator", Plain),
            ("keyword", Keyword),
            ("storage", Keyword),
            ("variable.language", Keyword),
            ("entity.name.tag", Keyword),
            ("entity.name.function", Function),
            ("support.function", Function),
            ("variable.function", Function),
            ("entity.name.type", Type),
            ("entity.name.class", Type),
            ("entity.name.struct", Type),
            ("entity.name.enum", Type),
            ("entity.name.trait", Type),
            ("entity.name.interface", Type),
            ("entity.other.inherited-class", Type),
            ("support.type", Type),
            ("support.class", Type),
        ];
        Engine {
            set: two_face::syntax::extra_newlines(),
            rules: table.iter().filter_map(|(s, t)| Scope::new(s).ok().map(|s| (s, *t))).collect(),
        }
    })
}

impl Engine {
    fn tok(&self, stack: &ScopeStack) -> Tok {
        for scope in stack.as_slice().iter().rev() {
            if let Some((_, t)) = self.rules.iter().find(|(r, _)| r.is_prefix_of(*scope)) {
                return *t;
            }
        }
        Tok::Plain
    }
}

/// The syntax for a file, by extension and then by whole name (`Makefile`, `Dockerfile`).
fn syntax_for(path: &str) -> Option<&'static SyntaxReference> {
    let e = engine();
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.').map(|(_, x)| x);
    ext.and_then(|x| e.set.find_syntax_by_extension(x))
        .or_else(|| e.set.find_syntax_by_extension(name))
        .or_else(|| ext.and_then(|x| e.set.find_syntax_by_extension(&x.to_ascii_lowercase())))
}

/// Highlight consecutive lines of one file: per line, `(from, to, kind)` byte ranges.
fn spans_of<'a>(syntax: &SyntaxReference, lines: impl Iterator<Item = &'a str>) -> Vec<Vec<(usize, usize, Tok)>> {
    let e = engine();
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut out = Vec::new();
    for text in lines {
        let line = format!("{text}\n");
        let mut spans: Vec<(usize, usize, Tok)> = Vec::new();
        let Ok(ops) = state.parse_line(&line, &e.set) else {
            out.push(spans);
            continue;
        };
        let mut at = 0;
        let push = |spans: &mut Vec<(usize, usize, Tok)>, from: usize, to: usize, tok: Tok| {
            let to = to.min(text.len());
            if tok == Tok::Plain || from >= to {
                return;
            }
            match spans.last_mut() {
                Some(l) if l.1 == from && l.2 == tok => l.1 = to,
                _ => spans.push((from, to, tok)),
            }
        };
        for (pos, op) in ops {
            push(&mut spans, at, pos, e.tok(&stack));
            at = pos;
            if stack.apply(&op).is_err() {
                break;
            }
        }
        push(&mut spans, at, text.len(), e.tok(&stack));
        out.push(spans);
    }
    out
}

/// The content of a version (empty when it doesn't exist).
fn read(cwd: &str, spec: &str) -> Vec<u8> {
    if let Some(rest) = spec.strip_prefix("git:") {
        let Some((rev, path)) = rest.split_once(':') else { return Vec::new() };
        return Command::new("git")
            .args(["--no-optional-locks", "show", &format!("{rev}:{path}")])
            .current_dir(cwd)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| o.stdout)
            .unwrap_or_default();
    }
    if let Some(path) = spec.strip_prefix("file:") {
        return std::fs::read(Path::new(cwd).join(path)).unwrap_or_default();
    }
    Vec::new()
}

/// Bytes of a version, without reading it when git can tell.
fn size_of(cwd: &str, spec: &str) -> u64 {
    if let Some(rest) = spec.strip_prefix("git:") {
        return Command::new("git")
            .args(["--no-optional-locks", "cat-file", "-s", rest])
            .current_dir(cwd)
            .output()
            .ok()
            .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
            .unwrap_or(0);
    }
    if let Some(path) = spec.strip_prefix("file:") {
        return std::fs::metadata(Path::new(cwd).join(path)).map(|m| m.len()).unwrap_or(0);
    }
    0
}

pub fn size(cwd: &str, old: &str, new: &str) -> Result<(), String> {
    println!("{} {}", size_of(cwd, old), size_of(cwd, new));
    Ok(())
}

pub fn highlight(cwd: &str, path: &str, old: &str, new: &str) -> Result<(), String> {
    let Some(syntax) = syntax_for(path) else { return Ok(()) };
    let mut out = String::new();
    for (side, spec) in [('L', old), ('R', new)] {
        let bytes = read(cwd, spec);
        if bytes.contains(&0) {
            continue; // binary
        }
        let text = String::from_utf8_lossy(&bytes);
        // the lines as the plugin shows them
        let lines: Vec<String> = text.lines().take(MAX_LINES).map(|l| l.trim_end_matches('\r').replace('\t', "    ")).collect();
        for (i, spans) in spans_of(syntax, lines.iter().map(String::as_str)).iter().enumerate() {
            if spans.is_empty() {
                continue;
            }
            let _ = write!(out, "{side}\t{}\t", i + 1);
            for (n, (from, to, tok)) in spans.iter().enumerate() {
                let _ = write!(out, "{}{from}-{to}:{}", if n > 0 { " " } else { "" }, tok.code());
            }
            out.push('\n');
        }
        if out.len() > MAX_OUTPUT {
            break;
        }
    }
    print!("{out}");
    Ok(())
}
