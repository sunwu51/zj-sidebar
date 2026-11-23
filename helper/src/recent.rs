//! The newest agent sessions across all directories (`zjsb-helper recent <n>`).

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::UNIX_EPOCH;

use serde_json::{json, Value};

struct Entry {
    kind: &'static str,
    id: String,
    title: String,
    cwd: String,
    updated_ms: i64,
}

fn clean(s: &str) -> Option<String> {
    let t = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.is_empty() { None } else { Some(t) }
}

/// `2026-10-26T14:05:12.345Z` -> unix seconds.
fn parse_timestamp(ts: &str) -> Option<i64> {
    let num = |r: std::ops::Range<usize>| ts.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146_097 + doe - 719_468) * 86_400 + hh * 3600 + mm * 60 + ss)
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
    if t.is_empty() || t.starts_with('<') { None } else { clean(t) }
}

fn read_head(path: &Path, max: u64) -> String {
    let mut buf = Vec::new();
    if let Ok(f) = File::open(path) {
        let _ = f.take(max).read_to_end(&mut buf);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn read_tail(path: &Path, len: u64, max: u64) -> String {
    let Ok(mut f) = File::open(path) else { return String::new() };
    let start = len.saturating_sub(max);
    let mut buf = Vec::new();
    if f.seek(SeekFrom::Start(start)).is_err() || f.read_to_end(&mut buf).is_err() {
        return String::new();
    }
    let s = String::from_utf8_lossy(&buf).into_owned();
    if start > 0 { s.split_once('\n').map(|(_, r)| r.to_string()).unwrap_or_default() } else { s }
}

/// One Claude session file: its title, directory and last message time (ms).
fn claude_entry(path: &Path, len: u64, mtime_ms: i64) -> Option<Entry> {
    let (mut custom, mut ai, mut summary, mut prompt, mut last) = (None, None, None, None, None);
    for line in read_tail(path, len, 256 * 1024).lines() {
        let interesting = ["\"type\":\"user\"", "\"type\":\"assistant\"", "\"custom-title\"", "\"ai-title\"", "\"summary\"", "\"last-prompt\""]
            .iter()
            .any(|k| line.contains(k));
        if !interesting {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let get = |k: &str| v.get(k).and_then(Value::as_str).and_then(clean);
        match v.get("type").and_then(Value::as_str) {
            Some("user" | "assistant") => {
                if let Some(ts) = v.get("timestamp").and_then(Value::as_str).and_then(parse_timestamp) {
                    last = Some(ts * 1000);
                }
            }
            Some("custom-title") => custom = get("customTitle").or(custom),
            Some("ai-title") => ai = get("aiTitle").or(ai),
            Some("summary") => summary = get("summary").or(summary),
            Some("last-prompt") => prompt = get("lastPrompt").or(prompt),
            _ => {}
        }
    }
    let head = read_head(path, 128 * 1024);
    let msgs: Vec<Value> = head
        .lines()
        .filter(|l| l.contains("\"type\":\"user\"") || l.contains("\"type\":\"assistant\""))
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    // nobody typed into it: `/resume` does not list it either
    if last.is_none() && prompt.is_none() && msgs.is_empty() {
        return None;
    }
    let cwd = msgs.iter().find_map(|v| v.get("cwd").and_then(Value::as_str)).map(String::from)?;
    let title = custom.or(ai).or(summary).or(prompt).or_else(|| msgs.iter().find_map(user_text))?;
    Some(Entry {
        kind: "claude",
        id: path.file_stem()?.to_string_lossy().into_owned(),
        title,
        cwd,
        updated_ms: last.unwrap_or(mtime_ms),
    })
}

/// The `n` newest Claude sessions: the files are ordered by mtime first, so only about
/// `n` of them are ever parsed.
fn claude(n: usize) -> Vec<Entry> {
    let Some(projects) = crate::home_dir().map(|h| h.join(".claude").join("projects")) else { return Vec::new() };
    let mut files = Vec::new();
    for dir in fs::read_dir(&projects).into_iter().flatten().flatten() {
        for f in fs::read_dir(dir.path()).into_iter().flatten().flatten() {
            let p = f.path();
            let Ok(meta) = f.metadata() else { continue };
            if !meta.is_file() || p.extension().is_none_or(|x| x != "jsonl") {
                continue;
            }
            let ms = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as i64);
            files.push((ms, meta.len(), p));
        }
    }
    files.sort_by(|a, b| b.0.cmp(&a.0));
    // the file's mtime is not the last message's time (Claude appends metadata to old
    // sessions), so look at a few more than asked for before cutting
    files.into_iter().take(n * 2).filter_map(|(ms, len, p)| claude_entry(&p, len, ms)).collect()
}

pub fn recent(n: usize) -> Result<(), String> {
    let mut all = claude(n);
    // no Codex database is fine
    if let Ok(threads) = crate::codex_threads(None) {
        all.extend(threads.into_iter().take(n).map(|t| Entry {
            kind: "codex",
            id: t.id,
            title: t.title,
            cwd: t.cwd,
            updated_ms: t.updated_ms,
        }));
    }
    all.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));
    for e in all.into_iter().take(n) {
        println!("{}", json!({ "kind": e.kind, "id": e.id, "title": e.title, "cwd": e.cwd, "updated_ms": e.updated_ms }));
    }
    Ok(())
}
