//! Parsing of `git status --porcelain -b` and `git diff --numstat` output.

use std::collections::HashMap;

#[derive(Default, Clone)]
pub struct GitFile {
    pub code: String,
    pub path: String,
    pub stats: Option<(u64, u64)>,
}

#[derive(Default, Clone)]
pub struct GitInfo {
    pub branch: String,
    pub ahead: u64,
    pub behind: u64,
    pub files: Vec<GitFile>,
    pub added: u64,
    pub deleted: u64,
}

/// Raw command outputs for one directory; `info()` combines whatever has arrived.
#[derive(Default)]
pub struct GitRaw {
    pub status: Option<Result<String, String>>,
    pub unstaged: Option<String>,
    pub staged: Option<String>,
}

pub enum GitState {
    Loading,
    NotRepo,
    Repo(GitInfo),
}

impl GitRaw {
    pub fn state(&self) -> GitState {
        match &self.status {
            None => GitState::Loading,
            Some(Err(_)) => GitState::NotRepo,
            Some(Ok(out)) => {
                let mut stats = HashMap::new();
                for ns in [&self.unstaged, &self.staged].into_iter().flatten() {
                    parse_numstat(ns, &mut stats);
                }
                GitState::Repo(build(out, &stats))
            }
        }
    }
}

fn unquote(p: &str) -> String {
    let p = p.trim();
    if p.len() >= 2 && p.starts_with('"') && p.ends_with('"') {
        serde_json::from_str::<String>(p).unwrap_or_else(|_| p[1..p.len() - 1].to_string())
    } else {
        p.to_string()
    }
}

/// `dir/{old => new}/f` or `old => new` -> new path.
fn rename_target(p: &str) -> String {
    if let (Some(l), Some(r)) = (p.find('{'), p.find('}')) {
        if l < r {
            let inner = &p[l + 1..r];
            let new = inner.split(" => ").nth(1).unwrap_or(inner);
            return format!("{}{}{}", &p[..l], new, &p[r + 1..]).replace("//", "/");
        }
    }
    p.split(" => ").last().unwrap_or(p).to_string()
}

fn parse_numstat(out: &str, stats: &mut HashMap<String, (u64, u64)>) {
    for line in out.lines() {
        let mut it = line.splitn(3, '\t');
        let (Some(a), Some(d), Some(path)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        let a = a.parse().unwrap_or(0);
        let d = d.parse().unwrap_or(0);
        let e = stats.entry(rename_target(&unquote(path))).or_insert((0, 0));
        e.0 += a;
        e.1 += d;
    }
}

fn build(status: &str, stats: &HashMap<String, (u64, u64)>) -> GitInfo {
    let mut info = GitInfo::default();
    for line in status.lines() {
        if let Some(head) = line.strip_prefix("## ") {
            parse_branch(head, &mut info);
            continue;
        }
        if line.len() < 4 {
            continue;
        }
        let code = line[..2].trim().to_string();
        let rest = &line[3..];
        let path = match rest.split_once(" -> ") {
            Some((_, new)) => unquote(new),
            None => unquote(rest),
        };
        let s = stats.get(&path).copied();
        if let Some((a, d)) = s {
            info.added += a;
            info.deleted += d;
        }
        info.files.push(GitFile { code, path, stats: s });
    }
    info
}

fn parse_branch(head: &str, info: &mut GitInfo) {
    if let Some(b) = head.strip_prefix("No commits yet on ") {
        info.branch = b.trim().to_string();
        return;
    }
    let (name, tracking) = match head.split_once(" [") {
        Some((n, t)) => (n, Some(t.trim_end_matches(']'))),
        None => (head, None),
    };
    let local = name.split("...").next().unwrap_or(name);
    info.branch = if local.starts_with("HEAD (no branch)") {
        "detached".to_string()
    } else {
        local.to_string()
    };
    if let Some(t) = tracking {
        for part in t.split(", ") {
            if let Some(n) = part.strip_prefix("ahead ") {
                info.ahead = n.parse().unwrap_or(0);
            } else if let Some(n) = part.strip_prefix("behind ") {
                info.behind = n.parse().unwrap_or(0);
            }
        }
    }
}
