//! Merging, and reading the conflicts it leaves behind.
//!
//! A merge that conflicts is not a failure — it is the normal path into the
//! conflict resolver. [`merge`] therefore returns [`MergeOutcome::Conflicted`]
//! rather than an error, and the conflicted paths come back with it.
//!
//! Conflict *content* is read from the index rather than by parsing the
//! `<<<<<<<` markers in the working file. The index holds the three stages
//! (base, ours, theirs) as real blobs, so the three-way view gets exact
//! content. Marker parsing cannot recover the base at all under the default
//! `merge.conflictStyle`, and breaks outright on a file that legitimately
//! contains a line of seven angle brackets.

use std::process::Command;

use crate::{GitError, Repo};

#[derive(Debug, PartialEq, Eq)]
pub enum MergeOutcome {
    /// Merged and committed.
    Merged,
    /// Nothing to do; already contains the other branch.
    AlreadyUpToDate,
    /// Fast-forwarded without a merge commit.
    FastForward,
    /// Stopped with conflicts. The tree is mid-merge until resolved or aborted.
    Conflicted { paths: Vec<String> },
}

/// Merge `branch` into the current branch.
pub fn merge(repo: &Repo, branch: &str, no_ff: bool) -> Result<MergeOutcome, GitError> {
    let workdir = repo.workdir().ok_or_else(|| GitError::NotARepo {
        path: repo.git_dir().display().to_string(),
    })?;

    let mut args: Vec<&str> = vec!["merge"];
    if no_ff {
        args.push("--no-ff");
    }
    args.push(branch);

    let out = Command::new("git")
        .arg("-C")
        .arg(workdir)
        .args(&args)
        // A merge that needs a message must not open $EDITOR and hang forever.
        .env("GIT_EDITOR", "true")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;

    let stdout = String::from_utf8_lossy(&out.stdout);

    if out.status.success() {
        if stdout.contains("Already up to date") {
            return Ok(MergeOutcome::AlreadyUpToDate);
        }
        if stdout.contains("Fast-forward") {
            return Ok(MergeOutcome::FastForward);
        }
        return Ok(MergeOutcome::Merged);
    }

    // A non-zero exit with conflicts in the index is the expected path into the
    // resolver, not an error to surface.
    let paths = conflicted_paths(repo)?;
    if !paths.is_empty() {
        return Ok(MergeOutcome::Conflicted { paths });
    }

    Err(GitError::Walk(format!(
        "merge failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    )))
}

/// Abandon an in-progress merge and restore the pre-merge state.
pub fn abort(repo: &Repo) -> Result<(), GitError> {
    run(repo, &["merge", "--abort"])
}

/// Conclude a merge once every conflict is staged.
pub fn commit_merge(repo: &Repo) -> Result<(), GitError> {
    let workdir = repo.workdir().unwrap_or_else(|| repo.git_dir());
    let out = Command::new("git")
        .arg("-C")
        .arg(workdir)
        .args(["commit", "--no-edit"])
        .env("GIT_EDITOR", "true")
        .output()?;
    if !out.status.success() {
        return Err(GitError::Walk(format!(
            "could not conclude the merge: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

/// Paths currently in conflict.
pub fn conflicted_paths(repo: &Repo) -> Result<Vec<String>, GitError> {
    Ok(crate::status::status(repo)?
        .conflicted()
        .map(|e| e.path.clone())
        .collect())
}

/// The three sides of a conflicted file, read from the index stages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictSides {
    /// Stage 1 — the common ancestor. `None` when both sides added the file,
    /// which has no ancestor to show.
    pub base: Option<String>,
    /// Stage 2 — our version (the branch being merged into).
    pub ours: Option<String>,
    /// Stage 3 — their version (the branch being merged in).
    pub theirs: Option<String>,
}

/// Read the three stages of a conflicted path.
pub fn conflict_sides(repo: &Repo, path: &str) -> Result<ConflictSides, GitError> {
    Ok(ConflictSides {
        base: stage_blob(repo, 1, path),
        ours: stage_blob(repo, 2, path),
        theirs: stage_blob(repo, 3, path),
    })
}

/// Read one index stage. `None` when that stage does not exist — which is
/// meaningful, not an error: an add/add conflict has no stage 1, and a
/// delete/modify conflict is missing stage 2 or 3.
fn stage_blob(repo: &Repo, stage: u8, path: &str) -> Option<String> {
    let workdir = repo.workdir().unwrap_or_else(|| repo.git_dir());
    let out = Command::new("git")
        .arg("-C")
        .arg(workdir)
        .args(["show", &format!(":{stage}:{path}")])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Which side to keep when resolving without hand-editing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    Ours,
    Theirs,
}

/// Resolve a conflict by taking one side wholesale, then staging it.
pub fn resolve_with(repo: &Repo, path: &str, choice: Resolution) -> Result<(), GitError> {
    let flag = match choice {
        Resolution::Ours => "--ours",
        Resolution::Theirs => "--theirs",
    };
    run(repo, &["checkout", flag, "--", path])?;
    crate::stage::stage_file(repo, path)
}

/// Resolve by writing merged content the user produced, then staging it.
pub fn resolve_with_content(repo: &Repo, path: &str, content: &str) -> Result<(), GitError> {
    let workdir = repo.workdir().ok_or_else(|| GitError::NotARepo {
        path: repo.git_dir().display().to_string(),
    })?;
    std::fs::write(workdir.join(path), content)?;
    crate::stage::stage_file(repo, path)
}

fn run(repo: &Repo, args: &[&str]) -> Result<(), GitError> {
    let workdir = repo.workdir().unwrap_or_else(|| repo.git_dir());
    let out = Command::new("git")
        .arg("-C")
        .arg(workdir)
        .args(args)
        .env("GIT_EDITOR", "true")
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
    use crate::branch;
    use crate::repo::tests::fixture;
    use crate::Repo;

    fn commit_all(dir: &std::path::Path, msg: &str) {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["add", "-A"])
            .output()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["commit", "-q", "-m", msg])
            .env("GIT_AUTHOR_NAME", "F")
            .env("GIT_AUTHOR_EMAIL", "f@e.invalid")
            .env("GIT_COMMITTER_NAME", "F")
            .env("GIT_COMMITTER_EMAIL", "f@e.invalid")
            .output()
            .unwrap();
    }

    /// `main` and `other` both edit the same line of `f.txt`.
    fn conflicting() -> (tempfile::TempDir, Repo) {
        let dir = fixture(1);
        for (k, v) in [("user.name", "F"), ("user.email", "f@e.invalid")] {
            Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(["config", k, v])
                .output()
                .unwrap();
        }
        std::fs::write(dir.path().join("f.txt"), "base line\n").unwrap();
        commit_all(dir.path(), "base");

        let repo = Repo::open(dir.path()).unwrap();
        branch::create(&repo, "other", None, true).unwrap();
        std::fs::write(dir.path().join("f.txt"), "their line\n").unwrap();
        commit_all(dir.path(), "theirs");

        let repo = Repo::open(dir.path()).unwrap();
        branch::checkout(&repo, "main", false).unwrap();
        std::fs::write(dir.path().join("f.txt"), "our line\n").unwrap();
        commit_all(dir.path(), "ours");

        let repo = Repo::open(dir.path()).unwrap();
        (dir, repo)
    }

    #[test]
    fn a_clean_merge_reports_merged_or_fast_forward() {
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
        branch::create(&repo, "topic", None, true).unwrap();
        std::fs::write(dir.path().join("new.txt"), "content\n").unwrap();
        commit_all(dir.path(), "topic work");

        let repo = Repo::open(dir.path()).unwrap();
        branch::checkout(&repo, "main", false).unwrap();
        let repo = Repo::open(dir.path()).unwrap();

        let outcome = merge(&repo, "topic", false).unwrap();
        assert_eq!(outcome, MergeOutcome::FastForward);
        assert!(dir.path().join("new.txt").exists());
    }

    #[test]
    fn merging_an_ancestor_is_already_up_to_date() {
        let dir = fixture(2);
        let repo = Repo::open(dir.path()).unwrap();
        branch::create(&repo, "behind", Some("HEAD~1"), false).unwrap();
        assert_eq!(
            merge(&repo, "behind", false).unwrap(),
            MergeOutcome::AlreadyUpToDate
        );
    }

    #[test]
    fn a_conflict_is_an_outcome_not_an_error() {
        let (_d, repo) = conflicting();
        match merge(&repo, "other", false).unwrap() {
            MergeOutcome::Conflicted { paths } => assert_eq!(paths, ["f.txt"]),
            other => panic!("expected a conflict, got {other:?}"),
        }
        assert_eq!(
            branch::operation_in_progress(&repo).as_deref(),
            Some("merge"),
            "the tree stays mid-merge until resolved or aborted"
        );
    }

    #[test]
    fn conflict_sides_come_from_the_index_with_the_base_intact() {
        let (_d, repo) = conflicting();
        merge(&repo, "other", false).unwrap();

        let sides = conflict_sides(&repo, "f.txt").unwrap();
        assert_eq!(sides.ours.as_deref(), Some("our line\n"));
        assert_eq!(sides.theirs.as_deref(), Some("their line\n"));
        assert_eq!(
            sides.base.as_deref(),
            Some("base line\n"),
            "the ancestor is only recoverable from the index, never from the \
             conflict markers in the working file"
        );
    }

    #[test]
    fn resolving_with_ours_keeps_our_content() {
        let (dir, repo) = conflicting();
        merge(&repo, "other", false).unwrap();

        resolve_with(&repo, "f.txt", Resolution::Ours).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "our line\n"
        );
        assert!(conflicted_paths(&repo).unwrap().is_empty());

        commit_merge(&repo).unwrap();
        assert_eq!(branch::operation_in_progress(&repo), None);
    }

    #[test]
    fn resolving_with_theirs_keeps_their_content() {
        let (dir, repo) = conflicting();
        merge(&repo, "other", false).unwrap();

        resolve_with(&repo, "f.txt", Resolution::Theirs).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "their line\n"
        );
    }

    #[test]
    fn resolving_with_hand_merged_content_works() {
        let (dir, repo) = conflicting();
        merge(&repo, "other", false).unwrap();

        resolve_with_content(&repo, "f.txt", "our line\ntheir line\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "our line\ntheir line\n"
        );
        assert!(conflicted_paths(&repo).unwrap().is_empty());

        commit_merge(&repo).unwrap();
        assert_eq!(branch::operation_in_progress(&repo), None);
    }

    #[test]
    fn abort_restores_the_pre_merge_state() {
        let (dir, repo) = conflicting();
        merge(&repo, "other", false).unwrap();
        assert!(!conflicted_paths(&repo).unwrap().is_empty());

        abort(&repo).unwrap();

        assert_eq!(branch::operation_in_progress(&repo), None);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "our line\n",
            "abort must put our side back exactly"
        );
        assert!(conflicted_paths(&repo).unwrap().is_empty());
    }

    #[test]
    fn an_add_add_conflict_has_no_base() {
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
        branch::create(&repo, "other", None, true).unwrap();
        std::fs::write(dir.path().join("both.txt"), "theirs\n").unwrap();
        commit_all(dir.path(), "their new file");

        let repo = Repo::open(dir.path()).unwrap();
        branch::checkout(&repo, "main", false).unwrap();
        std::fs::write(dir.path().join("both.txt"), "ours\n").unwrap();
        commit_all(dir.path(), "our new file");

        let repo = Repo::open(dir.path()).unwrap();
        merge(&repo, "other", false).unwrap();

        let sides = conflict_sides(&repo, "both.txt").unwrap();
        assert_eq!(
            sides.base, None,
            "two independent additions share no ancestor; the resolver must \
             handle a missing base rather than assume three stages"
        );
        assert_eq!(sides.ours.as_deref(), Some("ours\n"));
        assert_eq!(sides.theirs.as_deref(), Some("theirs\n"));
    }
}
