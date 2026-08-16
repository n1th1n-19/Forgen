//! Staging: whole files, single hunks, and individual lines.
//!
//! Partial staging works by synthesizing a patch containing only the selected
//! changes and feeding it to `git apply --cached`. Everything unselected has to
//! be rewritten as context, and the hunk header counts have to be recomputed to
//! match — get either wrong and git rejects the patch, or worse, accepts a
//! patch that stages something the user did not pick.
//!
//! The rules, which the tests below pin:
//!
//! * An unselected **addition** did not exist in the index, so it must vanish
//!   from the patch entirely. Leaving it as context claims a line is present
//!   that is not.
//! * An unselected **removal** is still present in the index, so it becomes a
//!   context line. Dropping it would shift every following line.
//! * `old_count` is the context plus removals; `new_count` is the context plus
//!   additions. Both are recounted after filtering, never carried over.

use std::io::Write;
use std::process::{Command, Stdio};

use crate::diff::{DiffLine, FileDiff, Hunk, LineKind};
use crate::{GitError, Repo};

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
    let patch = build_patch(file, selected)?;
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
    let patch = build_patch(file, selected)?;
    if patch.is_empty() {
        return Ok(());
    }
    run_git(
        repo,
        &["apply", "--cached", "-R", "--unidiff-zero", "-"],
        Some(&patch),
    )
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
pub fn build_patch(file: &FileDiff, selected: &[Vec<bool>]) -> Result<String, GitError> {
    if file.is_binary {
        return Err(GitError::Object(format!(
            "{} is binary; stage the whole file instead",
            file.new_path
        )));
    }

    let mut hunks = String::new();
    for (i, hunk) in file.hunks.iter().enumerate() {
        let picks = selected.get(i).map(Vec::as_slice).unwrap_or(&[]);
        if let Some(rendered) = filter_hunk(hunk, picks) {
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
fn filter_hunk(hunk: &Hunk, selected: &[bool]) -> Option<String> {
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
            // Not staging this addition: the index does not contain the line at
            // all, so it must be absent from the patch — not demoted to context.
            LineKind::Added => {}
            LineKind::Removed if picked => {
                any_change = true;
                kept.push(line.clone());
            }
            // Not staging this removal: the line is still in the index, so it
            // becomes context. Dropping it would misalign everything after.
            LineKind::Removed => kept.push(DiffLine {
                kind: LineKind::Context,
                ..line.clone()
            }),
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
        let patch = build_patch(&f, &selected).unwrap();

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
        let patch = build_patch(&f, &selected).unwrap();

        assert!(
            patch.contains(" drop me"),
            "an unstaged removal is still in the index and must be context:\n{patch}"
        );
        assert!(!patch.contains("-drop me"), "{patch}");
        assert!(patch.contains("+add me"));
        // context 3 old, context 3 + addition 1 = 4 new
        assert!(patch.contains("@@ -1,3 +1,4 @@"), "{patch}");
    }

    #[test]
    fn selecting_nothing_yields_an_empty_patch_not_an_error() {
        let f = sample();
        let selected = vec![vec![false; 4]];
        assert_eq!(build_patch(&f, &selected).unwrap(), "");
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
        let patch = build_patch(&f, &selected).unwrap();

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
        let patch = build_patch(&f, &selected).unwrap();

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

        let err = build_patch(&f, &[]).unwrap_err();
        assert!(err.to_string().contains("binary"), "{err}");
    }
}
