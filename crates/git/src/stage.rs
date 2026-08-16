//! Staging: whole files, single hunks, and individual lines.
//!
//! Partial staging works by synthesizing a patch containing only the selected
//! changes and feeding it to `git apply --cached`. Everything unselected has to
//! be rewritten as context, and the hunk header counts have to be recomputed to
//! match — get either wrong and git rejects the patch, or worse, accepts a
//! patch that stages something the user did not pick.
//!
//! The rules depend on **which file the patch will be applied to**, and getting
//! this backwards produces `patch does not apply` at best and a silently wrong
//! result at worst.
//!
//! Against the **index** (`git apply --cached`, for staging and unstaging):
//!
//! * An unselected **addition** does not exist in the index, so it must vanish
//!   from the patch entirely. Leaving it as context claims a line is present
//!   that is not.
//! * An unselected **removal** is still present in the index, so it becomes a
//!   context line. Dropping it would shift every following line.
//!
//! Against the **working tree** (`git apply -R`, for discarding), the two rules
//! swap, because the working tree is the diff's post-image rather than its
//! pre-image: unselected additions *are* present and become context, unselected
//! removals are *not* and must be omitted.
//!
//! Either way `old_count` is context plus removals and `new_count` is context
//! plus additions, both recounted after filtering and never carried over.

use std::io::Write;
use std::process::{Command, Stdio};

use crate::diff::{DiffLine, FileDiff, Hunk, LineKind};
use crate::{GitError, Repo};

/// Which file the synthesized patch will be applied to.
///
/// Determines how *unselected* changes are rendered — see the module docs. The
/// two targets are mirror images, and using the wrong one makes `git apply`
/// reject the patch outright.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatchTarget {
    /// `git apply --cached`: the index is the diff's pre-image.
    Index,
    /// `git apply -R` with no `--cached`: the working tree is the post-image.
    Worktree,
}

/// Stage every change in a path.
pub fn stage_file(repo: &Repo, path: &str) -> Result<(), GitError> {
    // `--` guards against a path that looks like an option, and `-A` picks up
    // deletions, which a bare `git add <path>` misses on older git versions.
    run_git(repo, &["add", "-A", "--", path], None)
}

/// Unstage a path, leaving the working tree untouched.
pub fn unstage_file(repo: &Repo, path: &str) -> Result<(), GitError> {
    // `restore --staged` rather than `reset HEAD --`: it behaves correctly in a
    // repository with no commits yet, where HEAD does not resolve.
    run_git(repo, &["restore", "--staged", "--", path], None)
}

/// Discard unstaged changes to a path. Destructive and unrecoverable.
pub fn discard_file(repo: &Repo, path: &str) -> Result<(), GitError> {
    run_git(repo, &["restore", "--", path], None)
}

/// Stage exactly the selected lines of one file.
///
/// `selected` is indexed as `selected[hunk_index][line_index]`, matching the
/// shape of `file.hunks[..].lines[..]`. A `false` for a context line is
/// meaningless and ignored — context is always emitted.
pub fn stage_lines(repo: &Repo, file: &FileDiff, selected: &[Vec<bool>]) -> Result<(), GitError> {
    let patch = build_patch(file, selected, PatchTarget::Index)?;
    if patch.is_empty() {
        return Ok(());
    }
    run_git(
        repo,
        &["apply", "--cached", "--unidiff-zero", "-"],
        Some(&patch),
    )
}

/// Unstage exactly the selected lines of one file.
///
/// The same patch applied in reverse. `--cached -R` moves the change out of the
/// index without touching the working tree.
pub fn unstage_lines(repo: &Repo, file: &FileDiff, selected: &[Vec<bool>]) -> Result<(), GitError> {
    let patch = build_patch(file, selected, PatchTarget::Index)?;
    if patch.is_empty() {
        return Ok(());
    }
    run_git(
        repo,
        &["apply", "--cached", "-R", "--unidiff-zero", "-"],
        Some(&patch),
    )
}

/// Discard exactly the selected lines from the **working tree**.
///
/// Distinct from [`unstage_lines`], which moves changes out of the index and
/// leaves the file alone. This reverse-applies the patch to the file itself,
/// so the edits are gone — they were never committed, so nothing can recover
/// them. Callers must confirm first.
pub fn discard_lines(repo: &Repo, file: &FileDiff, selected: &[Vec<bool>]) -> Result<(), GitError> {
    // Worktree, not Index: the file being patched is the diff's post-image, so
    // unselected additions must appear as context and unselected removals must
    // be omitted — the exact opposite of the staging path.
    let patch = build_patch(file, selected, PatchTarget::Worktree)?;
    if patch.is_empty() {
        return Ok(());
    }
    // No `--cached`: this applies to the working tree, not the index.
    run_git(repo, &["apply", "-R", "--unidiff-zero", "-"], Some(&patch))
}

/// Stage one whole hunk.
pub fn stage_hunk(repo: &Repo, file: &FileDiff, hunk_index: usize) -> Result<(), GitError> {
    stage_lines(repo, file, &select_only_hunk(file, hunk_index))
}

pub fn unstage_hunk(repo: &Repo, file: &FileDiff, hunk_index: usize) -> Result<(), GitError> {
    unstage_lines(repo, file, &select_only_hunk(file, hunk_index))
}

fn select_only_hunk(file: &FileDiff, hunk_index: usize) -> Vec<Vec<bool>> {
    file.hunks
        .iter()
        .enumerate()
        .map(|(i, h)| vec![i == hunk_index; h.lines.len()])
        .collect()
}

/// Synthesize a patch containing only the selected changes.
///
/// Returns an empty string when nothing is selected, which callers treat as a
/// no-op rather than as an error — a click that selects nothing should do
/// nothing, not raise.
pub fn build_patch(
    file: &FileDiff,
    selected: &[Vec<bool>],
    target: PatchTarget,
) -> Result<String, GitError> {
    if file.is_binary {
        return Err(GitError::Object(format!(
            "{} is binary; stage the whole file instead",
            file.new_path
        )));
    }

    let mut hunks = String::new();
    for (i, hunk) in file.hunks.iter().enumerate() {
        let picks = selected.get(i).map(Vec::as_slice).unwrap_or(&[]);
        if let Some(rendered) = filter_hunk(hunk, picks, target) {
            hunks.push_str(&rendered);
        }
    }

    if hunks.is_empty() {
        return Ok(String::new());
    }

    let mut patch = String::new();
    for line in &file.header {
        patch.push_str(line);
        patch.push('\n');
    }
    // A rename header alone carries no `---`/`+++` pair; without them
    // `git apply` cannot tell which file the hunks belong to.
    if !file.header.iter().any(|l| l.starts_with("--- ")) {
        patch.push_str(&format!("--- a/{}\n", file.old_path));
        patch.push_str(&format!("+++ b/{}\n", file.new_path));
    }
    patch.push_str(&hunks);
    Ok(patch)
}

/// Render one hunk with only the selected changes, or `None` if it holds none.
fn filter_hunk(hunk: &Hunk, selected: &[bool], target: PatchTarget) -> Option<String> {
    let mut kept: Vec<DiffLine> = Vec::with_capacity(hunk.lines.len());
    let mut any_change = false;

    for (i, line) in hunk.lines.iter().enumerate() {
        let picked = selected.get(i).copied().unwrap_or(false);
        match line.kind {
            LineKind::Context => kept.push(line.clone()),
            LineKind::Added if picked => {
                any_change = true;
                kept.push(line.clone());
            }
            // Unselected addition. Absent from the index, present in the
            // working tree — so omit it for one target and demote it to context
            // for the other.
            LineKind::Added => {
                if target == PatchTarget::Worktree {
                    kept.push(DiffLine {
                        kind: LineKind::Context,
                        ..line.clone()
                    });
                }
            }
            LineKind::Removed if picked => {
                any_change = true;
                kept.push(line.clone());
            }
            // Unselected removal. Still in the index, already gone from the
            // working tree — the mirror of the case above.
            LineKind::Removed => {
                if target == PatchTarget::Index {
                    kept.push(DiffLine {
                        kind: LineKind::Context,
                        ..line.clone()
                    });
                }
            }
            // Only meaningful when attached to a line that survived.
            LineKind::NoNewline => {
                if kept.last().is_some_and(DiffLine::is_change) {
                    kept.push(line.clone());
                }
            }
        }
    }

    if !any_change {
        return None;
    }

    // Counts are recomputed, never inherited: the filtering above changed both
    // sides, and a stale count is the single most common reason `git apply`
    // rejects a synthesized patch.
    let old_count = kept
        .iter()
        .filter(|l| matches!(l.kind, LineKind::Context | LineKind::Removed))
        .count() as u32;
    let new_count = kept
        .iter()
        .filter(|l| matches!(l.kind, LineKind::Context | LineKind::Added))
        .count() as u32;

    let mut out = format!(
        "@@ -{},{} +{},{} @@",
        hunk.old_start, old_count, hunk.new_start, new_count
    );
    if !hunk.section.is_empty() {
        out.push(' ');
        out.push_str(&hunk.section);
    }
    out.push('\n');
    for line in &kept {
        out.push_str(&line.to_patch_line());
        out.push('\n');
    }
    Some(out)
}

fn run_git(repo: &Repo, args: &[&str], stdin_data: Option<&str>) -> Result<(), GitError> {
    let workdir = repo.workdir().unwrap_or_else(|| repo.git_dir());

    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(workdir).args(args);
    if stdin_data.is_some() {
        cmd.stdin(Stdio::piped());
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn()?;
    if let Some(data) = stdin_data {
        child
            .stdin
            .as_mut()
            .expect("stdin piped above")
            .write_all(data.as_bytes())?;
    }

    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(GitError::Walk(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{self, DiffSource};
    use crate::repo::tests::fixture;
    use crate::Repo;
    use std::process::Command;

    fn write(repo: &Repo, name: &str, content: &str) {
        std::fs::write(repo.workdir().unwrap().join(name), content).unwrap();
    }

    fn read_index(repo: &Repo, name: &str) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo.workdir().unwrap())
            .args(["show", &format!(":{name}")])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// A repo whose committed `f.txt` is lines one..five.
    fn five_lines() -> (tempfile::TempDir, Repo) {
        let dir = fixture(1);
        let repo = Repo::open(dir.path()).unwrap();
        write(&repo, "f.txt", "one\ntwo\nthree\nfour\nfive\n");
        stage_file(&repo, "f.txt").unwrap();
        Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["commit", "-q", "-m", "base"])
            .env("GIT_AUTHOR_NAME", "F")
            .env("GIT_AUTHOR_EMAIL", "f@e.invalid")
            .env("GIT_COMMITTER_NAME", "F")
            .env("GIT_COMMITTER_EMAIL", "f@e.invalid")
            .output()
            .unwrap();
        (dir, repo)
    }

    #[test]
    fn stage_and_unstage_a_whole_file() {
        let (_d, repo) = five_lines();
        write(&repo, "f.txt", "one\nTWO\nthree\nfour\nfive\n");

        stage_file(&repo, "f.txt").unwrap();
        assert_eq!(read_index(&repo, "f.txt"), "one\nTWO\nthree\nfour\nfive\n");

        unstage_file(&repo, "f.txt").unwrap();
        assert_eq!(
            read_index(&repo, "f.txt"),
            "one\ntwo\nthree\nfour\nfive\n",
            "unstaging must restore the index without touching the worktree"
        );
        assert_eq!(
            std::fs::read_to_string(repo.workdir().unwrap().join("f.txt")).unwrap(),
            "one\nTWO\nthree\nfour\nfive\n"
        );
    }

    #[test]
    fn stage_one_line_of_two_changes() {
        let (_d, repo) = five_lines();
        // Two separate edits, far enough apart to land in one hunk with -U3.
        write(&repo, "f.txt", "ONE\ntwo\nthree\nfour\nFIVE\n");

        let files = diff::diff(&repo, DiffSource::Unstaged, None).unwrap();
        let f = &files[0];

        // Pick only the first change (the `-one` / `+ONE` pair).
        let mut selected: Vec<Vec<bool>> =
            f.hunks.iter().map(|h| vec![false; h.lines.len()]).collect();
        for (i, l) in f.hunks[0].lines.iter().enumerate() {
            if l.text == "one" || l.text == "ONE" {
                selected[0][i] = true;
            }
        }

        stage_lines(&repo, f, &selected).unwrap();

        assert_eq!(
            read_index(&repo, "f.txt"),
            "ONE\ntwo\nthree\nfour\nfive\n",
            "only the selected line may reach the index"
        );
    }

    #[test]
    fn staging_a_hunk_stages_all_of_it_and_nothing_else() {
        let (_d, repo) = five_lines();
        // Far apart so -U3 yields two hunks.
        write(
            &repo,
            "f.txt",
            "ONE\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nTEN\n",
        );
        stage_file(&repo, "f.txt").unwrap();
        Command::new("git")
            .arg("-C")
            .arg(repo.workdir().unwrap())
            .args(["commit", "-q", "-m", "ten lines"])
            .env("GIT_AUTHOR_NAME", "F")
            .env("GIT_AUTHOR_EMAIL", "f@e.invalid")
            .env("GIT_COMMITTER_NAME", "F")
            .env("GIT_COMMITTER_EMAIL", "f@e.invalid")
            .output()
            .unwrap();

        write(
            &repo,
            "f.txt",
            "1ST\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\n10TH\n",
        );

        let files = diff::diff(&repo, DiffSource::Unstaged, None).unwrap();
        let f = &files[0];
        assert_eq!(f.hunks.len(), 2, "edits at both ends should be two hunks");

        stage_hunk(&repo, f, 0).unwrap();

        let staged = read_index(&repo, "f.txt");
        assert!(staged.starts_with("1ST\n"), "first hunk staged: {staged}");
        assert!(
            staged.ends_with("TEN\n"),
            "second hunk must be untouched: {staged}"
        );
    }

    #[test]
    fn discarding_lines_removes_them_from_the_working_tree() {
        let (_d, repo) = five_lines();
        // Two independent edits.
        write(&repo, "f.txt", "ONE\ntwo\nthree\nfour\nFIVE\n");

        let files = diff::diff(&repo, DiffSource::Unstaged, None).unwrap();
        let f = &files[0];

        // Discard only the first change.
        let mut selected: Vec<Vec<bool>> =
            f.hunks.iter().map(|h| vec![false; h.lines.len()]).collect();
        for (i, l) in f.hunks[0].lines.iter().enumerate() {
            if l.text == "one" || l.text == "ONE" {
                selected[0][i] = true;
            }
        }

        discard_lines(&repo, f, &selected).unwrap();

        assert_eq!(
            std::fs::read_to_string(repo.workdir().unwrap().join("f.txt")).unwrap(),
            "one\ntwo\nthree\nfour\nFIVE\n",
            "only the selected edit should be reverted; the other must survive"
        );
    }

    #[test]
    fn discarding_leaves_the_index_alone() {
        let (_d, repo) = five_lines();
        write(&repo, "f.txt", "STAGED\ntwo\nthree\nfour\nfive\n");
        stage_file(&repo, "f.txt").unwrap();
        // Further unstaged edit on top of the staged one.
        write(&repo, "f.txt", "STAGED\ntwo\nthree\nfour\nWORKTREE\n");

        let files = diff::diff(&repo, DiffSource::Unstaged, None).unwrap();
        let f = &files[0];
        let selected: Vec<Vec<bool>> = f.hunks.iter().map(|h| vec![true; h.lines.len()]).collect();

        discard_lines(&repo, f, &selected).unwrap();

        assert_eq!(
            read_index(&repo, "f.txt"),
            "STAGED\ntwo\nthree\nfour\nfive\n",
            "discarding a worktree change must not disturb what is staged"
        );
    }

    #[test]
    fn unstaging_a_hunk_reverses_it() {
        let (_d, repo) = five_lines();
        write(&repo, "f.txt", "ONE\ntwo\nthree\nfour\nfive\n");
        stage_file(&repo, "f.txt").unwrap();

        let staged = diff::diff(&repo, DiffSource::Staged, None).unwrap();
        unstage_hunk(&repo, &staged[0], 0).unwrap();

        assert_eq!(
            read_index(&repo, "f.txt"),
            "one\ntwo\nthree\nfour\nfive\n",
            "the index should be back at HEAD"
        );
    }

    // --- patch synthesis, no git process involved ---------------------------

    fn sample() -> FileDiff {
        diff::parse(
            "\
diff --git a/f.txt b/f.txt
index 1..2 100644
--- a/f.txt
+++ b/f.txt
@@ -1,4 +1,4 @@
 keep
-drop me
+add me
 tail
",
        )
        .remove(0)
    }

    #[test]
    fn unselected_addition_is_omitted_not_demoted_to_context() {
        let f = sample();
        // Select the removal only.
        let selected = vec![vec![false, true, false, false]];
        let patch = build_patch(&f, &selected, PatchTarget::Index).unwrap();

        assert!(
            !patch.contains("add me"),
            "an unstaged addition is not in the index and must not appear \
             as context:\n{patch}"
        );
        assert!(patch.contains("-drop me"));
        // context 2 + removal 1 = 3 old, context 2 + additions 0 = 2 new
        assert!(patch.contains("@@ -1,3 +1,2 @@"), "{patch}");
    }

    #[test]
    fn unselected_removal_becomes_context() {
        let f = sample();
        // Select the addition only.
        let selected = vec![vec![false, false, true, false]];
        let patch = build_patch(&f, &selected, PatchTarget::Index).unwrap();

        assert!(
            patch.contains(" drop me"),
            "an unstaged removal is still in the index and must be context:\n{patch}"
        );
        assert!(!patch.contains("-drop me"), "{patch}");
        assert!(patch.contains("+add me"));
        // context 3 old, context 3 + addition 1 = 4 new
        assert!(patch.contains("@@ -1,3 +1,4 @@"), "{patch}");
    }

    /// The two targets are mirror images. Rendering the same selection for the
    /// wrong one is what produced `patch does not apply` when discard was first
    /// written against the staging patch.
    #[test]
    fn worktree_target_mirrors_the_index_rules() {
        let f = sample();
        // Select the removal only, leaving the addition unselected.
        let selected = vec![vec![false, true, false, false]];

        let index = build_patch(&f, &selected, PatchTarget::Index).unwrap();
        let worktree = build_patch(&f, &selected, PatchTarget::Worktree).unwrap();

        // Index: the unselected addition is not in the index, so it is omitted.
        assert!(!index.contains("add me"), "index patch:\n{index}");
        // Worktree: the unselected addition *is* in the file, so it is context.
        assert!(
            worktree.contains(" add me"),
            "worktree patch must carry it as context:\n{worktree}"
        );

        // Counts follow. Index: context 2 + removal 1 = 3 old, 2 new.
        assert!(index.contains("@@ -1,3 +1,2 @@"), "{index}");
        // Worktree: context 3 + removal 1 = 4 old, context 3 new.
        assert!(worktree.contains("@@ -1,4 +1,3 @@"), "{worktree}");
    }

    #[test]
    fn worktree_target_omits_unselected_removals() {
        let f = sample();
        // Select the addition only.
        let selected = vec![vec![false, false, true, false]];
        let worktree = build_patch(&f, &selected, PatchTarget::Worktree).unwrap();

        assert!(
            !worktree.contains("drop me"),
            "an unselected removal is already gone from the working tree, so it \
             must not appear at all:\n{worktree}"
        );
        assert!(worktree.contains("+add me"), "{worktree}");
    }

    #[test]
    fn selecting_nothing_yields_an_empty_patch_not_an_error() {
        let f = sample();
        let selected = vec![vec![false; 4]];
        assert_eq!(build_patch(&f, &selected, PatchTarget::Index).unwrap(), "");
    }

    #[test]
    fn a_hunk_with_no_selection_is_dropped_entirely() {
        let mut f = sample();
        // Duplicate the hunk so there are two, and select only the second.
        let h = f.hunks[0].clone();
        f.hunks.push(Hunk {
            old_start: 20,
            new_start: 20,
            ..h
        });

        let selected = vec![vec![false; 4], vec![false, true, true, false]];
        let patch = build_patch(&f, &selected, PatchTarget::Index).unwrap();

        assert_eq!(
            patch.matches("@@ ").count(),
            1,
            "an unselected hunk must not appear at all:\n{patch}"
        );
        assert!(patch.contains("@@ -20,"), "{patch}");
    }

    #[test]
    fn the_patch_carries_the_original_header_verbatim() {
        let f = sample();
        let selected = vec![vec![false, true, true, false]];
        let patch = build_patch(&f, &selected, PatchTarget::Index).unwrap();

        assert!(patch.starts_with("diff --git a/f.txt b/f.txt\n"));
        assert!(
            patch.contains("index 1..2 100644"),
            "reconstructing the index line by hand is how apply starts \
             rejecting patches:\n{patch}"
        );
    }

    #[test]
    fn binary_files_are_refused_with_a_useful_message() {
        let f = diff::parse(
            "\
diff --git a/x.png b/x.png
index 1..2 100644
Binary files a/x.png and b/x.png differ
",
        )
        .remove(0);

        let err = build_patch(&f, &[], PatchTarget::Index).unwrap_err();
        assert!(err.to_string().contains("binary"), "{err}");
    }
}
