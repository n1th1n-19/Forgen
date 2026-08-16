//! Unified diff parsing and patch synthesis.
//!
//! This module is the foundation of hunk- and line-level staging, not just of
//! the diff viewer. Staging part of a file means handing git a patch containing
//! exactly the selected changes — so the parser must round-trip: whatever it
//! reads, it must be able to write back in a form `git apply` accepts.
//!
//! Diffs come from the `git` binary rather than from gix. The viewer could use
//! either, but the *staging* path must produce patches git accepts byte for
//! byte, and the surest way to do that is to modify text git itself produced.
//! Rename detection, binary-file handling, `core.autocrlf`, and
//! `diff.noprefix` are all decided by the user's own git configuration this
//! way, rather than by our approximation of it.

use std::path::Path;
use std::process::Command;

use crate::{GitError, Repo};

/// Which tree the diff is taken against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffSource {
    /// Working tree vs index — the unstaged changes.
    Unstaged,
    /// Index vs HEAD — the staged changes.
    Staged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
    /// `\ No newline at end of file`. Carried because dropping it corrupts the
    /// patch when the final line is part of a selection.
    NoNewline,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    /// Line content without the leading marker.
    pub text: String,
    /// 1-based line number in the pre-image, if this line exists there.
    pub old_lineno: Option<u32>,
    /// 1-based line number in the post-image, if this line exists there.
    pub new_lineno: Option<u32>,
}

impl DiffLine {
    /// Render back to patch form, marker included.
    pub fn to_patch_line(&self) -> String {
        match self.kind {
            LineKind::Context => format!(" {}", self.text),
            LineKind::Added => format!("+{}", self.text),
            LineKind::Removed => format!("-{}", self.text),
            LineKind::NoNewline => "\\ No newline at end of file".to_string(),
        }
    }

    pub fn is_change(&self) -> bool {
        matches!(self.kind, LineKind::Added | LineKind::Removed)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    /// Text after the `@@` marker — usually the enclosing function.
    pub section: String,
    pub lines: Vec<DiffLine>,
}

impl Hunk {
    pub fn header(&self) -> String {
        let mut h = format!(
            "@@ -{},{} +{},{} @@",
            self.old_start, self.old_count, self.new_start, self.new_count
        );
        if !self.section.is_empty() {
            h.push(' ');
            h.push_str(&self.section);
        }
        h
    }

    pub fn added(&self) -> usize {
        self.lines
            .iter()
            .filter(|l| l.kind == LineKind::Added)
            .count()
    }

    pub fn removed(&self) -> usize {
        self.lines
            .iter()
            .filter(|l| l.kind == LineKind::Removed)
            .count()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiff {
    /// Path in the pre-image. Differs from `new_path` on a rename.
    pub old_path: String,
    pub new_path: String,
    /// Header lines between `diff --git` and the first `@@`, verbatim.
    ///
    /// Kept exactly as git emitted them because a synthesized patch must carry
    /// them unchanged — reconstructing `index`, `similarity index`, or mode
    /// lines by hand is how `git apply` starts rejecting patches.
    pub header: Vec<String>,
    pub hunks: Vec<Hunk>,
    pub is_binary: bool,
}

impl FileDiff {
    pub fn is_rename(&self) -> bool {
        self.old_path != self.new_path
    }

    pub fn added(&self) -> usize {
        self.hunks.iter().map(Hunk::added).sum()
    }

    pub fn removed(&self) -> usize {
        self.hunks.iter().map(Hunk::removed).sum()
    }
}

/// Diff one path, or the whole tree when `path` is `None`.
pub fn diff(
    repo: &Repo,
    source: DiffSource,
    path: Option<&Path>,
) -> Result<Vec<FileDiff>, GitError> {
    // Run from the working tree, not the git dir: `git diff` against the
    // worktree refuses to run with `-C .git` ("must be run in a work tree").
    let workdir = repo.workdir().ok_or_else(|| GitError::NotARepo {
        path: repo.git_dir().display().to_string(),
    })?;

    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(workdir);
    cmd.args(["--no-pager", "diff", "--no-color", "--no-ext-diff", "-U3"]);
    if source == DiffSource::Staged {
        cmd.arg("--cached");
    }
    if let Some(p) = path {
        cmd.arg("--").arg(p);
    }

    let out = cmd.output()?;
    if !out.status.success() {
        return Err(GitError::Walk(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }

    // Diff output is not guaranteed UTF-8 — a file can hold arbitrary bytes.
    // Lossy conversion keeps the viewer working on such a file instead of
    // failing the whole refresh; the staging path refuses non-UTF-8 separately.
    Ok(parse(&String::from_utf8_lossy(&out.stdout)))
}

/// Parse `git diff` output into per-file diffs.
pub fn parse(text: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let (old_path, new_path) = split_diff_paths(rest);
            files.push(FileDiff {
                old_path,
                new_path,
                header: vec![line.to_string()],
                hunks: Vec::new(),
                is_binary: false,
            });
            continue;
        }

        let Some(file) = files.last_mut() else {
            // Output before any `diff --git` header: not something we produced.
            continue;
        };

        if line.starts_with("@@") {
            if let Some(hunk) = parse_hunk_header(line) {
                file.hunks.push(hunk);
            }
            continue;
        }

        if file.hunks.is_empty() {
            // Still in the per-file header block.
            if line.starts_with("Binary files") || line.starts_with("GIT binary patch") {
                file.is_binary = true;
            }
            // `rename from`/`rename to` are authoritative where the
            // `diff --git a/x b/y` line is ambiguous — a path containing " b/"
            // cannot be split reliably.
            if let Some(p) = line.strip_prefix("rename from ") {
                file.old_path = p.to_string();
            }
            if let Some(p) = line.strip_prefix("rename to ") {
                file.new_path = p.to_string();
            }
            file.header.push(line.to_string());
            continue;
        }

        let hunk = file.hunks.last_mut().expect("hunks is non-empty");
        let (kind, text) = match line.as_bytes().first() {
            Some(b'+') => (LineKind::Added, &line[1..]),
            Some(b'-') => (LineKind::Removed, &line[1..]),
            Some(b' ') => (LineKind::Context, &line[1..]),
            Some(b'\\') => (LineKind::NoNewline, ""),
            // An empty line inside a hunk is a context line whose trailing
            // space git omitted. Treating it as a header would silently drop it.
            None => (LineKind::Context, ""),
            _ => continue,
        };

        // Numbering is derived rather than parsed: each line advances the old
        // side, the new side, or both, from the hunk's declared start.
        let (old_lineno, new_lineno) = match kind {
            LineKind::Context => {
                let o = hunk.old_start + count_side(hunk, true);
                let n = hunk.new_start + count_side(hunk, false);
                (Some(o), Some(n))
            }
            LineKind::Added => (None, Some(hunk.new_start + count_side(hunk, false))),
            LineKind::Removed => (Some(hunk.old_start + count_side(hunk, true)), None),
            LineKind::NoNewline => (None, None),
        };

        hunk.lines.push(DiffLine {
            kind,
            text: text.to_string(),
            old_lineno,
            new_lineno,
        });
    }

    files
}

/// Lines consumed so far on one side of a hunk.
fn count_side(hunk: &Hunk, old: bool) -> u32 {
    hunk.lines
        .iter()
        .filter(|l| match l.kind {
            LineKind::Context => true,
            LineKind::Added => !old,
            LineKind::Removed => old,
            LineKind::NoNewline => false,
        })
        .count() as u32
}

/// Split `a/path b/path` from a `diff --git` line.
///
/// Ambiguous by construction when a path contains a space — git quotes such
/// paths, but only when it must. The `rename from`/`rename to` lines that
/// follow are authoritative and override whatever this guesses.
fn split_diff_paths(rest: &str) -> (String, String) {
    let strip = |s: &str| -> String {
        s.strip_prefix("a/")
            .or_else(|| s.strip_prefix("b/"))
            .unwrap_or(s)
            .trim_matches('"')
            .to_string()
    };

    if let Some(mid) = rest.find(" b/") {
        let (a, b) = rest.split_at(mid);
        return (strip(a), strip(&b[1..]));
    }
    let mut parts = rest.splitn(2, ' ');
    (
        strip(parts.next().unwrap_or_default()),
        strip(parts.next().unwrap_or_default()),
    )
}

fn parse_hunk_header(line: &str) -> Option<Hunk> {
    // @@ -old_start,old_count +new_start,new_count @@ section
    let body = line.strip_prefix("@@ ")?;
    let end = body.find(" @@")?;
    let (ranges, tail) = body.split_at(end);
    let section = tail
        .strip_prefix(" @@")
        .unwrap_or("")
        .trim_start()
        .to_string();

    let mut it = ranges.split_whitespace();
    let (old_start, old_count) = parse_range(it.next()?.strip_prefix('-')?)?;
    let (new_start, new_count) = parse_range(it.next()?.strip_prefix('+')?)?;

    Some(Hunk {
        old_start,
        old_count,
        new_start,
        new_count,
        section,
        lines: Vec::new(),
    })
}

/// `start,count` or a bare `start`, where the count defaults to 1.
fn parse_range(s: &str) -> Option<(u32, u32)> {
    match s.split_once(',') {
        Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
diff --git a/src/main.rs b/src/main.rs
index 83db48f..bf269f4 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,5 +1,6 @@ fn main() {
 use std::io;
-let x = 1;
+let x = 2;
+let y = 3;
 println!(\"hi\");
 done
";

    #[test]
    fn parses_a_single_file_with_one_hunk() {
        let files = parse(SAMPLE);
        assert_eq!(files.len(), 1);

        let f = &files[0];
        assert_eq!(f.old_path, "src/main.rs");
        assert_eq!(f.new_path, "src/main.rs");
        assert!(!f.is_rename());
        assert!(!f.is_binary);
        assert_eq!(f.added(), 2);
        assert_eq!(f.removed(), 1);

        let h = &f.hunks[0];
        assert_eq!((h.old_start, h.old_count), (1, 5));
        assert_eq!((h.new_start, h.new_count), (1, 6));
        assert_eq!(h.section, "fn main() {");
        assert_eq!(h.lines.len(), 6);
    }

    #[test]
    fn assigns_line_numbers_to_the_correct_side() {
        let f = &parse(SAMPLE)[0];
        let l = &f.hunks[0].lines;

        // context: present on both sides
        assert_eq!((l[0].old_lineno, l[0].new_lineno), (Some(1), Some(1)));
        // removal: old side only
        assert_eq!((l[1].old_lineno, l[1].new_lineno), (Some(2), None));
        // additions: new side only, advancing
        assert_eq!((l[2].old_lineno, l[2].new_lineno), (None, Some(2)));
        assert_eq!((l[3].old_lineno, l[3].new_lineno), (None, Some(3)));
        // context after the change: both sides, now offset
        assert_eq!((l[4].old_lineno, l[4].new_lineno), (Some(3), Some(4)));
    }

    #[test]
    fn hunk_header_round_trips() {
        let f = &parse(SAMPLE)[0];
        assert_eq!(f.hunks[0].header(), "@@ -1,5 +1,6 @@ fn main() {");
    }

    #[test]
    fn lines_round_trip_to_patch_form() {
        let f = &parse(SAMPLE)[0];
        let rendered: Vec<String> = f.hunks[0]
            .lines
            .iter()
            .map(DiffLine::to_patch_line)
            .collect();
        assert_eq!(
            rendered,
            [
                " use std::io;",
                "-let x = 1;",
                "+let x = 2;",
                "+let y = 3;",
                " println!(\"hi\");",
                " done",
            ]
        );
    }

    #[test]
    fn parses_multiple_files_and_multiple_hunks() {
        let text = "\
diff --git a/a.txt b/a.txt
index 1..2 100644
--- a/a.txt
+++ b/a.txt
@@ -1 +1 @@
-one
+ONE
@@ -10,2 +10,2 @@
-ten
+TEN
 eleven
diff --git a/b.txt b/b.txt
index 3..4 100644
--- a/b.txt
+++ b/b.txt
@@ -5 +5 @@
-five
+FIVE
";
        let files = parse(text);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].hunks.len(), 2);
        assert_eq!(files[1].hunks.len(), 1);
        // A bare `@@ -1 +1 @@` means a count of 1, not 0.
        assert_eq!(files[0].hunks[0].old_count, 1);
        assert_eq!(files[1].hunks[0].new_start, 5);
    }

    #[test]
    fn detects_renames_from_the_authoritative_lines() {
        let text = "\
diff --git a/old name.txt b/new name.txt
similarity index 92%
rename from old name.txt
rename to new name.txt
--- a/old name.txt
+++ b/new name.txt
@@ -1 +1 @@
-a
+b
";
        let f = &parse(text)[0];
        // The `diff --git` line cannot be split reliably when paths contain
        // spaces; the rename lines must win.
        assert_eq!(f.old_path, "old name.txt");
        assert_eq!(f.new_path, "new name.txt");
        assert!(f.is_rename());
    }

    #[test]
    fn flags_binary_files_and_leaves_them_without_hunks() {
        let text = "\
diff --git a/logo.png b/logo.png
index 1..2 100644
Binary files a/logo.png and b/logo.png differ
";
        let f = &parse(text)[0];
        assert!(f.is_binary);
        assert!(f.hunks.is_empty());
    }

    #[test]
    fn preserves_the_no_newline_marker() {
        let text = "\
diff --git a/a b/a
index 1..2 100644
--- a/a
+++ b/a
@@ -1 +1 @@
-one
\\ No newline at end of file
+one
";
        let f = &parse(text)[0];
        let kinds: Vec<_> = f.hunks[0].lines.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            [LineKind::Removed, LineKind::NoNewline, LineKind::Added],
            "dropping the marker corrupts a patch whose last line is selected"
        );
        assert_eq!(
            f.hunks[0].lines[1].to_patch_line(),
            "\\ No newline at end of file"
        );
    }

    #[test]
    fn an_empty_context_line_is_context_not_a_header() {
        // git omits the trailing space on a blank context line.
        let text = "\
diff --git a/a b/a
index 1..2 100644
--- a/a
+++ b/a
@@ -1,3 +1,3 @@
 first

-third
+THIRD
";
        let f = &parse(text)[0];
        let kinds: Vec<_> = f.hunks[0].lines.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            [
                LineKind::Context,
                LineKind::Context,
                LineKind::Removed,
                LineKind::Added
            ]
        );
    }

    #[test]
    fn empty_input_yields_no_files() {
        assert!(parse("").is_empty());
        assert!(parse("\n\n").is_empty());
    }
}
