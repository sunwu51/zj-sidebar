//! Native companion for the zj-sidebar plugin, for data a wasm plugin can't read.
//!
//! `zjsb-helper codex <cwd>` prints one JSON object per line
//! (`{"id","title","updated_ms"}`) for the Codex threads `codex resume` would list
//! for that directory, newest first.
//!
//! `zjsb-helper running <pid>...` prints `{"root","kind","id"}` for each Claude Code /
//! Codex session running somewhere below the given (pane shell) processes.

mod procs;

use std::collections::{BTreeSet, HashMap};
use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use serde_json::json;

/// Sources `codex resume` shows without `--include-non-interactive`.
const INTERACTIVE_SOURCES: &[&str] = &["cli", "vscode"];

fn codex_home() -> Option<PathBuf> {
    if let Some(h) = env::var_os("CODEX_HOME") {
        return Some(PathBuf::from(h));
    }
    Some(home_dir()?.join(".codex"))
}

/// The newest `state_<n>.sqlite` (the number is a schema version).
fn state_db(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let n: u32 = name.strip_prefix("state_")?.strip_suffix(".sqlite")?.parse().ok()?;
            Some((n, e.path()))
        })
        .max_by_key(|(n, _)| *n)
        .map(|(_, p)| p)
}

/// Compare paths for equality. Windows paths (`C:\..`, optionally with a `\\?\` prefix)
/// are case-insensitive with unified separators; others are compared as is.
fn norm(path: &str) -> String {
    let p = path.strip_prefix(r"\\?\").unwrap_or(path);
    if p.as_bytes().get(1) == Some(&b':') || p.starts_with(r"\\") {
        let mut p = p.replace('/', "\\").to_lowercase();
        while p.len() > 3 && p.ends_with('\\') {
            p.pop();
        }
        p
    } else {
        let t = p.trim_end_matches('/');
        if t.is_empty() { "/".to_string() } else { t.to_string() }
    }
}

fn first_line(s: &str) -> &str {
    s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("")
}

fn codex(cwd: &str) -> Result<(), String> {
    let home = codex_home().ok_or("no home directory")?;
    let db = state_db(&home).ok_or("no codex state database")?;
    let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|e| e.to_string())?;
    conn.busy_timeout(Duration::from_secs(2)).map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare(
            "SELECT id, cwd, source, COALESCE(name, ''), title, first_user_message,
                    COALESCE(updated_at_ms, updated_at * 1000)
             FROM threads
             WHERE archived = 0
             ORDER BY 7 DESC",
        )
        .map_err(|e| e.to_string())?;
    let want = norm(cwd);
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, i64>(6)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    for row in rows.flatten() {
        let (id, thread_cwd, source, name, title, first_msg, updated_ms) = row;
        if !INTERACTIVE_SOURCES.contains(&source.as_str()) || norm(&thread_cwd) != want {
            continue;
        }
        let title = [&name, &title, &first_msg]
            .into_iter()
            .map(|s| first_line(s))
            .find(|s| !s.is_empty())
            .unwrap_or("(untitled)");
        println!("{}", json!({ "id": id, "title": title, "updated_ms": updated_ms }));
    }
    Ok(())
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("USERPROFILE").or_else(|| env::var_os("HOME")).map(PathBuf::from)
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36 && s.chars().filter(|&c| c == '-').count() == 4 && s.chars().all(|c| c == '-' || c.is_ascii_hexdigit())
}

/// The session id in a command line like `claude --resume <id>` / `codex resume <id>`.
fn session_from_cmdline(cmdline: &str) -> Option<(&'static str, String)> {
    let tokens: Vec<String> = cmdline.split_whitespace().map(|t| t.trim_matches('"').to_string()).collect();
    let lower = cmdline.to_lowercase();
    let after = |flags: &[&str]| {
        tokens.windows(2).find(|w| flags.contains(&w[0].as_str()) && is_uuid(&w[1])).map(|w| w[1].clone())
    };
    if lower.contains("codex") {
        if let Some(id) = after(&["resume"]) {
            return Some(("codex", id));
        }
    }
    if lower.contains("claude") {
        if let Some(id) = after(&["--resume", "-r", "--session-id"]) {
            return Some(("claude", id));
        }
    }
    None
}

/// Claude Code records each running process in `~/.claude/sessions/<pid>.json`.
fn claude_session_of(pid: u32) -> Option<String> {
    let path = home_dir()?.join(".claude").join("sessions").join(format!("{pid}.json"));
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    v.get("sessionId")?.as_str().map(String::from)
}

fn running(roots: &[u32]) -> Result<(), String> {
    let all = procs::list();
    let mut children: HashMap<u32, Vec<&procs::Proc>> = HashMap::new();
    for p in &all {
        if p.pid != p.ppid {
            children.entry(p.ppid).or_default().push(p);
        }
    }
    let by_pid: HashMap<u32, &procs::Proc> = all.iter().map(|p| (p.pid, p)).collect();
    let mut seen = BTreeSet::new();
    for &root in roots {
        // the pane's own process counts too (a pane started directly as `claude`)
        let mut stack = vec![root];
        let mut visited = BTreeSet::new();
        while let Some(pid) = stack.pop() {
            if !visited.insert(pid) {
                continue;
            }
            stack.extend(children.get(&pid).into_iter().flatten().map(|c| c.pid));
            let Some(p) = by_pid.get(&pid) else { continue };
            // Reading a command line costs a syscall per process on Windows, so skip
            // processes that can't be an agent; on Unix the listing has them already.
            if cfg!(windows) && !["claude", "codex", "node", "bun"].iter().any(|n| p.exe.contains(n)) {
                continue;
            }
            let found = claude_session_of(p.pid)
                .map(|id| ("claude", id))
                .or_else(|| procs::cmdline(p).as_deref().and_then(session_from_cmdline));
            if let Some((kind, id)) = found {
                if seen.insert((root, kind, id.clone())) {
                    println!("{}", json!({ "root": root, "kind": kind, "id": id }));
                }
            }
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let result = match args.as_slice() {
        [cmd, cwd] if cmd == "codex" => codex(cwd),
        [cmd, pids @ ..] if cmd == "running" => running(&pids.iter().filter_map(|p| p.parse().ok()).collect::<Vec<_>>()),
        _ => Err("usage: zjsb-helper codex <cwd> | zjsb-helper running <pid>...".into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("zjsb-helper: {e}");
            ExitCode::FAILURE
        }
    }
}
