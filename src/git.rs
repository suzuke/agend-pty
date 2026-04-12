//! Git worktree management — shell out to `git` CLI.

use std::path::{Path, PathBuf};

fn git(repo_dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(repo_dir)
        .output()
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

pub fn has_git() -> bool {
    crate::paths::which("git").is_some()
}

fn sanitize_branch(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn worktree_dir(repo_dir: &Path, agent_name: &str) -> PathBuf {
    repo_dir
        .join(".agend")
        .join("worktrees")
        .join(sanitize_branch(agent_name))
}

fn branch_name(agent_name: &str) -> String {
    format!("agend/{}", sanitize_branch(agent_name))
}

pub fn create_worktree(
    repo_dir: &Path,
    agent_name: &str,
    custom_branch: Option<&str>,
) -> Result<PathBuf, String> {
    let wt_path = worktree_dir(repo_dir, agent_name);
    if wt_path.exists() {
        return Ok(wt_path);
    } // reuse on respawn
    let branch = custom_branch
        .map(String::from)
        .unwrap_or_else(|| branch_name(agent_name));
    // Create branch from HEAD if it doesn't exist (reuse if it does)
    if git(repo_dir, &["rev-parse", "--verify", &branch]).is_err() {
        git(repo_dir, &["branch", &branch])?;
    }
    if let Some(parent) = wt_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    git(
        repo_dir,
        &["worktree", "add", &wt_path.display().to_string(), &branch],
    )?;
    // Warn if .agend/ not in .gitignore
    check_gitignore(repo_dir);
    tracing::info!(agent = %agent_name, branch = %branch, "created worktree");
    Ok(wt_path)
}

fn check_gitignore(repo_dir: &Path) {
    let gi = repo_dir.join(".gitignore");
    let content = std::fs::read_to_string(&gi).unwrap_or_default();
    if !content.contains(".agend") {
        tracing::warn!("add '.agend/' to .gitignore to exclude worktrees from version control");
    }
}

pub fn remove_worktree(repo_dir: &Path, agent_name: &str) -> Result<(), String> {
    let wt_path = worktree_dir(repo_dir, agent_name);
    if !wt_path.exists() {
        return Ok(());
    }
    git(
        repo_dir,
        &[
            "worktree",
            "remove",
            "--force",
            &wt_path.display().to_string(),
        ],
    )?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    pub agent_name: String,
    pub path: PathBuf,
    pub branch: String,
}

pub fn list_worktrees(repo_dir: &Path) -> Vec<WorktreeInfo> {
    let wt_dir = repo_dir.join(".agend").join("worktrees");
    let entries = match std::fs::read_dir(&wt_dir) {
        Ok(e) => e,
        Err(_) => return vec![],
    };
    entries
        .flatten()
        .map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let path = e.path();
            let branch = git(&path, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
            WorktreeInfo {
                agent_name: name,
                path,
                branch,
            }
        })
        .collect()
}

pub fn warn_residual_worktrees(repo_dir: &Path) {
    let wts = list_worktrees(repo_dir);
    if !wts.is_empty() {
        tracing::warn!(
            count = wts.len(),
            "residual worktree(s) — run `agend-pty cleanup` to remove"
        );
        for wt in &wts {
            tracing::warn!(agent = %wt.agent_name, branch = %wt.branch, "residual worktree");
        }
    }
}

#[derive(Debug, Clone)]
pub struct MergePreview {
    pub diff_stat: String,
    pub files_changed: usize,
    pub has_conflicts: bool,
}

pub fn merge_preview(repo_dir: &Path, branch: &str) -> Result<MergePreview, String> {
    let diff_stat = git(repo_dir, &["diff", "--stat", &format!("HEAD...{branch}")])?;
    let files_changed = diff_stat.lines().count().saturating_sub(1);
    let base = git(repo_dir, &["merge-base", "HEAD", branch]).unwrap_or_default();
    let has_conflicts = if !base.is_empty() {
        git(repo_dir, &["merge-tree", &base, "HEAD", branch])
            .map(|out| out.contains("<<<<<<"))
            .unwrap_or(false)
    } else {
        false
    };
    Ok(MergePreview {
        diff_stat,
        files_changed,
        has_conflicts,
    })
}

pub fn squash_merge(repo_dir: &Path, branch: &str, message: &str) -> Result<(), String> {
    // Try squash merge
    if let Err(e) = git(repo_dir, &["merge", "--squash", branch]) {
        // Conflict — abort and report
        git(repo_dir, &["merge", "--abort"]).ok();
        let conflicts = git(repo_dir, &["diff", "--name-only", "--diff-filter=U"])
            .unwrap_or_else(|_| e.clone());
        return Err(format!("merge conflicts:\n{conflicts}"));
    }
    git(repo_dir, &["commit", "-m", message])?;
    tracing::info!(branch = %branch, "squash-merged");
    Ok(())
}

pub fn cleanup_worktrees(repo_dir: &Path) -> usize {
    let wts = list_worktrees(repo_dir);
    let mut removed = 0;
    for wt in &wts {
        if remove_worktree(repo_dir, &wt.agent_name).is_ok() {
            removed += 1;
        }
    }
    removed
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
    fn test_create_reuse_remove() {
        let repo = setup_repo();
        let wt = create_worktree(repo.path(), "alice", None).unwrap();
        assert!(wt.exists());
        assert!(wt.to_string_lossy().contains(".agend/worktrees/alice"));
        // Reuse on second call
        let wt2 = create_worktree(repo.path(), "alice", None).unwrap();
        assert_eq!(wt, wt2);
        remove_worktree(repo.path(), "alice").unwrap();
        assert!(!wt.exists());
    }

    #[test]
    fn test_list_and_cleanup() {
        let repo = setup_repo();
        create_worktree(repo.path(), "a1", None).unwrap();
        create_worktree(repo.path(), "a2", None).unwrap();
        assert_eq!(list_worktrees(repo.path()).len(), 2);
        assert_eq!(cleanup_worktrees(repo.path()), 2);
        assert_eq!(list_worktrees(repo.path()).len(), 0);
    }

    #[test]
    fn test_merge_preview_and_squash() {
        let repo = setup_repo();
        let wt = create_worktree(repo.path(), "dev", None).unwrap();
        std::fs::write(wt.join("new.txt"), "hello").unwrap();
        git(&wt, &["add", "."]).unwrap();
        git(&wt, &["commit", "-m", "add new.txt"]).unwrap();
        let preview = merge_preview(repo.path(), "agend/dev").unwrap();
        assert!(preview.files_changed > 0);
        assert!(!preview.has_conflicts);
        squash_merge(repo.path(), "agend/dev", "merge dev").unwrap();
        assert!(repo.path().join("new.txt").exists());
    }

    #[test]
    fn test_sanitize_branch() {
        assert_eq!(sanitize_branch("my agent!"), "my-agent-");
        assert_eq!(sanitize_branch("normal-name_1"), "normal-name_1");
    }
}
