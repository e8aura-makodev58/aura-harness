//! The integrator: merges one task branch into the main branch, runs the
//! acceptance checks on the merge result, and moves the main branch only if
//! nothing changed underneath it. Runs on its own thread, so a long test run
//! never blocks the rest of the daemon.

use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct LandRequest {
    pub repo: PathBuf,
    /// Detached worktree where the merge and checks run. Removed afterwards.
    pub scratch: PathBuf,
    pub main_branch: String,
    pub branch: String,
    pub title: String,
    pub checks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Landed {
    Merged {
        sha: String,
    },
    /// Files that conflict with the main branch. The merge was aborted.
    Conflict(Vec<String>),
    /// An acceptance command failed on the merge result.
    CheckFailed {
        command: String,
        output: String,
    },
    /// The main branch moved, or could not be fast-forwarded. Safe to retry.
    Stale,
    Error(String),
}

const OUTPUT_TAIL: usize = 4000;

pub fn land(req: &LandRequest) -> Landed {
    let outcome = land_inner(req);
    let _ = aura_git::remove_worktree(&req.repo, &req.scratch);
    outcome
}

fn land_inner(req: &LandRequest) -> Landed {
    let base = match aura_git::rev_parse(&req.repo, &req.main_branch) {
        Ok(sha) => sha,
        Err(e) => return Landed::Error(e.to_string()),
    };
    if let Err(e) = aura_git::add_detached_worktree(&req.repo, &req.scratch, &base) {
        return Landed::Error(e.to_string());
    }

    let message = format!("Merge {}: {}", req.branch, req.title);
    match aura_git::merge_no_ff(&req.scratch, &req.branch, &message) {
        Ok(aura_git::MergeOutcome::Merged) => {}
        Ok(aura_git::MergeOutcome::Conflict(files)) => return Landed::Conflict(files),
        Err(e) => return Landed::Error(e.to_string()),
    }

    for check in &req.checks {
        if let Err((command, output)) = run_check(&req.scratch, check) {
            return Landed::CheckFailed { command, output };
        }
    }

    let merged = match aura_git::rev_parse(&req.scratch, "HEAD") {
        Ok(sha) => sha,
        Err(e) => return Landed::Error(e.to_string()),
    };

    // The main branch may have moved while the checks ran.
    match aura_git::rev_parse(&req.repo, &req.main_branch) {
        Ok(current) if current == base => {}
        _ => return Landed::Stale,
    }

    let on_main = matches!(
        aura_git::current_branch(&req.repo),
        Ok(Some(ref b)) if *b == req.main_branch
    );
    let landed = if on_main {
        // The checkout is on the main branch, so move it with a fast-forward.
        // That keeps the working tree in step. Refuse when it has local changes.
        match aura_git::is_clean(&req.repo) {
            Ok(true) => aura_git::fast_forward(&req.repo, &merged),
            _ => return Landed::Stale,
        }
    } else {
        aura_git::update_ref_cas(
            &req.repo,
            &format!("refs/heads/{}", req.main_branch),
            &merged,
            &base,
        )
    };
    match landed {
        Ok(()) => Landed::Merged { sha: merged },
        Err(_) => Landed::Stale,
    }
}

/// Runs one acceptance command as an argv (no shell) in `dir`.
fn run_check(dir: &PathBuf, command: &str) -> Result<(), (String, String)> {
    let mut parts = command.split_whitespace();
    let Some(program) = parts.next() else {
        return Ok(());
    };
    let output = Command::new(program)
        .args(parts)
        .current_dir(dir)
        .output()
        .map_err(|e| (command.to_string(), format!("cannot start {program}: {e}")))?;
    if output.status.success() {
        return Ok(());
    }
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    let tail: String = text
        .chars()
        .rev()
        .take(OUTPUT_TAIL)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Err((command.to_string(), tail))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        aura_git::run(p, &["init", "-q", "-b", "main"]).unwrap();
        std::fs::write(p.join("README.md"), "hi\n").unwrap();
        aura_git::commit_all(p, "init").unwrap();
        dir
    }

    fn branch_with_file(repo: &std::path::Path, name: &str, file: &str) {
        let wt_root = tempfile::tempdir().unwrap();
        let wt = wt_root.path().join(name);
        aura_git::add_worktree_for_branch(repo, &wt, name, "main").unwrap();
        std::fs::write(wt.join(file), "x\n").unwrap();
        aura_git::commit_all(&wt, "work").unwrap();
        aura_git::remove_worktree(repo, &wt).unwrap();
    }

    fn request(repo: &std::path::Path, branch: &str, checks: &[&str]) -> LandRequest {
        LandRequest {
            repo: repo.to_path_buf(),
            scratch: repo.parent().unwrap().join(format!("scratch-{branch}")),
            main_branch: "main".into(),
            branch: branch.into(),
            title: "test".into(),
            checks: checks.iter().map(|c| c.to_string()).collect(),
        }
    }

    #[test]
    fn green_branch_lands_on_main_checkout() {
        let dir = init_repo();
        let repo = dir.path();
        branch_with_file(repo, "aura/T-1", "feature.txt");
        let outcome = land(&request(repo, "aura/T-1", &["true"]));
        assert!(matches!(outcome, Landed::Merged { .. }), "{outcome:?}");
        assert!(repo.join("feature.txt").exists(), "working tree updated");
    }

    #[test]
    fn failing_check_blocks_landing() {
        let dir = init_repo();
        let repo = dir.path();
        branch_with_file(repo, "aura/T-2", "bad.txt");
        let before = aura_git::rev_parse(repo, "main").unwrap();
        let outcome = land(&request(repo, "aura/T-2", &["false"]));
        assert!(matches!(outcome, Landed::CheckFailed { .. }), "{outcome:?}");
        assert_eq!(aura_git::rev_parse(repo, "main").unwrap(), before);
    }

    #[test]
    fn conflict_is_reported_and_main_untouched() {
        let dir = init_repo();
        let repo = dir.path();
        branch_with_file(repo, "aura/T-3", "README.md");
        // Change README on main after the branch was cut.
        std::fs::write(repo.join("README.md"), "main side\n").unwrap();
        aura_git::commit_all(repo, "main edit").unwrap();
        let wt_root = tempfile::tempdir().unwrap();
        let wt = wt_root.path().join("T-3");
        aura_git::add_worktree_for_branch(repo, &wt, "aura/T-3", "main").unwrap();
        std::fs::write(wt.join("README.md"), "branch side\n").unwrap();
        aura_git::commit_all(&wt, "branch edit").unwrap();
        aura_git::remove_worktree(repo, &wt).unwrap();

        let before = aura_git::rev_parse(repo, "main").unwrap();
        match land(&request(repo, "aura/T-3", &["true"])) {
            Landed::Conflict(files) => assert_eq!(files, vec!["README.md".to_string()]),
            other => panic!("expected conflict, got {other:?}"),
        }
        assert_eq!(aura_git::rev_parse(repo, "main").unwrap(), before);
    }
}
