//! "Also remove its worktree" when closing a session: find the linked git
//! worktree a session works in, and remove it with plain `git worktree remove`,
//! which refuses on uncommitted or untracked changes. The main working tree is
//! never reported or removed, and the branch is left alone.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeInfo {
    path: String,
    branch: Option<String>,
    dirty: bool,
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// The linked worktree containing `dir`, as (its root, the repo's common git
/// dir). None for the main working tree or anything outside git.
fn linked_worktree(dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let out = git(
        dir,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-dir",
            "--git-common-dir",
        ],
    )
    .ok()?;
    let mut lines = out.lines().map(PathBuf::from);
    let (top, git_dir, common) = (lines.next()?, lines.next()?, lines.next()?);
    (git_dir != common).then_some((top, common))
}

fn worktree_info(dir: &Path) -> Option<WorktreeInfo> {
    let (top, _) = linked_worktree(dir)?;
    let branch = git(&top, &["symbolic-ref", "--quiet", "--short", "HEAD"]).ok();
    let dirty = git(&top, &["status", "--porcelain"]).map_or(true, |s| !s.is_empty());
    Some(WorktreeInfo {
        path: top.to_string_lossy().into_owned(),
        branch,
        dirty,
    })
}

/// The linked worktree a session works in, if any.
#[tauri::command]
pub async fn session_worktree(
    id: String,
    profile_id: Option<String>,
) -> Result<Option<WorktreeInfo>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let cwd = crate::sessions::last_session_cwd(&id, profile_id.as_deref())?;
        worktree_info(Path::new(&cwd))
    })
    .await
    .map_err(|e| e.to_string())
}

fn remove(path: &Path) -> Result<(), String> {
    let (top, common) = linked_worktree(path).ok_or("not a linked git worktree")?;
    if top != path {
        return Err("not the root of a git worktree".into());
    }
    if let Some((pid, name)) = process_inside(&top) {
        return Err(format!("still in use by {name} (pid {pid})"));
    }
    // Run from the repo's git dir, not from inside the worktree being removed.
    git(&common, &["worktree", "remove", &top.to_string_lossy()]).map(|_| ())
}

/// A process whose working directory is inside `dir`, e.g. another session's
/// claude or a dev server it left running. Linux only (reads /proc).
#[cfg(target_os = "linux")]
fn process_inside(dir: &Path) -> Option<(u32, String)> {
    let me = std::process::id();
    std::fs::read_dir("/proc")
        .ok()?
        .flatten()
        .find_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            if pid == me {
                return None;
            }
            let cwd = std::fs::read_link(entry.path().join("cwd")).ok()?;
            if !cwd.starts_with(dir) {
                return None;
            }
            let name = std::fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
            Some((pid, name.trim().to_string()))
        })
}

#[cfg(not(target_os = "linux"))]
fn process_inside(_dir: &Path) -> Option<(u32, String)> {
    None
}

/// Remove a linked worktree. No --force: git refuses when it has changes.
#[tauri::command]
pub async fn remove_worktree(path: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || remove(Path::new(&path)))
        .await
        .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_repo(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("mandor-wt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        ] {
            git(&repo, args).unwrap();
        }
        repo
    }

    #[test]
    fn reports_and_removes_only_linked_worktrees() {
        let repo = scratch_repo("basic");
        let wt = repo.parent().unwrap().join("wt");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                &wt.to_string_lossy(),
            ],
        )
        .unwrap();
        let wt = wt.canonicalize().unwrap();

        assert_eq!(worktree_info(&repo), None, "main working tree");
        std::fs::create_dir(wt.join("sub")).unwrap();
        let info = worktree_info(&wt.join("sub")).unwrap();
        assert_eq!(info.path, wt.to_string_lossy());
        assert_eq!(info.branch.as_deref(), Some("feature"));
        assert!(!info.dirty, "an empty dir isn't a change");

        assert!(remove(&repo).is_err(), "main working tree");
        assert!(remove(&wt.join("sub")).is_err(), "not the worktree root");

        std::fs::write(wt.join("new.txt"), "x").unwrap();
        assert!(worktree_info(&wt).unwrap().dirty);
        assert!(remove(&wt).is_err(), "untracked file");
        assert!(wt.exists());

        std::fs::remove_file(wt.join("new.txt")).unwrap();
        #[cfg(target_os = "linux")]
        {
            let mut sleeper = Command::new("sleep")
                .arg("30")
                .current_dir(wt.join("sub"))
                .spawn()
                .unwrap();
            let err = remove(&wt).unwrap_err();
            assert!(err.contains("sleep"), "{err}");
            sleeper.kill().unwrap();
            sleeper.wait().unwrap();
        }
        remove(&wt).unwrap();
        assert!(!wt.exists());
        assert!(
            git(&repo, &["rev-parse", "--verify", "-q", "feature"]).is_ok(),
            "branch kept"
        );
        let _ = std::fs::remove_dir_all(repo.parent().unwrap());
    }
}
