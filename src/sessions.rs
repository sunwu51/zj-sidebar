//! Claude Code and Codex sessions belonging to a directory.
//!
//! Claude sessions are read through the plugin's `/host` mount, which is pointed at
//! the user's home directory. Codex keeps its thread list in a SQLite database, which
//! the native `zjsb-helper` reads for us (see `parse_codex`).

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

const HOST: &str = "/host";
pub const MAX_SESSIONS: usize = 100;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Claude,
    Codex,
}

#[derive(Clone)]
pub struct Session {
    pub kind: Kind,
    pub id: String,
    pub title: String,
    /// last activity, unix seconds
    pub updated: u64,
}

/// What we learn from one Claude session file.
#[derive(Clone)]
struct ClaudeInfo {
    title: String,
    /// timestamp of the last user/assistant message
    last_message: Option<u64>,
}

#[derive(Default)]
pub struct SessionScanner {
    /// claude file -> (mtime, len, parsed info; None for sessions without messages)
    claude_cache: HashMap<PathBuf, (u64, u64, Option<ClaudeInfo>)>,
}

fn mtime_secs(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Parse an RFC 3339 UTC timestamp (`2026-10-26T14:05:12.345Z`) to unix seconds.
fn parse_timestamp(ts: &str) -> Option<u64> {
    let num = |r: std::ops::Range<usize>| ts.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    // days from civil (Howard Hinnant)
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hh * 3600 + mm * 60 + ss).ok()
}

/// Read up to `max` bytes from the start of the file.
fn read_head(path: &Path, max: u64) -> Option<String> {
    let mut buf = Vec::new();
    File::open(path).ok()?.take(max).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Read up to `max` bytes from the end of the file, dropping the first partial line.
fn read_tail(path: &Path, len: u64, max: u64) -> Option<String> {
    let mut f = File::open(path).ok()?;
    let start = len.saturating_sub(max);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let s = String::from_utf8_lossy(&buf).into_owned();
    if start > 0 {
        Some(s.split_once('\n').map(|(_, r)| r.to_string()).unwrap_or_default())
    } else {
        Some(s)
    }
}

fn clean_title(s: &str) -> Option<String> {
    let t: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.is_empty() { None } else { Some(t) }
}

/// Claude encodes a project dir by replacing every non-alphanumeric char with '-'.
fn claude_project_key(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn is_message_line(line: &str) -> bool {
    line.contains("\"type\":\"user\"") || line.contains("\"type\":\"assistant\"")
}

fn user_text(v: &Value) -> Option<String> {
    let content = v.get("message")?.get("content")?;
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter(|i| i.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|i| i.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => return None,
    };
    let t = text.trim();
    if t.is_empty() || t.starts_with('<') {
        return None;
    }
    clean_title(t)
}

/// Title and last message time of a Claude session. Claude keeps appending
/// metadata (cost, titles, mode) to old sessions, so the file mtime is not the
/// session's last activity; `/resume` goes by the last message instead.
fn parse_claude(path: &Path, len: u64) -> Option<ClaudeInfo> {
    let (mut custom, mut ai, mut summary, mut prompt) = (None, None, None, None);
    let mut last_message = None;
    if let Some(tail) = read_tail(path, len, 256 * 1024) {
        for line in tail.lines() {
            let message = is_message_line(line);
            let title_line = line.contains("\"custom-title\"")
                || line.contains("\"ai-title\"")
                || line.contains("\"summary\"")
                || line.contains("\"last-prompt\"");
            if !message && !title_line {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
            let get = |k: &str| v.get(k).and_then(Value::as_str).and_then(clean_title);
            match v.get("type").and_then(Value::as_str) {
                Some("user" | "assistant") => {
                    if let Some(ts) = v.get("timestamp").and_then(Value::as_str).and_then(parse_timestamp) {
                        last_message = Some(ts);
                    }
                }
                Some("custom-title") => custom = get("customTitle").or(custom),
                Some("ai-title") => ai = get("aiTitle").or(ai),
                Some("summary") => summary = get("summary").or(summary),
                Some("last-prompt") => prompt = get("lastPrompt").or(prompt),
                _ => {}
            }
        }
    }
    let head = read_head(path, 128 * 1024).unwrap_or_default();
    let mut head_messages = head
        .lines()
        .filter(|l| is_message_line(l))
        .filter_map(|l| serde_json::from_str::<Value>(l).ok());
    // A session nobody typed into (only metadata) is not listed by /resume either.
    if last_message.is_none() && prompt.is_none() && head_messages.clone().next().is_none() {
        return None;
    }
    let title = custom
        .or(ai)
        .or(summary)
        .or(prompt)
        .or_else(|| head_messages.find_map(|v| user_text(&v)))?;
    Some(ClaudeInfo { title, last_message })
}

impl SessionScanner {
    pub fn scan_claude(&mut self, cwd: &str) -> Vec<Session> {
        let projects = Path::new(HOST).join(".claude").join("projects");
        let key = claude_project_key(cwd);
        let mut dir = projects.join(&key);
        if !dir.is_dir() {
            // drive letters and the like may differ in case
            let found = fs::read_dir(&projects).ok().and_then(|rd| {
                rd.flatten()
                    .find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(&key))
                    .map(|e| e.path())
            });
            match found {
                Some(d) => dir = d,
                None => return Vec::new(),
            }
        }
        let Ok(rd) = fs::read_dir(&dir) else { return Vec::new() };
        let mut out = Vec::new();
        for e in rd.flatten() {
            let p = e.path();
            if !p.extension().is_some_and(|x| x == "jsonl") {
                continue;
            }
            let Ok(meta) = e.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            let (mtime, len) = (mtime_secs(&meta), meta.len());
            let info = match self.claude_cache.get(&p) {
                Some((m, l, info)) if *m == mtime && *l == len => info.clone(),
                _ => {
                    let info = parse_claude(&p, len);
                    self.claude_cache.insert(p.clone(), (mtime, len, info.clone()));
                    info
                }
            };
            let Some(info) = info else { continue };
            let id = p.file_stem().unwrap_or_default().to_string_lossy().into_owned();
            out.push(Session {
                kind: Kind::Claude,
                id,
                title: info.title,
                updated: info.last_message.unwrap_or(mtime),
            });
        }
        out
    }
}

/// Parse `zjsb-helper codex <cwd>` output: one `{"id","title","updated_ms"}` per line.
pub fn parse_codex(stdout: &str) -> Vec<Session> {
    stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| {
            Some(Session {
                kind: Kind::Codex,
                id: v.get("id")?.as_str()?.to_string(),
                title: v.get("title").and_then(Value::as_str).and_then(clean_title)?,
                updated: v.get("updated_ms")?.as_u64()? / 1000,
            })
        })
        .collect()
}
