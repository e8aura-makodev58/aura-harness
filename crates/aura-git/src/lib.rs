//! Git operations the harness needs, implemented by running the `git` CLI.
//! Every function takes the repository (or worktree) path explicitly, so the
//! daemon never depends on its own working directory.

use std::path::{Path, PathBuf};
use std::process::Command;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GitError {
    #[error("git {args} failed in {dir}: {stderr}")]
    Failed {
        args: String,
        dir: PathBuf,
        stderr: String,
    },
    #[error("cannot run git: {0}")]
    Spawn(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, GitError>;

/// Result of merging a branch into a checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    Merged,
    /// The merge was aborted. The listed files had conflicts.
    Conflict(Vec<String>),
}

/// Runs git with the given arguments and returns stdout. Commit identity is
/// set on the command line so that the daemon does not depend on the user's
/// git configuration.
pub fn run(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-c")
        .arg("user.name=aura")
        .arg("-c")
        .arg("user.email=aura@localhost")
        .arg("-c")
        .arg("commit.gpgsign=false")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(GitError::Failed {
            args: args.join(" "),
            dir: dir.to_path_buf(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub fn is_repo(dir: &Path) -> bool {
    run(dir, &["rev-parse", "--git-dir"]).is_ok()
}

/// Full commit id that `rev` points at.
pub fn rev_parse(dir: &Path, rev: &str) -> Result<String> {
    Ok(run(
        dir,
        &["rev-parse", "--verify", &format!("{rev}^{{commit}}")],
    )?
    .trim()
    .to_string())
}

pub fn branch_exists(dir: &Path, branch: &str) -> bool {
    run(
        dir,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_ok()
}

/// Name of the branch checked out in `dir`, or `None` when detached.
pub fn current_branch(dir: &Path) -> Result<Option<String>> {
    match run(dir, &["symbolic-ref", "--quiet", "--short", "HEAD"]) {
        Ok(name) => Ok(Some(name.trim().to_string())),
        Err(_) => Ok(None),
    }
}

/// True when there are no staged, unstaged or untracked changes.
pub fn is_clean(dir: &Path) -> Result<bool> {
    Ok(run(dir, &["status", "--porcelain"])?.trim().is_empty())
}

/// Adds a worktree on `branch`, creating the branch from `base` when it does
/// not exist yet.
pub fn add_worktree_for_branch(
    repo: &Path,
    worktree: &Path,
    branch: &str,
    base: &str,
) -> Result<()> {
    let wt = path_str(worktree);
    if branch_exists(repo, branch) {
        run(repo, &["worktree", "add", &wt, branch])?;
    } else {
        run(repo, &["worktree", "add", "-b", branch, &wt, base])?;
    }
    Ok(())
}

/// Adds a detached worktree at `rev`, for read-only or throwaway work.
pub fn add_detached_worktree(repo: &Path, worktree: &Path, rev: &str) -> Result<()> {
    run(
        repo,
        &["worktree", "add", "--detach", &path_str(worktree), rev],
    )?;
    Ok(())
}

/// Removes a worktree directory and its registration. Missing worktrees are
/// not an error, so cleanup can be repeated safely.
pub fn remove_worktree(repo: &Path, worktree: &Path) -> Result<()> {
    if worktree.exists() {
        let _ = run(
            repo,
            &["worktree", "remove", "--force", &path_str(worktree)],
        );
        if worktree.exists() {
            std::fs::remove_dir_all(worktree)?;
        }
    }
    run(repo, &["worktree", "prune"])?;
    Ok(())
}

/// Paths changed between `base` and `head` (both sides of the merge base).
pub fn changed_files(dir: &Path, base: &str, head: &str) -> Result<Vec<String>> {
    let out = run(dir, &["diff", "--name-only", &format!("{base}...{head}")])?;
    Ok(out
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// Paths with uncommitted changes in a worktree, including untracked files.
pub fn uncommitted_paths(dir: &Path) -> Result<Vec<String>> {
    let out = run(dir, &["status", "--porcelain=v1", "-uall"])?;
    let mut paths = Vec::new();
    for line in out.lines() {
        if line.len() < 4 {
            continue;
        }
        let path = &line[3..];
        // Renames show as "old -> new"; the new path is the one that matters.
        let path = path.rsplit(" -> ").next().unwrap_or(path);
        paths.push(unquote(path));
    }
    Ok(paths)
}

/// Stages everything and commits. Returns false when there was nothing to commit.
pub fn commit_all(dir: &Path, message: &str) -> Result<bool> {
    if is_clean(dir)? {
        return Ok(false);
    }
    run(dir, &["add", "-A"])?;
    run(dir, &["commit", "--no-verify", "-q", "-m", message])?;
    Ok(true)
}

/// Merges `branch` into the checkout at `dir` with a merge commit. On
/// conflict the merge is aborted and the conflicting paths are returned.
pub fn merge_no_ff(dir: &Path, branch: &str, message: &str) -> Result<MergeOutcome> {
    match run(
        dir,
        &["merge", "--no-ff", "--no-edit", "-m", message, branch],
    ) {
        Ok(_) => Ok(MergeOutcome::Merged),
        Err(err) => {
            let conflicts = run(dir, &["diff", "--name-only", "--diff-filter=U"])
                .unwrap_or_default()
                .lines()
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>();
            let _ = run(dir, &["merge", "--abort"]);
            if conflicts.is_empty() {
                Err(err)
            } else {
                Ok(MergeOutcome::Conflict(conflicts))
            }
        }
    }
}

/// Moves `reference` from `old` to `new`, failing if it is no longer at `old`.
pub fn update_ref_cas(repo: &Path, reference: &str, new: &str, old: &str) -> Result<()> {
    run(repo, &["update-ref", reference, new, old])?;
    Ok(())
}

/// Fast-forwards the checkout at `dir` to `rev`. Fails unless the checkout is
/// already at an ancestor of `rev`.
pub fn fast_forward(dir: &Path, rev: &str) -> Result<()> {
    run(dir, &["merge", "--ff-only", "--quiet", rev])?;
    Ok(())
}

/// Adds `entry` to the shared exclude file so every worktree ignores it.
pub fn exclude(repo: &Path, entry: &str) -> Result<()> {
    let common = run(repo, &["rev-parse", "--git-common-dir"])?;
    let mut path = PathBuf::from(common.trim());
    if path.is_relative() {
        path = repo.join(path);
    }
    let file = path.join("info").join("exclude");
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let current = std::fs::read_to_string(&file).unwrap_or_default();
    if !current.lines().any(|l| l.trim() == entry) {
        let mut text = current;
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(entry);
        text.push('\n');
        std::fs::write(&file, text)?;
    }
    Ok(())
}

/// Lists worktree paths git knows about.
pub fn worktree_paths(repo: &Path) -> Result<Vec<PathBuf>> {
    let out = run(repo, &["worktree", "list", "--porcelain"])?;
    Ok(out
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect())
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn unquote(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        trimmed[1..trimmed.len() - 1].to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repository with one commit on `main`.
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        run(p, &["init", "-q", "-b", "main"]).unwrap();
        std::fs::write(p.join("README.md"), "hello\n").unwrap();
        commit_all(p, "init").unwrap();
        dir
    }

    #[test]
    fn worktree_branch_commit_and_merge_round_trip() {
        let dir = repo();
        let repo = dir.path();
        let wt_root = tempfile::tempdir().unwrap();
        let wt = wt_root.path().join("T-1");

        add_worktree_for_branch(repo, &wt, "aura/T-1", "main").unwrap();
        std::fs::write(wt.join("feature.txt"), "x\n").unwrap();
        assert_eq!(
            uncommitted_paths(&wt).unwrap(),
            vec!["feature.txt".to_string()]
        );
        assert!(commit_all(&wt, "feature").unwrap());
        assert!(!commit_all(&wt, "nothing").unwrap());

        let changed = changed_files(repo, "main", "aura/T-1").unwrap();
        assert_eq!(changed, vec!["feature.txt".to_string()]);

        assert_eq!(
            merge_no_ff(repo, "aura/T-1", "merge T-1").unwrap(),
            MergeOutcome::Merged
        );
        assert!(repo.join("feature.txt").exists());

        remove_worktree(repo, &wt).unwrap();
        assert!(!wt.exists());
        assert!(
            branch_exists(repo, "aura/T-1"),
            "branch is kept after cleanup"
        );
    }

    #[test]
    fn conflicting_merge_is_aborted_and_reported() {
        let dir = repo();
        let repo = dir.path();
        let wt_root = tempfile::tempdir().unwrap();
        let wt = wt_root.path().join("T-2");
        add_worktree_for_branch(repo, &wt, "aura/T-2", "main").unwrap();
        std::fs::write(wt.join("README.md"), "branch\n").unwrap();
        commit_all(&wt, "branch edit").unwrap();

        std::fs::write(repo.join("README.md"), "main\n").unwrap();
        commit_all(repo, "main edit").unwrap();

        match merge_no_ff(repo, "aura/T-2", "merge").unwrap() {
            MergeOutcome::Conflict(files) => assert_eq!(files, vec!["README.md".to_string()]),
            other => panic!("expected conflict, got {other:?}"),
        }
        assert!(is_clean(repo).unwrap(), "merge was aborted");
        remove_worktree(repo, &wt).unwrap();
    }

    #[test]
    fn cas_update_refuses_stale_old_value() {
        let dir = repo();
        let repo = dir.path();
        let head = rev_parse(repo, "main").unwrap();
        let bogus = "0".repeat(40);
        assert!(update_ref_cas(repo, "refs/heads/main", &head, &bogus).is_err());
        assert!(update_ref_cas(repo, "refs/heads/main", &head, &head).is_ok());
    }

    #[test]
    fn exclude_entry_is_added_once() {
        let dir = repo();
        exclude(dir.path(), ".aura/").unwrap();
        exclude(dir.path(), ".aura/").unwrap();
        let text = std::fs::read_to_string(dir.path().join(".git/info/exclude")).unwrap();
        assert_eq!(text.matches(".aura/").count(), 1);
    }
}
