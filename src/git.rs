//! Git worktree management — shell out to `git` CLI.

use std::path::{Path, PathBuf};

fn git(repo_dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = std::process::Command::new("git")
        .args(args).current_dir(repo_dir).output()
        .map_err(|e| format!("git: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

pub fn is_git_repo(dir: &Path) -> bool {
    git(dir, &["rev-parse", "--git-dir"]).is_ok()
}

fn worktree_dir(repo_dir: &Path, agent_name: &str) -> PathBuf {
    repo_dir.join(".agend-worktrees").join(agent_name)
}

pub fn create_worktree(repo_dir: &Path, agent_name: &str, branch: &str) -> Result<PathBuf, String> {
    let wt_path = worktree_dir(repo_dir, agent_name);
    if wt_path.exists() {
        return Ok(wt_path); // already exists (respawn)
    }
    // Create branch from HEAD if it doesn't exist
    if git(repo_dir, &["rev-parse", "--verify", branch]).is_err() {
        git(repo_dir, &["branch", branch])?;
    }
    if let Some(parent) = wt_path.parent() { std::fs::create_dir_all(parent).ok(); }
    git(repo_dir, &["worktree", "add", &wt_path.display().to_string(), branch])?;
    eprintln!("[git] created worktree for '{agent_name}' at {}", wt_path.display());
    Ok(wt_path)
}

pub fn remove_worktree(repo_dir: &Path, agent_name: &str) -> Result<(), String> {
    let wt_path = worktree_dir(repo_dir, agent_name);
    if !wt_path.exists() { return Ok(()); }
    git(repo_dir, &["worktree", "remove", "--force", &wt_path.display().to_string()])?;
    eprintln!("[git] removed worktree for '{agent_name}'");
    Ok(())
}

#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    pub agent_name: String,
    pub path: PathBuf,
    pub branch: String,
}

pub fn list_worktrees(repo_dir: &Path) -> Vec<WorktreeInfo> {
    let wt_dir = repo_dir.join(".agend-worktrees");
    let entries = match std::fs::read_dir(&wt_dir) { Ok(e) => e, Err(_) => return vec![] };
    entries.flatten().map(|e| {
        let name = e.file_name().to_string_lossy().to_string();
        let path = e.path();
        let branch = git(&path, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
        WorktreeInfo { agent_name: name, path, branch }
    }).collect()
}

#[derive(Debug, Clone)]
pub struct MergePreview {
    pub diff_stat: String,
    pub files_changed: usize,
    pub has_conflicts: bool,
}

pub fn merge_preview(repo_dir: &Path, branch: &str) -> Result<MergePreview, String> {
    let diff_stat = git(repo_dir, &["diff", "--stat", &format!("HEAD...{branch}")])?;
    let files_changed = diff_stat.lines().count().saturating_sub(1); // last line is summary
    // Check for conflicts using merge-tree
    let base = git(repo_dir, &["merge-base", "HEAD", branch]).unwrap_or_default();
    let has_conflicts = if !base.is_empty() {
        git(repo_dir, &["merge-tree", &base, "HEAD", branch])
            .map(|out| out.contains("<<<<<<")).unwrap_or(false)
    } else { false };
    Ok(MergePreview { diff_stat, files_changed, has_conflicts })
}

pub fn squash_merge(repo_dir: &Path, branch: &str, message: &str) -> Result<(), String> {
    git(repo_dir, &["merge", "--squash", branch])?;
    git(repo_dir, &["commit", "-m", message])?;
    eprintln!("[git] squash-merged '{branch}' into HEAD");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init"]).unwrap();
        git(tmp.path(), &["config", "user.email", "test@test.com"]).unwrap();
        git(tmp.path(), &["config", "user.name", "Test"]).unwrap();
        std::fs::write(tmp.path().join("README.md"), "# test").unwrap();
        git(tmp.path(), &["add", "."]).unwrap();
        git(tmp.path(), &["commit", "-m", "init"]).unwrap();
        tmp
    }

    #[test]
    fn test_is_git_repo() {
        let repo = setup_repo();
        assert!(is_git_repo(repo.path()));
        let tmp = tempfile::tempdir().unwrap();
        assert!(!is_git_repo(tmp.path()));
    }

    #[test]
    fn test_create_remove_worktree() {
        let repo = setup_repo();
        let wt = create_worktree(repo.path(), "alice", "agent/alice").unwrap();
        assert!(wt.exists());
        assert!(is_git_repo(&wt));
        // Idempotent
        let wt2 = create_worktree(repo.path(), "alice", "agent/alice").unwrap();
        assert_eq!(wt, wt2);
        remove_worktree(repo.path(), "alice").unwrap();
        assert!(!wt.exists());
    }

    #[test]
    fn test_list_worktrees() {
        let repo = setup_repo();
        create_worktree(repo.path(), "a1", "agent/a1").unwrap();
        create_worktree(repo.path(), "a2", "agent/a2").unwrap();
        let wts = list_worktrees(repo.path());
        assert_eq!(wts.len(), 2);
    }

    #[test]
    fn test_merge_preview_and_squash() {
        let repo = setup_repo();
        let wt = create_worktree(repo.path(), "dev", "agent/dev").unwrap();
        std::fs::write(wt.join("new.txt"), "hello").unwrap();
        git(&wt, &["add", "."]).unwrap();
        git(&wt, &["commit", "-m", "add new.txt"]).unwrap();
        let preview = merge_preview(repo.path(), "agent/dev").unwrap();
        assert!(preview.files_changed > 0);
        assert!(!preview.has_conflicts);
        squash_merge(repo.path(), "agent/dev", "merge dev work").unwrap();
        assert!(repo.path().join("new.txt").exists());
    }
}
