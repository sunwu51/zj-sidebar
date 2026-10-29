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
}
