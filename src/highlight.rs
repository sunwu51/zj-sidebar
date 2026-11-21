//! Syntax highlighting for the diff overlay: syntect parses each side of the file with the
//! Sublime syntaxes bundled by `two-face` (bat's set: Java, Kotlin, TypeScript, …), and the
//! scopes are boiled down to a handful of token kinds that the view paints with the
//! terminal's own 16 colors (no theme, so it follows the user's palette).

use std::sync::OnceLock;

use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

/// Files longer than this (per side) are shown without highlighting.
pub const MAX_LINES: usize = 8000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tok {
    Plain,
    Keyword,
    String,
    Comment,
    Number,
    Type,
    Function,
}

/// A highlighted byte range of a line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub tok: Tok,
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
pub fn syntax_for(path: &str) -> Option<&'static SyntaxReference> {
    let e = engine();
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit_once('.').map(|(_, x)| x);
    ext.and_then(|x| e.set.find_syntax_by_extension(x))
        .or_else(|| e.set.find_syntax_by_extension(name))
        .or_else(|| ext.and_then(|x| e.set.find_syntax_by_extension(&x.to_ascii_lowercase())))
}

/// Highlight `lines` (consecutive lines of one file); one list of spans per line.
pub fn highlight<'a>(syntax: &SyntaxReference, lines: impl Iterator<Item = &'a str>) -> Vec<Vec<Span>> {
    let e = engine();
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut out = Vec::new();
    for text in lines {
        let line = format!("{text}\n");
        let mut spans = Vec::new();
        let Ok(ops) = state.parse_line(&line, &e.set) else {
            out.push(spans);
            continue;
        };
        let mut at = 0;
        let push = |spans: &mut Vec<Span>, from: usize, to: usize, tok: Tok| {
            let to = to.min(text.len());
            if tok == Tok::Plain || from >= to {
                return;
            }
            match spans.last_mut() {
                Some(l) if l.end == from && l.tok == tok => l.end = to,
                _ => spans.push(Span { start: from, end: to, tok }),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(path: &str, src: &str) -> Vec<Vec<(String, Tok)>> {
        let syntax = syntax_for(path).unwrap_or_else(|| panic!("no syntax for {path}"));
        let lines: Vec<&str> = src.lines().collect();
        highlight(syntax, lines.iter().copied())
            .into_iter()
            .zip(&lines)
            .map(|(spans, l)| spans.iter().map(|s| (l[s.start..s.end].to_string(), s.tok)).collect())
            .collect()
    }

    fn has(line: &[(String, Tok)], text: &str, tok: Tok) -> bool {
        line.iter().any(|(t, k)| t.contains(text) && *k == tok)
    }

    #[test]
    fn finds_syntaxes_for_common_languages() {
        for f in [
            "A.java", "a.kt", "a.kts", "a.ts", "a.tsx", "a.js", "a.py", "a.rs", "a.go", "a.c", "a.cpp", "a.cs",
            "a.rb", "a.php", "a.swift", "a.toml", "a.yaml", "a.json", "a.sh", "a.sql", "a.html", "a.css",
            "a.md", "a.vue", "a.scala", "a.lua", "Makefile", "Dockerfile",
        ] {
            assert!(syntax_for(f).is_some(), "{f}");
        }
        assert!(syntax_for("notes.unknownext").is_none());
    }

    #[test]
    fn java_kotlin_typescript() {
        let l = toks("A.java", "public class A { // hi\n  String s = \"x\"; int n = 42;\n}");
        assert!(has(&l[0], "public", Tok::Keyword) && has(&l[0], "// hi", Tok::Comment));
        assert!(has(&l[1], "\"x\"", Tok::String) && has(&l[1], "42", Tok::Number));
        let l = toks("a.kt", "fun main() { val x = \"s\" }");
        assert!(has(&l[0], "fun", Tok::Keyword) && has(&l[0], "\"s\"", Tok::String));
        let l = toks("a.ts", "const x: number = 1; // c");
        assert!(has(&l[0], "const", Tok::Keyword) && has(&l[0], "// c", Tok::Comment));
    }

    #[test]
    fn multiline_state_carries_over() {
        let l = toks("a.rs", "/* a\nstill comment */ fn f() {}");
        assert!(has(&l[0], "/* a", Tok::Comment));
        assert!(has(&l[1], "still comment", Tok::Comment) && has(&l[1], "fn", Tok::Keyword));
    }
}
