use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use serde::Serialize;
use serde_json::Value;

/// Newest transcripts to inspect. `cs` capped this at 120 for fzf speed, but the
/// GUI picker can afford more — the scan only reads each file's head, off-thread,
/// when the picker opens. Sessions older than the newest `SCAN_LIMIT` by mtime
/// won't appear (use the filter box to narrow what does).
const SCAN_LIMIT: usize = 500;
/// Max lines to scan per transcript. cwd + the first message sit at the very
/// top, but the session's display name (a `custom-title` entry) can be written
/// at any point — well past a small header — so we read further to catch it,
/// bounded to keep the off-thread picker scan snappy. Parsing is gated: past the
/// cwd+preview point we only parse the cheap `custom-title` lines.
const SCAN_LINES: usize = 20_000;
const PREVIEW_CHARS: usize = 100;

/// A pull request claude created/updated in this session (from a git tool's
/// `gitOperation.pr` record in the transcript).
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PrInfo {
    pr_number: u64,
    pr_url: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    id: String,
    cwd: String,
    preview: String,
    name: String, // the session's -n display name (customTitle), "" if none
    mtime: u64,
    pr: Option<PrInfo>,
}

struct Head {
    cwd: String,
    preview: String,
    name: String,
    pr: Option<PrInfo>,
}

/// Discover resumable Claude sessions from the shared on-disk store
/// (`~/.claude/projects/**/*.jsonl`), newest first. Blocking fs work runs off
/// the main thread.
#[tauri::command]
pub async fn list_sessions(profile_id: Option<String>) -> Result<Vec<SessionInfo>, String> {
    tauri::async_runtime::spawn_blocking(move || scan_sessions(profile_id.as_deref()))
        .await
        .map_err(|e| e.to_string())
}

/// The transcript store for a profile (`<profile dir>/projects`) or the default
/// (`~/.claude/projects`). Claude writes `projects/` under `$CLAUDE_CONFIG_DIR`,
/// so a profile's sessions live under its own dir — mirror that here.
fn projects_dir(profile_id: Option<&str>) -> Option<PathBuf> {
    if let Some(dir) = profile_id
        .filter(|p| !p.is_empty())
        .and_then(crate::pty::profile_dir)
    {
        return Some(dir.join("projects"));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".claude").join("projects"))
}

fn scan_sessions(profile_id: Option<&str>) -> Vec<SessionInfo> {
    let Some(projects) = projects_dir(profile_id) else {
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
        let Some(h) = transcript_head(&path) else {
            continue;
        };
        out.push(SessionInfo {
            id: id.to_string(),
            cwd: h.cwd,
            preview: h.preview,
            name: h.name,
            mtime,
            pr: h.pr,
        });
    }
    out
}

/// The transcript path for a session id running in `cwd` (project dir = the cwd
/// with '/' and '.' turned into '-', matching Claude's on-disk layout).
fn transcript_path(cwd: &str, id: &str, profile_id: Option<&str>) -> Option<PathBuf> {
    let enc: String = cwd
        .chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect();
    Some(
        projects_dir(profile_id)?
            .join(enc)
            .join(format!("{id}.jsonl")),
    )
}

/// The PR (if any) claude opened/updated in a specific session — used to refresh
/// a live session's PR badge without re-scanning every transcript.
#[tauri::command]
pub async fn session_pr(
    id: String,
    cwd: String,
    profile_id: Option<String>,
) -> Result<Option<PrInfo>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        transcript_path(&cwd, &id, profile_id.as_deref())
            .as_deref()
            .and_then(transcript_head)
            .and_then(|h| h.pr)
    })
    .await
    .map_err(|e| e.to_string())
}

/// Read a transcript for its cwd, first real user message, and the session's
/// display name (`customTitle`, set via `-n` and stored on `custom-title`
/// entries). cwd + preview are near the top; the name can appear much later, so
/// we scan on (bounded by `SCAN_LINES`) and keep the last name seen (the current
/// one). Skips command/system-reminder/bash envelope messages, like `cs`'s jq
/// filter. Parsing is gated: once cwd+preview are known we only parse the cheap
/// `custom-title` lines, so unnamed sessions cost little beyond the head.
fn transcript_head(path: &std::path::Path) -> Option<Head> {
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);

    let mut cwd: Option<String> = None;
    let mut preview: Option<String> = None;
    let mut name = String::new();
    let mut pr: Option<PrInfo> = None;

    for line in reader.lines().take(SCAN_LINES).map_while(Result::ok) {
        let need_meta = cwd.is_none() || preview.is_none();
        let is_title = line.contains("\"type\":\"custom-title\"");
        // A git tool result records a PR under toolUseResult.gitOperation.pr.
        let is_git = line.contains("\"gitOperation\"");
        if !need_meta && !is_title && !is_git {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if is_git {
            if let Some(p) = entry.pointer("/toolUseResult/gitOperation/pr") {
                if let (Some(number), Some(url)) = (
                    p.get("number").and_then(Value::as_u64),
                    p.get("url").and_then(Value::as_str),
                ) {
                    pr = Some(PrInfo {
                        pr_number: number,
                        pr_url: url.to_string(),
                    }); // last one wins (most recent PR action)
                }
            }
            continue;
        }
        if is_title {
            if let Some(t) = entry.get("customTitle").and_then(Value::as_str) {
                let t = t.trim();
                if !t.is_empty() {
                    name = t.to_string();
                }
            }
            continue;
        }
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
    }

    Some(Head {
        cwd: cwd?,
        preview: preview?,
        name,
        pr,
    })
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
        || trimmed.starts_with("<bash-")
    {
        return None;
    }
    let one_line = trimmed.replace(['\n', '\t'], " ");
    Some(one_line.chars().take(PREVIEW_CHARS).collect())
}
