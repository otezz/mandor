use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use serde::Serialize;
use serde_json::Value;

/// Newest transcripts to inspect — mirrors `cs`'s CS_SCAN_LIMIT.
const SCAN_LIMIT: usize = 120;
/// Transcript header lines to read when extracting cwd + first message.
const HEAD_LINES: usize = 80;
const PREVIEW_CHARS: usize = 100;

#[derive(Serialize)]
pub struct SessionInfo {
    id: String,
    cwd: String,
    preview: String,
    mtime: u64,
}

/// Discover resumable Claude sessions from the shared on-disk store
/// (`~/.claude/projects/**/*.jsonl`), newest first. Blocking fs work runs off
/// the main thread.
#[tauri::command]
pub async fn list_sessions() -> Result<Vec<SessionInfo>, String> {
    tauri::async_runtime::spawn_blocking(scan_sessions)
        .await
        .map_err(|e| e.to_string())
}

fn projects_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".claude").join("projects"))
}

fn scan_sessions() -> Vec<SessionInfo> {
    let Some(projects) = projects_dir() else {
        return Vec::new();
    };

    // (mtime, path) for every transcript, at most one dir deep like `cs`.
    let mut files: Vec<(u64, PathBuf)> = Vec::new();
    if let Ok(dirs) = std::fs::read_dir(&projects) {
        for dir in dirs.flatten() {
            if let Ok(entries) = std::fs::read_dir(dir.path()) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                        continue;
                    }
                    let mtime = entry
                        .metadata()
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    files.push((mtime, path));
                }
            }
        }
    }

    files.sort_by(|a, b| b.0.cmp(&a.0));
    files.truncate(SCAN_LIMIT);

    let mut out = Vec::new();
    for (mtime, path) in files {
        let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some((cwd, preview)) = transcript_head(&path) else {
            continue;
        };
        out.push(SessionInfo {
            id: id.to_string(),
            cwd,
            preview,
            mtime,
        });
    }
    out
}

/// Read a transcript's header for its session cwd and first real user message.
/// Skips command/system-reminder envelope messages, like `cs`'s jq filter.
fn transcript_head(path: &PathBuf) -> Option<(String, String)> {
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);

    let mut cwd: Option<String> = None;
    let mut preview: Option<String> = None;

    for line in reader.lines().take(HEAD_LINES).map_while(Result::ok) {
        let Ok(entry) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if cwd.is_none() {
            if let Some(c) = entry.get("cwd").and_then(Value::as_str) {
                if !c.is_empty() {
                    cwd = Some(c.to_string());
                }
            }
        }
        if preview.is_none() && entry.get("type").and_then(Value::as_str) == Some("user") {
            if let Some(text) = user_text(&entry) {
                preview = Some(text);
            }
        }
        if cwd.is_some() && preview.is_some() {
            break;
        }
    }

    Some((cwd?, preview?))
}

/// Extract displayable text from a user message, or None for envelope-only
/// messages (slash commands, local-command output, system reminders).
fn user_text(entry: &Value) -> Option<String> {
    let content = entry.get("message")?.get("content")?;
    let raw = match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .find(|p| p.get("type").and_then(Value::as_str) == Some("text"))
            .and_then(|p| p.get("text").and_then(Value::as_str))
            .unwrap_or("")
            .to_string(),
        _ => return None,
    };
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.starts_with("<command-")
        || trimmed.starts_with("<local-command-")
        || trimmed.starts_with("<system-reminder>")
    {
        return None;
    }
    let one_line = trimmed.replace(['\n', '\t'], " ");
    Some(one_line.chars().take(PREVIEW_CHARS).collect())
}
