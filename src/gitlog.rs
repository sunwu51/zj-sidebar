//! Parsing of `git log`, `git for-each-ref` and `git diff --raw --numstat -z` output, and
//! the lane layout of the commit graph. Std only, so it can be tested on the host:
//! `rustc --test --edition 2024 src/gitlog.rs`.

use std::collections::HashSet;

/// `git log` format matching `parse_log`: one record per commit, fields separated by 0x1f.
pub const LOG_FORMAT: &str = "--format=%x1e%H%x1f%P%x1f%an%x1f%ae%x1f%ad%x1f%at%x1f%D%x1f%s%x1f%b";
pub const LOG_DATE: &str = "--date=format:%Y-%m-%d %H:%M";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Commit {
    pub sha: String,
    pub parents: Vec<String>,
    pub author: String,
    pub email: String,
    pub date: String,
    pub time: u64,
    /// raw decorations (`HEAD -> main`, `origin/main`, `tag: v1`)
    pub decorations: Vec<String>,
    pub subject: String,
    pub body: String,
}

impl Commit {
    pub fn short(&self) -> &str {
        &self.sha[..self.sha.len().min(8)]
    }
}

pub fn parse_log(out: &str) -> Vec<Commit> {
    out.split('\u{1e}')
        .filter_map(|rec| {
            let mut f = rec.split('\u{1f}');
            let sha = f.next()?.trim().to_string();
            if sha.is_empty() {
                return None;
            }
            let parents = f.next()?.split_whitespace().map(String::from).collect();
            let author = f.next()?.to_string();
            let email = f.next()?.to_string();
            let date = f.next()?.to_string();
            let time = f.next()?.trim().parse().unwrap_or(0);
            let decorations = f
                .next()?
                .split(", ")
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .map(String::from)
                .collect();
            let subject = f.next()?.to_string();
            let body = f.next().unwrap_or("").trim().to_string();
            Some(Commit { sha, parents, author, email, date, time, decorations, subject, body })
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefKind {
    /// `HEAD` itself (detached)
    Head,
    /// the checked-out branch
    Current,
    Local,
    Remote,
    Tag,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefLabel {
    pub kind: RefKind,
    pub name: String,
}

/// Turn `%D` decorations into labels; `locals` tells local branches from remote ones.
pub fn classify(decorations: &[String], locals: &HashSet<String>) -> Vec<RefLabel> {
    let mut out = Vec::new();
    for d in decorations {
        let (kind, name) = if let Some(b) = d.strip_prefix("HEAD -> ") {
            (RefKind::Current, b)
        } else if d == "HEAD" {
            (RefKind::Head, "HEAD")
        } else if let Some(t) = d.strip_prefix("tag: ") {
            (RefKind::Tag, t)
        } else if locals.contains(d.as_str()) {
            (RefKind::Local, d.as_str())
        } else if d.ends_with("/HEAD") {
            continue; // origin/HEAD: noise
        } else {
            (RefKind::Remote, d.as_str())
        };
        out.push(RefLabel { kind, name: name.to_string() });
    }
    out
}

// -- branches -----------------------------------------------------------------------

pub const BRANCH_FORMAT: &str = "--format=%(refname)%1f%(objectname)%1f%(upstream:short)%1f%(upstream:track)%1f%(HEAD)%1f%(committerdate:unix)%1f%(contents:subject)";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Branch {
    /// `main` or `origin/main`
    pub name: String,
    pub remote: bool,
    pub sha: String,
    pub upstream: Option<String>,
    pub ahead: u64,
    pub behind: u64,
    /// the upstream was deleted
    pub gone: bool,
    pub head: bool,
    pub time: u64,
    pub subject: String,
}

pub fn parse_branches(out: &str) -> Vec<Branch> {
    let mut v: Vec<Branch> = out
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\u{1f}').collect();
            if f.len() < 7 {
                return None;
            }
            let (name, remote) = if let Some(n) = f[0].strip_prefix("refs/heads/") {
                (n, false)
            } else if let Some(n) = f[0].strip_prefix("refs/remotes/") {
                if n.ends_with("/HEAD") {
                    return None;
                }
                (n, true)
            } else {
                return None;
            };
            let mut b = Branch {
                name: name.to_string(),
                remote,
                sha: f[1].to_string(),
                upstream: Some(f[2].to_string()).filter(|u| !u.is_empty()),
                head: f[4].trim() == "*",
                time: f[5].trim().parse().unwrap_or(0),
                subject: f[6..].join("\u{1f}"),
                ..Default::default()
            };
            let track = f[3].trim().trim_start_matches('[').trim_end_matches(']');
            for part in track.split(", ") {
                if let Some(n) = part.strip_prefix("ahead ") {
                    b.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix("behind ") {
                    b.behind = n.parse().unwrap_or(0);
                } else if part == "gone" {
                    b.gone = true;
                }
            }
            Some(b)
        })
        .collect();
    // current branch first, then the most recently committed to (like IDEA's "recent")
    v.sort_by(|a, b| b.head.cmp(&a.head).then(b.time.cmp(&a.time)));
    v
}

/// `origin/feature/x` -> (`origin`, `feature/x`), using the known remote names.
pub fn split_remote<'a>(name: &'a str, remotes: &[String]) -> Option<(&'a str, &'a str)> {
    let mut best: Option<(&str, &str)> = None;
    for r in remotes {
        if let Some(rest) = name.strip_prefix(r.as_str()).and_then(|s| s.strip_prefix('/')) {
            if best.is_none_or(|(b, _)| r.len() > b.len()) {
                best = Some((&name[..r.len()], rest));
            }
        }
    }
    best.or_else(|| name.split_once('/'))
}

// -- changed files of a commit ----------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChangedFile {
    /// M, A, D, R, C, T
    pub code: String,
    pub path: String,
    /// the source path of a rename/copy
    pub old: Option<String>,
    /// (added, deleted); None for binary files
    pub stats: Option<(u64, u64)>,
}

/// Parse `git diff --raw --numstat -z` (raw entries first, then numstat entries).
pub fn parse_changes(out: &str) -> Vec<ChangedFile> {
    let mut files: Vec<ChangedFile> = Vec::new();
    let mut it = out.split('\0').peekable();
    while let Some(tok) = it.next() {
        let tok = tok.trim_start_matches('\n');
        if tok.is_empty() {
            continue;
        }
        if let Some(raw) = tok.strip_prefix(':') {
            // :100644 100644 abc def M   (then path, or old+new for R/C)
            let status = raw.split_whitespace().last().unwrap_or("M");
            let code: String = status.chars().take(1).collect();
            let first = it.next().unwrap_or("").to_string();
            if code == "R" || code == "C" {
                let new = it.next().unwrap_or("").to_string();
                files.push(ChangedFile { code, path: new, old: Some(first), stats: None });
            } else {
                files.push(ChangedFile { code, path: first, old: None, stats: None });
            }
            continue;
        }
        // numstat: "a\td\tpath" or "a\td\t" followed by old, new
        let mut f = tok.splitn(3, '\t');
        let (Some(a), Some(d), Some(rest)) = (f.next(), f.next(), f.next()) else { continue };
        let path = if rest.is_empty() {
            let _old = it.next();
            it.next().unwrap_or("").to_string()
        } else {
            rest.to_string()
        };
        let stats = match (a.parse(), d.parse()) {
            (Ok(a), Ok(d)) => Some((a, d)),
            _ => None,
        };
        if let Some(file) = files.iter_mut().find(|f| f.path == path && f.stats.is_none()) {
            file.stats = stats;
        }
    }
    files
}

// -- operation in progress ------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpKind {
    Merge,
    Rebase,
    CherryPick,
    Revert,
}

impl OpKind {
    pub fn name(self) -> &'static str {
        match self {
            OpKind::Merge => "merge",
            OpKind::Rebase => "rebase",
            OpKind::CherryPick => "cherry-pick",
            OpKind::Revert => "revert",
        }
    }
    pub fn title(self) -> &'static str {
        match self {
            OpKind::Merge => "MERGING",
            OpKind::Rebase => "REBASING",
            OpKind::CherryPick => "CHERRY-PICKING",
            OpKind::Revert => "REVERTING",
        }
    }
}

/// Read the operation in progress from (C-locale, long format) `git status`.
pub fn repo_state(status: &str) -> Option<OpKind> {
    for line in status.lines() {
        let l = line.trim();
        if l.starts_with("You are currently rebasing")
            || l.starts_with("interactive rebase in progress")
            || l.starts_with("rebase in progress")
        {
            return Some(OpKind::Rebase);
        }
        if l.starts_with("You are currently cherry-picking") {
            return Some(OpKind::CherryPick);
        }
        if l.starts_with("You are currently reverting") {
            return Some(OpKind::Revert);
        }
        if l.starts_with("You have unmerged paths") || l.starts_with("All conflicts fixed but you are still merging") {
            return Some(OpKind::Merge);
        }
    }
    None
}

// -- graph ----------------------------------------------------------------------

/// One column of a graph row: the glyph and the lane (for its color).
pub type Cell = (char, usize);

/// The graph next to one commit: its own row, and the connector row drawn below it
/// (a `│` for every lane still open), so adjacent commits are joined by a line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GraphRow {
    pub node: Vec<Cell>,
    pub link: Vec<Cell>,
}

/// Lay out the commit graph (children must come before parents, as with
/// `git log --date-order`). Each lane is two columns: glyph + connector.
pub fn graph(commits: &[Commit]) -> Vec<GraphRow> {
    let mut lanes: Vec<Option<&str>> = Vec::new();
    let mut rows = Vec::with_capacity(commits.len());
    for c in commits {
        let sha = c.sha.as_str();
        let col = match lanes.iter().position(|l| *l == Some(sha)) {
            Some(i) => i,
            None => match lanes.iter().position(Option::is_none) {
                Some(i) => i,
                None => {
                    lanes.push(None);
                    lanes.len() - 1
                }
            },
        };
        let before: Vec<bool> = lanes.iter().map(Option::is_some).collect();
        // other lanes waiting for this commit end here (branches converging into it)
        let merging: Vec<usize> = (0..lanes.len()).filter(|&i| i != col && lanes[i] == Some(sha)).collect();
        for &i in &merging {
            lanes[i] = None;
        }
        lanes[col] = c.parents.first().map(String::as_str);
        let mut opened = Vec::new();
        let mut joined = Vec::new();
        for p in c.parents.iter().skip(1) {
            if let Some(i) = lanes.iter().position(|l| *l == Some(p.as_str())) {
                if i != col {
                    joined.push(i);
                }
                continue;
            }
            // a free lane that isn't ending in this very row
            let free = (0..lanes.len()).find(|&i| lanes[i].is_none() && !merging.contains(&i) && i != col);
            let i = match free {
                Some(i) => i,
                None => {
                    lanes.push(None);
                    lanes.len() - 1
                }
            };
            lanes[i] = Some(p.as_str());
            opened.push(i);
        }

        let n = lanes.len().max(before.len());
        let mut cells: Vec<Cell> = Vec::with_capacity(n * 2);
        let reach: Vec<usize> = merging.iter().chain(&opened).chain(&joined).copied().collect();
        let lo = reach.iter().copied().chain([col]).min().unwrap_or(col);
        let hi = reach.iter().copied().chain([col]).max().unwrap_or(col);
        for i in 0..n {
            let was = before.get(i).copied().unwrap_or(false);
            // a lane in the middle of a horizontal connection (another one ends further out)
            let inner = (i > col && i < hi) || (i < col && i > lo);
            let glyph = if i == col {
                '●'
            } else if merging.contains(&i) {
                if inner { '┴' } else if i > col { '╯' } else { '╰' }
            } else if opened.contains(&i) {
                if inner { '┬' } else if i > col { '╮' } else { '╭' }
            } else if joined.contains(&i) {
                if inner { '┼' } else if i > col { '┤' } else { '├' }
            } else if was {
                if i > lo && i < hi { '┼' } else { '│' }
            } else if i > lo && i < hi {
                '─'
            } else {
                ' '
            };
            let far = if i < col { i } else { reach.iter().copied().filter(|&r| r > col).max().unwrap_or(col) };
            cells.push((glyph, if glyph == '─' { far } else { i }));
            let conn = if i >= lo && i < hi { '─' } else { ' ' };
            let conn_lane = if i < col { i } else { far };
            cells.push((conn, conn_lane));
        }
        while cells.last().is_some_and(|c| c.0 == ' ') {
            cells.pop();
        }
        while lanes.last().is_some_and(Option::is_none) {
            lanes.pop();
        }
        let mut link: Vec<Cell> = Vec::with_capacity(lanes.len() * 2);
        for (i, l) in lanes.iter().enumerate() {
            link.push((if l.is_some() { '│' } else { ' ' }, i));
            link.push((' ', i));
        }
        while link.last().is_some_and(|c| c.0 == ' ') {
            link.pop();
        }
        rows.push(GraphRow { node: cells, link });
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(sha: &str, parents: &[&str]) -> Commit {
        Commit { sha: sha.into(), parents: parents.iter().map(|p| p.to_string()).collect(), ..Default::default() }
    }

    fn text(rows: &[GraphRow]) -> Vec<String> {
        rows.iter().map(|r| r.node.iter().map(|c| c.0).collect()).collect()
    }

    fn links(rows: &[GraphRow]) -> Vec<String> {
        rows.iter().map(|r| r.link.iter().map(|c| c.0).collect()).collect()
    }

    #[test]
    fn connector_rows_join_adjacent_commits() {
        let rows = graph(&[c("m", &["b", "x"]), c("b", &["d"]), c("x", &["d"]), c("d", &[])]);
        // below each commit: every lane that is still waiting for a parent
        assert_eq!(links(&rows), vec!["│ │", "│ │", "│ │", ""]);
        let rows = graph(&[c("a", &["b"]), c("b", &[])]);
        assert_eq!(links(&rows), vec!["│", ""]);
    }

    #[test]
    fn linear_history_is_one_lane() {
        let rows = graph(&[c("a", &["b"]), c("b", &["c"]), c("c", &[])]);
        assert_eq!(text(&rows), vec!["●", "●", "●"]);
    }

    #[test]
    fn merge_opens_and_closes_a_lane() {
        let rows = graph(&[c("m", &["b", "x"]), c("b", &["d"]), c("x", &["d"]), c("d", &[])]);
        assert_eq!(text(&rows), vec!["●─╮", "● │", "│ ●", "●─╯"]);
    }

    #[test]
    fn two_children_of_one_parent() {
        // two branch tips sharing a parent: both lanes run until the parent
        let rows = graph(&[c("t1", &["p"]), c("t2", &["p"]), c("p", &[])]);
        assert_eq!(text(&rows), vec!["●", "│ ●", "●─╯"]);
    }

    #[test]
    fn several_lanes_ending_in_one_commit() {
        // three tips sharing one parent: the middle lane joins the line, the outer one ends it
        let rows = graph(&[c("t1", &["p"]), c("t2", &["p"]), c("t3", &["p"]), c("p", &[])]);
        assert_eq!(text(&rows)[3], "●─┴─╯");
    }

    #[test]
    fn merge_into_existing_lane() {
        let rows = graph(&[c("a", &["p"]), c("m", &["q", "p"]), c("q", &["p"]), c("p", &[])]);
        assert_eq!(text(&rows)[1], "├─●");
    }

    #[test]
    fn parses_log_records() {
        let out = "\u{1e}abc\u{1f}p1 p2\u{1f}Ann\u{1f}a@x\u{1f}2026-01-02 03:04\u{1f}1700000000\u{1f}HEAD -> main, origin/main, tag: v1\u{1f}Subject\u{1f}Body line\n\n";
        let v = parse_log(out);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].parents, vec!["p1", "p2"]);
        assert_eq!(v[0].body, "Body line");
        let locals: HashSet<String> = ["main".to_string()].into();
        let refs = classify(&v[0].decorations, &locals);
        assert_eq!(refs[0], RefLabel { kind: RefKind::Current, name: "main".into() });
        assert_eq!(refs[1].kind, RefKind::Remote);
        assert_eq!(refs[2], RefLabel { kind: RefKind::Tag, name: "v1".into() });
    }

    #[test]
    fn parses_branches() {
        let out = "refs/heads/main\u{1f}aaa\u{1f}origin/main\u{1f}[ahead 2, behind 1]\u{1f}*\u{1f}100\u{1f}s1\n\
                   refs/heads/old\u{1f}bbb\u{1f}origin/old\u{1f}[gone]\u{1f} \u{1f}50\u{1f}s2\n\
                   refs/remotes/origin/HEAD\u{1f}aaa\u{1f}\u{1f}\u{1f} \u{1f}100\u{1f}s1\n\
                   refs/remotes/origin/main\u{1f}aaa\u{1f}\u{1f}\u{1f} \u{1f}100\u{1f}s1\n";
        let b = parse_branches(out);
        assert_eq!(b.len(), 3);
        assert!(b[0].head && b[0].ahead == 2 && b[0].behind == 1);
        assert!(b.iter().any(|x| x.name == "old" && x.gone));
        assert!(b.iter().any(|x| x.name == "origin/main" && x.remote));
    }

    #[test]
    fn splits_remote_names() {
        let remotes = vec!["origin".to_string(), "up/stream".to_string()];
        assert_eq!(split_remote("origin/feat/x", &remotes), Some(("origin", "feat/x")));
        assert_eq!(split_remote("up/stream/main", &remotes), Some(("up/stream", "main")));
    }

    #[test]
    fn parses_raw_numstat() {
        let out = ":100644 100644 aaa bbb M\0src/a.rs\0:100644 100644 ccc ddd R087\0old.rs\0new.rs\0:000000 100644 000 eee A\0img.png\0\
                   3\t1\tsrc/a.rs\0" ;
        let mut out = out.to_string();
        out.push_str("5\t2\t\0old.rs\0new.rs\0-\t-\timg.png\0");
        let v = parse_changes(&out);
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].stats, Some((3, 1)));
        assert_eq!((v[1].code.as_str(), v[1].old.as_deref(), v[1].stats), ("R", Some("old.rs"), Some((5, 2))));
        assert_eq!(v[2].stats, None);
    }

    #[test]
    fn detects_operations() {
        assert_eq!(repo_state("interactive rebase in progress; onto abc\n"), Some(OpKind::Rebase));
        assert_eq!(repo_state("You have unmerged paths.\n"), Some(OpKind::Merge));
        assert_eq!(repo_state("On branch main\nnothing to commit\n"), None);
    }
}
