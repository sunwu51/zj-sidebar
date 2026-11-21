//! Turns `git diff` (unified format) into rows for a side-by-side view.
//! Std only, so it can be tested on the host: `rustc --test --edition 2024 src/diffparse.rs`.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Context,
    Removed,
    Added,
    Empty,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Side {
    pub no: Option<usize>,
    pub text: String,
    pub kind: Kind,
}

impl Side {
    fn empty() -> Self {
        Side { no: None, text: String::new(), kind: Kind::Empty }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Row {
    /// hunk header or a notice, spanning both columns
    Meta(String),
    Pair { left: Side, right: Side },
}

impl Row {
    /// an added/removed line (or a notice such as "binary file differs")
    pub fn changed(&self) -> bool {
        match self {
            Row::Meta(_) => true,
            Row::Pair { left, right } => left.kind != Kind::Context || right.kind != Kind::Context,
        }
    }
}

#[derive(Default, Debug)]
pub struct Parsed {
    pub rows: Vec<Row>,
    /// indexes into `rows` of the hunk headers
    pub hunks: Vec<usize>,
    pub added: usize,
    pub removed: usize,
    pub binary: bool,
}

fn clean(text: &str) -> String {
    text.trim_end_matches('\r').replace('\t', "    ")
}

/// `@@ -12,5 +14,7 @@ fn name` -> (12, 14)
fn hunk_start(header: &str) -> Option<(usize, usize)> {
    let rest = header.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(' ')?;
    let new = rest.strip_prefix('+')?.split(' ').next()?;
    let num = |s: &str| s.split(',').next()?.parse::<usize>().ok();
    Some((num(old)?, num(new)?))
}

pub fn parse(diff: &str) -> Parsed {
    let mut p = Parsed::default();
    let (mut old_no, mut new_no) = (0usize, 0usize);
    let mut in_hunk = false;
    let mut del: Vec<Side> = Vec::new();
    let mut add: Vec<Side> = Vec::new();

    fn flush(p: &mut Parsed, del: &mut Vec<Side>, add: &mut Vec<Side>) {
        for i in 0..del.len().max(add.len()) {
            let left = del.get(i).cloned().unwrap_or_else(Side::empty);
            let right = add.get(i).cloned().unwrap_or_else(Side::empty);
            p.rows.push(Row::Pair { left, right });
        }
        del.clear();
        add.clear();
    }

    for line in diff.lines() {
        if line.starts_with("@@ ") {
            flush(&mut p, &mut del, &mut add);
            if let Some((o, n)) = hunk_start(line) {
                old_no = o;
                new_no = n;
                in_hunk = true;
                p.hunks.push(p.rows.len());
                p.rows.push(Row::Meta(line.to_string()));
            }
            continue;
        }
        if !in_hunk {
            if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
                p.binary = true;
                p.rows.push(Row::Meta("binary file differs".to_string()));
            }
            continue; // diff --git / index / --- / +++ / mode lines
        }
        match line.as_bytes().first() {
            Some(b'-') => {
                if !add.is_empty() {
                    flush(&mut p, &mut del, &mut add);
                }
                del.push(Side { no: Some(old_no), text: clean(&line[1..]), kind: Kind::Removed });
                old_no += 1;
                p.removed += 1;
            }
            Some(b'+') => {
                add.push(Side { no: Some(new_no), text: clean(&line[1..]), kind: Kind::Added });
                new_no += 1;
                p.added += 1;
            }
            Some(b'\\') => {} // "\ No newline at end of file"
            _ => {
                flush(&mut p, &mut del, &mut add);
                let text = clean(line.get(1..).unwrap_or(""));
                p.rows.push(Row::Pair {
                    left: Side { no: Some(old_no), text: text.clone(), kind: Kind::Context },
                    right: Side { no: Some(new_no), text, kind: Kind::Context },
                });
                old_no += 1;
                new_no += 1;
            }
        }
    }
    flush(&mut p, &mut del, &mut add);
    p
}

// -- folding ------------------------------------------------------------------------
//
// The diff is requested with (practically) unlimited context, so every line of the file is
// in `rows`. Runs of unchanged lines far from any change are folded into a `Gap` that the
// user can open step by step.

/// Lines of context kept around every change.
pub const CONTEXT: usize = 3;

/// A run of unchanged rows, `order[start..end]`, that is folded by default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gap {
    pub start: usize,
    pub end: usize,
    /// something (a change) comes before / after the gap
    pub above: bool,
    pub below: bool,
    /// rows revealed below the preceding change / above the following one
    pub top: usize,
    pub bottom: usize,
}

impl Gap {
    pub fn hidden(&self) -> usize {
        (self.end - self.start).saturating_sub(self.top + self.bottom)
    }

    /// Reveal `n` more rows below the change above the gap.
    pub fn open_down(&mut self, n: usize) {
        self.top += n.min(self.hidden());
    }

    /// Reveal `n` more rows above the change below the gap.
    pub fn open_up(&mut self, n: usize) {
        self.bottom += n.min(self.hidden());
    }

    pub fn open_all(&mut self) {
        self.top = self.end - self.start;
        self.bottom = 0;
    }

    /// Make the row at `pos` (`start <= pos < end`) visible, with `ctx` rows around it.
    pub fn reveal(&mut self, pos: usize, ctx: usize) {
        let len = self.end - self.start;
        let from_top = pos - self.start + 1 + ctx;
        let from_bottom = self.end - pos + ctx;
        let (top, bottom) = if self.above && (!self.below || from_top <= from_bottom) {
            (self.top.max(from_top), self.bottom)
        } else {
            (self.top, self.bottom.max(from_bottom))
        };
        if top + bottom >= len {
            self.open_all();
        } else {
            self.top = top;
            self.bottom = bottom;
        }
    }
}

#[derive(Default, Debug)]
pub struct Folds {
    /// indexes into `Parsed::rows` of the rows that take part (hunk headers do not)
    pub order: Vec<usize>,
    pub gaps: Vec<Gap>,
    /// positions in `order` where a hunk (changes plus their context) starts
    pub starts: Vec<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    /// index into `Parsed::rows`
    Row(usize),
    /// index into `Folds::gaps`: the "N lines hidden" row
    Fold(usize),
}

pub fn folds(p: &Parsed, ctx: usize) -> Folds {
    // one hunk (the whole file) needs no header; several (a context-limited diff) do
    let headers = p.hunks.len() > 1;
    let order: Vec<usize> = (0..p.rows.len())
        .filter(|&i| headers || !matches!(&p.rows[i], Row::Meta(t) if t.starts_with("@@ ")))
        .collect();
    let n = order.len();
    let changed: Vec<bool> = order.iter().map(|&i| p.rows[i].changed()).collect();
    let mut keep = vec![false; n];
    for i in (0..n).filter(|&i| changed[i]) {
        for k in keep.iter_mut().take((i + ctx + 1).min(n)).skip(i.saturating_sub(ctx)) {
            *k = true;
        }
    }
    let mut f = Folds { order, ..Default::default() };
    let mut i = 0;
    while i < n {
        let mut j = i;
        while j < n && keep[j] == keep[i] {
            j += 1;
        }
        if keep[i] {
            f.starts.push(i);
        } else {
            f.gaps.push(Gap { start: i, end: j, above: i > 0, below: j < n, top: 0, bottom: 0 });
        }
        i = j;
    }
    f
}

/// The lines to show, and where (in them) each hunk starts.
pub fn layout(f: &Folds) -> (Vec<Item>, Vec<usize>) {
    let mut items = Vec::with_capacity(f.order.len());
    let mut hunks = Vec::with_capacity(f.starts.len());
    let mut starts = f.starts.iter().peekable();
    let mut gaps = f.gaps.iter().enumerate().peekable();
    let mut pos = 0;
    while pos < f.order.len() {
        if let Some(&(g, gap)) = gaps.peek() {
            if gap.start == pos {
                let rows = |a: usize, b: usize| (a..b).map(|k| Item::Row(f.order[k]));
                items.extend(rows(gap.start, gap.start + gap.top));
                if gap.hidden() > 0 {
                    items.push(Item::Fold(g));
                }
                items.extend(rows(gap.end - gap.bottom, gap.end));
                pos = gap.end;
                gaps.next();
                continue;
            }
        }
        if starts.peek() == Some(&&pos) {
            hunks.push(items.len());
            starts.next();
        }
        items.push(Item::Row(f.order[pos]));
        pos += 1;
    }
    (items, hunks)
}

// -- search -------------------------------------------------------------------------

fn lower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Byte ranges of the case-insensitive occurrences of `needle` in `text`.
pub fn find_all(text: &str, needle: &str) -> Vec<(usize, usize)> {
    let n: Vec<char> = needle.chars().map(lower).collect();
    let mut out = Vec::new();
    if n.is_empty() {
        return out;
    }
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut i = 0;
    while i + n.len() <= chars.len() {
        if chars[i..i + n.len()].iter().zip(&n).all(|(&(_, c), &m)| lower(c) == m) {
            let end = chars.get(i + n.len()).map_or(text.len(), |c| c.0);
            out.push((chars[i].0, end));
            i += n.len();
        } else {
            i += 1;
        }
    }
    out
}

/// Indexes into `rows` of the rows with `needle` on either side.
pub fn search(p: &Parsed, needle: &str) -> Vec<usize> {
    if needle.is_empty() {
        return Vec::new();
    }
    let hit = |s: &Side| !find_all(&s.text, needle).is_empty();
    p.rows
        .iter()
        .enumerate()
        .filter(|(_, r)| match r {
            Row::Meta(_) => false,
            Row::Pair { left, right } => hit(left) || hit(right),
        })
        .map(|(i, _)| i)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "diff --git a/f.rs b/f.rs
index 111..222 100644
--- a/f.rs
+++ b/f.rs
@@ -1,4 +1,5 @@ fn main
 keep
-old one
-old two
+new one
+new two
+new three
 tail
@@ -20,2 +21,1 @@
-gone
 end
";

    fn pair(row: &Row) -> (&Side, &Side) {
        match row {
            Row::Pair { left, right } => (left, right),
            _ => panic!("not a pair: {row:?}"),
        }
    }

    #[test]
    fn aligns_changes_side_by_side() {
        let p = parse(SAMPLE);
        assert_eq!((p.added, p.removed), (3, 3));
        assert_eq!(p.hunks, vec![0, 6]);
        assert_eq!(p.rows[0], Row::Meta("@@ -1,4 +1,5 @@ fn main".into()));
        let (l, r) = pair(&p.rows[1]);
        assert_eq!((l.no, r.no, l.kind), (Some(1), Some(1), Kind::Context));
        let (l, r) = pair(&p.rows[2]);
        assert_eq!((l.text.as_str(), r.text.as_str()), ("old one", "new one"));
        assert_eq!((l.no, r.no), (Some(2), Some(2)));
        let (l, r) = pair(&p.rows[4]);
        assert_eq!((l.kind, r.kind, r.text.as_str(), r.no), (Kind::Empty, Kind::Added, "new three", Some(4)));
        let (l, r) = pair(&p.rows[5]);
        assert_eq!((l.text.as_str(), l.no, r.no), ("tail", Some(4), Some(5)));
    }

    #[test]
    fn second_hunk_uses_its_own_numbers() {
        let p = parse(SAMPLE);
        assert_eq!(p.rows[6], Row::Meta("@@ -20,2 +21,1 @@".into()));
        let (l, r) = pair(&p.rows[7]);
        assert_eq!((l.kind, l.no, r.kind), (Kind::Removed, Some(20), Kind::Empty));
        let (l, r) = pair(&p.rows[8]);
        assert_eq!((l.no, r.no), (Some(21), Some(21)));
    }

    #[test]
    fn new_file_and_tabs_and_crlf() {
        let p = parse("diff --git a/x b/x\nnew file mode 100644\n--- /dev/null\n+++ b/x\n@@ -0,0 +1,2 @@\n+a\tb\r\n+c\n\\ No newline at end of file\n");
        assert_eq!(p.rows.len(), 3);
        let (l, r) = pair(&p.rows[1]);
        assert_eq!((l.kind, r.text.as_str()), (Kind::Empty, "a    b"));
    }

    #[test]
    fn binary_and_empty() {
        let p = parse("diff --git a/i.png b/i.png\nBinary files a/i.png and b/i.png differ\n");
        assert!(p.binary);
        assert!(parse("").rows.is_empty());
    }

    #[test]
    fn removal_after_addition_starts_a_new_block() {
        let p = parse("@@ -1,2 +1,2 @@\n-a\n+b\n-c\n+d\n");
        let texts: Vec<_> = p.rows[1..].iter().map(|r| { let (l, r) = pair(r); (l.text.clone(), r.text.clone()) }).collect();
        assert_eq!(texts, vec![("a".to_string(), "b".to_string()), ("c".to_string(), "d".to_string())]);
    }

    #[test]
    fn folds_far_context_and_opens_step_by_step() {
        // 20 rows, changes at rows 3 and 18 (1-based): context 3 keeps rows 1..=6 and 15..=20
        let mut d = String::from("@@ -1,20 +1,20 @@\n");
        for i in 1..=20 {
            match i {
                3 | 18 => d.push_str(&format!("-old{i}\n+new{i}\n")),
                _ => d.push_str(&format!(" l{i}\n")),
            }
        }
        let p = parse(&d);
        let mut f = folds(&p, 3);
        assert_eq!(f.starts, vec![0, 14]);
        assert_eq!(f.gaps.len(), 1);
        assert_eq!((f.gaps[0].start, f.gaps[0].end, f.gaps[0].hidden()), (6, 14, 8));
        let (items, hunks) = layout(&f);
        assert_eq!(items.len(), 6 + 1 + 6);
        assert_eq!(items[6], Item::Fold(0));
        assert_eq!(hunks, vec![0, 7]);
        f.gaps[0].open_down(2);
        f.gaps[0].open_up(2);
        assert_eq!(f.gaps[0].hidden(), 4);
        assert_eq!(layout(&f).0.len(), 6 + 2 + 1 + 2 + 6);
        f.gaps[0].open_down(50);
        assert_eq!(f.gaps[0].hidden(), 0);
        assert!(!layout(&f).0.contains(&Item::Fold(0)));
        assert_eq!(layout(&f).0.len(), 20);
    }

    #[test]
    fn edges_fold_towards_the_change() {
        let mut d = String::from("@@ -1,30 +1,30 @@\n");
        for i in 1..=30 {
            if i == 15 { d.push_str("-a\n+b\n") } else { d.push_str(&format!(" l{i}\n")) }
        }
        let p = parse(&d);
        let f = folds(&p, 3);
        assert_eq!(f.gaps.len(), 2);
        assert!(!f.gaps[0].above && f.gaps[0].below);
        assert!(f.gaps[1].above && !f.gaps[1].below);
        let mut g = f.gaps[0];
        g.reveal(9, 3);
        assert_eq!((g.top, g.bottom), (0, 5));
    }

    #[test]
    fn finds_text_case_insensitively() {
        assert_eq!(find_all("Foo foo FOO", "foo"), vec![(0, 3), (4, 7), (8, 11)]);
        assert_eq!(find_all("héllo", "LLO"), vec![(3, 6)]);
        assert!(find_all("abc", "").is_empty());
        let p = parse("@@ -1,2 +1,2 @@\n-hello\n+world\n tail\n");
        assert_eq!(search(&p, "WORLD"), vec![1]);
        assert_eq!(search(&p, "tail"), vec![2]);
    }

    #[test]
    fn context_limited_diffs_keep_their_hunk_headers() {
        let multi = folds(&parse(SAMPLE), 3);
        assert_eq!(multi.order.len(), 9); // 2 headers + 7 rows
        let single = folds(&parse("@@ -1,2 +1,2 @@\n-a\n+b\n c\n"), 3);
        assert_eq!(single.order.len(), 2); // the lone header is not shown
    }
}
