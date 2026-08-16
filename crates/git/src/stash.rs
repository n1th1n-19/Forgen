//! Stash management.
//!
//! `git stash` is a porcelain command with no plumbing equivalent that handles
//! untracked files, the index, and the reflog together — so this shells out,
//! like every other write path.

use std::process::Command;

use crate::{GitError, Repo};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stash {
    /// Position in the stack. `0` is the most recent.
    pub index: usize,
    /// Ref name, `stash@{0}`. Held rather than rebuilt, because the index
    /// shifts as entries are dropped and a stale reconstruction would address
    /// the wrong entry.
    pub name: String,
    pub message: String,
    /// Branch the stash was taken on, when git recorded one.
    pub branch: Option<String>,
}

pub fn list(repo: &Repo) -> Result<Vec<Stash>, GitError> {
    let out = git(repo, &["stash", "list", "--format=%gd%x00%gs"])?;
    let text = String::from_utf8_lossy(&out);

    Ok(text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(index, line)| {
            let (name, subject) = line.split_once('\0').unwrap_or((line, ""));
            // Subjects look like "WIP on main: 1a2b3c message" or
            // "On main: message" for an explicit -m.
            let branch = subject
                .strip_prefix("WIP on ")
                .or_else(|| subject.strip_prefix("On "))
                .and_then(|rest| rest.split_once(':'))
                .map(|(b, _)| b.to_string());
            let message = subject
                .split_once(": ")
                .map(|(_, m)| m.trim().to_string())
                .unwrap_or_else(|| subject.to_string());

            Stash {
                index,
                name: name.to_string(),
                message,
                branch,
            }
        })
        .collect())
}

/// Stash the working tree.
///
/// `include_untracked` matters more than it looks: without it a new file that
/// the user believes is stashed stays in the tree, and the next checkout either
/// carries it along or refuses.
pub fn push(
    repo: &Repo,
    message: Option<&str>,
    include_untracked: bool,
    keep_index: bool,
) -> Result<(), GitError> {
    let mut args: Vec<&str> = vec!["stash", "push"];
    if include_untracked {
        args.push("--include-untracked");
    }
    if keep_index {
        args.push("--keep-index");
    }
    if let Some(m) = message {
        args.push("-m");
        args.push(m);
    }
    git(repo, &args).map(|_| ())
}

/// Apply a stash and drop it.
pub fn pop(repo: &Repo, name: &str) -> Result<(), GitError> {
    git(repo, &["stash", "pop", name]).map(|_| ())
}

/// Apply a stash, keeping it on the stack.
pub fn apply(repo: &Repo, name: &str) -> Result<(), GitError> {
    git(repo, &["stash", "apply", name]).map(|_| ())
}

/// Discard a stash. Unrecoverable through the UI, though the reflog keeps it
/// briefly.
pub fn drop(repo: &Repo, name: &str) -> Result<(), GitError> {
    git(repo, &["stash", "drop", name]).map(|_| ())
}

/// Diff of what a stash would apply, for the preview pane.
pub fn show(repo: &Repo, name: &str) -> Result<Vec<crate::diff::FileDiff>, GitError> {
    let out = git(
        repo,
        &["stash", "show", "--patch", "--no-color", "-U3", name],
    )?;
    Ok(crate::diff::parse(&String::from_utf8_lossy(&out)))
}

fn git(repo: &Repo, args: &[&str]) -> Result<Vec<u8>, GitError> {
    let workdir = repo.workdir().ok_or_else(|| GitError::NotARepo {
        path: repo.git_dir().display().to_string(),
    })?;

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
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::tests::fixture;
    use crate::Repo;

    fn repo() -> (tempfile::TempDir, Repo) {
        let dir = fixture(1);
        for (k, v) in [("user.name", "F"), ("user.email", "f@e.invalid")] {
            Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(["config", k, v])
                .output()
                .unwrap();
        }
        let repo = Repo::open(dir.path()).unwrap();
        (dir, repo)
    }

    #[test]
    fn push_then_pop_restores_the_working_tree() {
        let (dir, repo) = repo();
        let f = dir.path().join("f.txt");
        let original = std::fs::read_to_string(&f).unwrap();

        std::fs::write(&f, "work in progress\n").unwrap();
        push(&repo, Some("wip"), false, false).unwrap();

        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            original,
            "stashing must restore the tracked file"
        );

        let stashes = list(&repo).unwrap();
        assert_eq!(stashes.len(), 1);
        assert_eq!(stashes[0].name, "stash@{0}");
        assert_eq!(stashes[0].message, "wip");
        assert_eq!(stashes[0].branch.as_deref(), Some("main"));

        pop(&repo, "stash@{0}").unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "work in progress\n");
        assert!(list(&repo).unwrap().is_empty(), "pop must drop the entry");
    }

    #[test]
    fn apply_keeps_the_entry_on_the_stack() {
        let (dir, repo) = repo();
        std::fs::write(dir.path().join("f.txt"), "changed\n").unwrap();
        push(&repo, Some("keep me"), false, false).unwrap();

        apply(&repo, "stash@{0}").unwrap();
        assert_eq!(list(&repo).unwrap().len(), 1, "apply must not drop");
    }

    #[test]
    fn untracked_files_are_only_stashed_when_asked() {
        let (dir, repo) = repo();
        let untracked = dir.path().join("new.txt");
        std::fs::write(&untracked, "fresh\n").unwrap();
        std::fs::write(dir.path().join("f.txt"), "edited\n").unwrap();

        push(&repo, Some("tracked only"), false, false).unwrap();
        assert!(
            untracked.exists(),
            "without --include-untracked the new file must stay put"
        );

        std::fs::write(dir.path().join("f.txt"), "edited again\n").unwrap();
        push(&repo, Some("everything"), true, false).unwrap();
        assert!(!untracked.exists(), "--include-untracked must take it");
    }

    #[test]
    fn stashes_stack_newest_first() {
        let (dir, repo) = repo();
        for msg in ["first", "second"] {
            std::fs::write(dir.path().join("f.txt"), format!("{msg}\n")).unwrap();
            push(&repo, Some(msg), false, false).unwrap();
        }

        let stashes = list(&repo).unwrap();
        assert_eq!(stashes.len(), 2);
        assert_eq!(stashes[0].message, "second", "index 0 is the most recent");
        assert_eq!(stashes[1].message, "first");
        assert_eq!(stashes[1].name, "stash@{1}");
    }

    #[test]
    fn drop_removes_only_the_named_entry() {
        let (dir, repo) = repo();
        for msg in ["a", "b"] {
            std::fs::write(dir.path().join("f.txt"), format!("{msg}\n")).unwrap();
            push(&repo, Some(msg), false, false).unwrap();
        }

        drop(&repo, "stash@{0}").unwrap();
        let left = list(&repo).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].message, "a");
    }

    #[test]
    fn show_returns_a_parsed_diff() {
        let (dir, repo) = repo();
        std::fs::write(dir.path().join("f.txt"), "stashed change\n").unwrap();
        push(&repo, Some("preview"), false, false).unwrap();

        let diffs = show(&repo, "stash@{0}").unwrap();
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].new_path, "f.txt");
        assert!(diffs[0].added() > 0);
    }

    #[test]
    fn listing_an_empty_stack_is_empty_not_an_error() {
        let (_d, repo) = repo();
        assert!(list(&repo).unwrap().is_empty());
    }
}
