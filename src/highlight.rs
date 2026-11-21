//! Syntax highlighting for the diff overlay. The coloring itself is done by the native
//! `zjsb-helper highlight` (a wasm plugin may not use more than 16 MiB, far too little to
//! compile the regexes of a syntax like TypeScript); this reads its answer. The kinds are
//! painted with the terminal's own 16 colors, so they follow the user's palette.

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

/// Lines beyond this are ignored (the helper never sends more than it highlights).
const MAX_LINE_NO: usize = 100_000;

fn tok(code: &str) -> Option<Tok> {
    Some(match code {
        "k" => Tok::Keyword,
        "s" => Tok::String,
        "c" => Tok::Comment,
        "n" => Tok::Number,
        "t" => Tok::Type,
        "f" => Tok::Function,
        _ => return None,
    })
}

/// Parse the helper's lines (`L|R <TAB> line <TAB> from-to:kind ...`) into the spans of
/// every line (index = line number - 1) of the old (0) and the new (1) version.
pub fn parse(out: &str) -> [Vec<Vec<Span>>; 2] {
    let mut sides: [Vec<Vec<Span>>; 2] = Default::default();
    for l in out.lines() {
        let mut f = l.splitn(3, '\t');
        let (Some(side), Some(no), Some(spans)) = (f.next(), f.next(), f.next()) else { continue };
        let side = match side {
            "L" => 0,
            "R" => 1,
            _ => continue,
        };
        let Some(no) = no.parse::<usize>().ok().filter(|n| (1..=MAX_LINE_NO).contains(n)) else { continue };
        let spans: Vec<Span> = spans
            .split(' ')
            .filter_map(|s| {
                let (range, kind) = s.split_once(':')?;
                let (a, b) = range.split_once('-')?;
                Some(Span { start: a.parse().ok()?, end: b.parse().ok()?, tok: tok(kind)? })
            })
            .collect();
        let v = &mut sides[side];
        if v.len() < no {
            v.resize(no, Vec::new());
        }
        v[no - 1] = spans;
    }
    sides
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_helper_output() {
        let h = parse("L\t2\t0-6:k 7-9:s\nR\t1\t3-4:c\nR\t3\t0-1:n\nbogus\nR\tx\t0-1:k\nR\t0\t0-1:k\n");
        assert_eq!(h[0].len(), 2);
        assert!(h[0][0].is_empty());
        assert_eq!(h[0][1], vec![Span { start: 0, end: 6, tok: Tok::Keyword }, Span { start: 7, end: 9, tok: Tok::String }]);
        assert_eq!(h[1].len(), 3);
        assert_eq!(h[1][0][0].tok, Tok::Comment);
        assert!(h[1][1].is_empty());
        assert_eq!(h[1][2][0].tok, Tok::Number);
    }

    #[test]
    fn ignores_absurd_line_numbers() {
        assert!(parse("R\t99999999999\t0-1:k\n")[1].is_empty());
    }
}
