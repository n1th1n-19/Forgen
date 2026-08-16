//! Branch operations and the dirty-tree guard around checkout.
//!
//! Switching branches is the operation most likely to lose someone's work, so
//! the guard is not optional: [`checkout`] refuses when the switch would
//! overwrite modified files, and the caller decides whether to stash, commit,
//! or force.

use std::process::Command;

use crate::status::{self, Change};
use crate::{GitError, Repo};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckoutBlocker {
    /// Files are modified and the target branch touches them.
    WouldOverwrite,
    /// A merge, rebase, or cherry-pick is in progress.
    OperationInProgress,
}

#[derive(Debug)]
pub enum CheckoutOutcome {
    Switched,
    /// Refused, with the paths that would have been lost.
    Blocked {
        reason: CheckoutBlocker,
        paths: Vec<String>,
    },
}

/// Switch to an existing branch.
///
/// Uses `git switch` rather than `git checkout`: `checkout` is overloaded to
/// also mean "discard file changes", and a mistyped branch name that happens to
/// match a path silently destroys that path's edits instead of erroring.
pub fn checkout(repo: &Repo, branch: &str, force: bool) -> Result<CheckoutOutcome, GitError> {
    if let Some(op) = operation_in_progress(repo) {
        return Ok(CheckoutOutcome::Blocked {
            reason: CheckoutBlocker::OperationInProgress,
            paths: vec![op],
        });
    }

    if !force {
        let dirty = dirty_paths(repo)?;
        if !dirty.is_empty() {
            // git decides for itself whether the switch actually conflicts, so
            // this is a dry run rather than our own guess at the answer.
            if let Err(paths) = try_switch(repo, branch, true) {
                return Ok(CheckoutOutcome::Blocked {
                    reason: CheckoutBlocker::WouldOverwrite,
                    paths,
                });
            }
        }
    }

    match try_switch(repo, branch, false) {
        Ok(()) => Ok(CheckoutOutcome::Switched),
        Err(paths) => Ok(CheckoutOutcome::Blocked {
            reason: CheckoutBlocker::WouldOverwrite,
            paths,
        }),
    }
}

fn try_switch(repo: &Repo, branch: &str, dry_run: bool) -> Result<(), Vec<String>> {
    let workdir = repo.workdir().unwrap_or_else(|| repo.git_dir());
    let mut args = vec!["switch"];
    if dry_run {
        // `--no-guess` keeps a typo from silently creating a branch from a
        // remote of the same name during what is meant to be a check.
        args.push("--no-guess");
    }
    args.push(branch);

    let out = Command::new("git")
        .arg("-C")
        .arg(workdir)
        .args(&args)
        .output()
        .map_err(|e| vec![e.to_string()])?;

    if dry_run {
        // There is no real `--dry-run` for switch, so a successful dry run has
        // actually switched. Switch back and report success.
        if out.status.success() {
            let _ = Command::new("git")
                .arg("-C")
                .arg(workdir)
                .args(["switch", "-"])
                .output();
            return Ok(());
        }
    } else if out.status.success() {
        return Ok(());
    }

    Err(parse_overwrite_paths(&String::from_utf8_lossy(&out.stderr)))
}

/// Pull the file list out of git's "would be overwritten" refusal.
fn parse_overwrite_paths(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .map(str::trim)
        .filter(|l| {
            !l.is_empty()
                && !l.starts_with("error:")
                && !l.starts_with("Please")
                && !l.starts_with("Aborting")
                && !l.starts_with("fatal:")
        })
        .map(str::to_string)
        .collect()
}

/// Paths with uncommitted modifications.
fn dirty_paths(repo: &Repo) -> Result<Vec<String>, GitError> {
    Ok(status::status(repo)?
        .entries
        .into_iter()
        .filter(|e| !e.untracked && !e.ignored && e.worktree != Change::Unmodified)
        .map(|e| e.path)
        .collect())
}

/// Name of an in-progress operation, if any.
///
/// Checked before switching because git's own refusal in this state is
/// cryptic, and because switching mid-rebase can strand the rebase state
/// directory with no obvious way back.
pub fn operation_in_progress(repo: &Repo) -> Option<String> {
    let g = repo.git_dir();
    for (marker, name) in [
        ("MERGE_HEAD", "merge"),
        ("rebase-merge", "rebase"),
        ("rebase-apply", "rebase"),
        ("CHERRY_PICK_HEAD", "cherry-pick"),
        ("REVERT_HEAD", "revert"),
        ("BISECT_LOG", "bisect"),
    ] {
        if g.join(marker).exists() {
            return Some(name.to_string());
        }
    }
    None
}

/// Create a branch. `start_point` defaults to HEAD.
pub fn create(
    repo: &Repo,
    name: &str,
    start_point: Option<&str>,
    switch_to: bool,
) -> Result<(), GitError> {
    let mut args: Vec<&str> = if switch_to {
        vec!["switch", "-c", name]
    } else {
        vec!["branch", name]
    };
    if let Some(sp) = start_point {
        args.push(sp);
    }
    run(repo, &args)
}

/// Delete a branch. Without `force`, git refuses if it is not merged.
pub fn delete(repo: &Repo, name: &str, force: bool) -> Result<(), GitError> {
    run(repo, &["branch", if force { "-D" } else { "-d" }, name])
}

pub fn rename(repo: &Repo, from: &str, to: &str) -> Result<(), GitError> {
    run(repo, &["branch", "-m", from, to])
}

fn run(repo: &Repo, args: &[&str]) -> Result<(), GitError> {
    let workdir = repo.workdir().unwrap_or_else(|| repo.git_dir());
    let out = Command::new("git")
        .arg("-C")
        .arg(workdir)
        .args(args)
        .output()?;
    if !out.status.success() {
        return Err(GitError::Walk(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::tests::fixture;
    use crate::Repo;

    fn current(repo: &Repo) -> String {
        repo.current_branch().unwrap_or_default()
    }

    #[test]
    fn create_switch_rename_and_delete() {
        let dir = fixture(1);
        let repo = Repo::open(dir.path()).unwrap();

        create(&repo, "feature", None, false).unwrap();
        assert_eq!(current(&repo), "main", "plain create must not switch");

        assert!(matches!(
            checkout(&repo, "feature", false).unwrap(),
            CheckoutOutcome::Switched
        ));
        // Re-open: the cached HEAD ref does not see the switch.
        let repo = Repo::open(dir.path()).unwrap();
        assert_eq!(current(&repo), "feature");

        rename(&repo, "feature", "feature2").unwrap();
        let repo = Repo::open(dir.path()).unwrap();
        assert_eq!(current(&repo), "feature2");

        checkout(&repo, "main", false).unwrap();
        let repo = Repo::open(dir.path()).unwrap();
        delete(&repo, "feature2", true).unwrap();

        let names: Vec<_> = crate::refs::list(&repo)
            .unwrap()
            .into_iter()
            .map(|r| r.short)
            .collect();
        assert_eq!(names, ["main"]);
    }

    #[test]
    fn create_with_switch_moves_head() {
        let dir = fixture(1);
        let repo = Repo::open(dir.path()).unwrap();
        create(&repo, "topic", None, true).unwrap();

        let repo = Repo::open(dir.path()).unwrap();
        assert_eq!(current(&repo), "topic");
    }

    #[test]
    fn deleting_an_unmerged_branch_without_force_is_refused() {
        let dir = fixture(1);
        let repo = Repo::open(dir.path()).unwrap();
        create(&repo, "work", None, true).unwrap();

        let repo = Repo::open(dir.path()).unwrap();
        std::fs::write(dir.path().join("w.txt"), "x\n").unwrap();
        crate::stage::stage_file(&repo, "w.txt").unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["commit", "-q", "-m", "work"])
            .env("GIT_AUTHOR_NAME", "F")
            .env("GIT_AUTHOR_EMAIL", "f@e.invalid")
            .env("GIT_COMMITTER_NAME", "F")
            .env("GIT_COMMITTER_EMAIL", "f@e.invalid")
            .output()
            .unwrap();

        checkout(&repo, "main", false).unwrap();
        let repo = Repo::open(dir.path()).unwrap();

        assert!(
            delete(&repo, "work", false).is_err(),
            "unmerged work must not be deletable without force"
        );
        assert!(delete(&repo, "work", true).is_ok());
    }

    #[test]
    fn a_switch_that_would_lose_edits_is_blocked() {
        let dir = fixture(1);
        let repo = Repo::open(dir.path()).unwrap();

        // `main` has f.txt = "0\n". Give `other` a different f.txt.
        create(&repo, "other", None, true).unwrap();
        let repo = Repo::open(dir.path()).unwrap();
        std::fs::write(dir.path().join("f.txt"), "from other branch\n").unwrap();
        crate::stage::stage_file(&repo, "f.txt").unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["commit", "-q", "-m", "other"])
            .env("GIT_AUTHOR_NAME", "F")
            .env("GIT_AUTHOR_EMAIL", "f@e.invalid")
            .env("GIT_COMMITTER_NAME", "F")
            .env("GIT_COMMITTER_EMAIL", "f@e.invalid")
            .output()
            .unwrap();

        // Uncommitted edit to the same file, then try to leave.
        std::fs::write(dir.path().join("f.txt"), "unsaved work\n").unwrap();
        let repo = Repo::open(dir.path()).unwrap();

        match checkout(&repo, "main", false).unwrap() {
            CheckoutOutcome::Blocked { reason, .. } => {
                assert_eq!(reason, CheckoutBlocker::WouldOverwrite);
            }
            CheckoutOutcome::Switched => panic!("uncommitted work was silently discarded"),
        }

        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "unsaved work\n",
            "the guard must leave the working tree alone"
        );
    }

    #[test]
    fn a_clean_tree_switches_freely() {
        let dir = fixture(1);
        let repo = Repo::open(dir.path()).unwrap();
        create(&repo, "clean-topic", None, false).unwrap();

        assert!(matches!(
            checkout(&repo, "clean-topic", false).unwrap(),
            CheckoutOutcome::Switched
        ));
    }

    #[test]
    fn an_in_progress_merge_blocks_the_switch() {
        let dir = fixture(1);
        let repo = Repo::open(dir.path()).unwrap();
        create(&repo, "target", None, false).unwrap();

        // Simulate the state git leaves behind mid-merge.
        std::fs::write(repo.git_dir().join("MERGE_HEAD"), "deadbeef\n").unwrap();

        assert_eq!(operation_in_progress(&repo).as_deref(), Some("merge"));
        match checkout(&repo, "target", false).unwrap() {
            CheckoutOutcome::Blocked { reason, .. } => {
                assert_eq!(reason, CheckoutBlocker::OperationInProgress);
            }
            CheckoutOutcome::Switched => panic!("switched out of a half-finished merge"),
        }
    }

    #[test]
    fn no_operation_in_a_quiet_repo() {
        let dir = fixture(1);
        let repo = Repo::open(dir.path()).unwrap();
        assert_eq!(operation_in_progress(&repo), None);
    }
}
