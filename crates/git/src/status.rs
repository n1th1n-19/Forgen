//! Working-tree status.
//!
//! Read from `git status --porcelain=v2 -z` rather than from `gix-status`.
//!
//! The plan called for gix here, and gix would be faster — it avoids a process
//! spawn per refresh. Porcelain v2 wins anyway because it is a *documented,
//! stable* format that reflects the user's own configuration: `core.ignorecase`,
//! `core.autocrlf`, `.gitattributes` filters, rename detection thresholds, and
//! submodule state all come out already resolved. Reimplementing that agreement
//! is exactly the class of subtle divergence that makes a git GUI untrustworthy.
//!
//! `-z` because paths are arbitrary bytes: a filename containing a newline is
//! legal, and the line-based format quotes it in a way that must then be
//! unquoted. NUL-delimited output sidesteps the round trip entirely.
//!
//! ponytail: one `git status` process per refresh, ~10-20ms on a large tree.
//! If refresh-on-focus ever feels sluggish, move to `gix-status` behind this
//! same `Status` type and keep a differential test against this implementation.

use std::process::Command;

use crate::{GitError, Repo};

/// What happened to a path, on one side (index or worktree).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Unmodified,
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
}

impl Change {
    /// Decode one XY character from porcelain v2.
    fn from_code(c: u8) -> Self {
        match c {
            b'M' => Self::Modified,
            b'A' => Self::Added,
            b'D' => Self::Deleted,
            b'R' => Self::Renamed,
            b'C' => Self::Copied,
            b'T' => Self::TypeChanged,
            // '.' means unmodified on that side; anything unrecognised is
            // treated the same way rather than guessed at.
            _ => Self::Unmodified,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusEntry {
    pub path: String,
    /// Original path, for renames and copies.
    pub original_path: Option<String>,
    /// Change staged in the index.
    pub index: Change,
    /// Change in the working tree, not yet staged.
    pub worktree: Change,
    /// Both sides modified — a merge conflict.
    pub conflicted: bool,
    pub untracked: bool,
    pub ignored: bool,
}

impl StatusEntry {
    pub fn is_staged(&self) -> bool {
        !self.conflicted && self.index != Change::Unmodified
    }

    pub fn is_unstaged(&self) -> bool {
        self.untracked || self.conflicted || self.worktree != Change::Unmodified
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub entries: Vec<StatusEntry>,
    /// Current branch, or `None` when detached.
    pub branch: Option<String>,
    /// Upstream tracking branch, e.g. `origin/main`.
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
}

impl Status {
    pub fn staged(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries.iter().filter(|e| e.is_staged())
    }

    pub fn unstaged(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries.iter().filter(|e| e.is_unstaged())
    }

    pub fn conflicted(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries.iter().filter(|e| e.conflicted)
    }

    pub fn is_clean(&self) -> bool {
        self.entries.is_empty()
    }
}

pub fn status(repo: &Repo) -> Result<Status, GitError> {
    let workdir = repo.workdir().ok_or_else(|| GitError::NotARepo {
        path: repo.git_dir().display().to_string(),
    })?;

    let out = Command::new("git")
        .arg("-C")
        .arg(workdir)
        .args([
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=normal",
            "-z",
        ])
        .output()?;

    if !out.status.success() {
        return Err(GitError::Walk(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }

    Ok(parse(&out.stdout))
}

/// Parse NUL-delimited porcelain v2 output.
pub fn parse(bytes: &[u8]) -> Status {
    let mut status = Status::default();

    // Records are NUL-separated, but a rename record contains *two* paths
    // separated by an extra NUL — so records cannot simply be split and mapped.
    let mut fields = bytes.split(|b| *b == 0).peekable();

    while let Some(raw) = fields.next() {
        if raw.is_empty() {
            continue;
        }
        let record = String::from_utf8_lossy(raw);

        match record.as_bytes().first() {
            Some(b'#') => parse_header(&record, &mut status),
            Some(b'1') => {
                if let Some(e) = parse_ordinary(&record) {
                    status.entries.push(e);
                }
            }
            Some(b'2') => {
                // A rename or copy: the original path is the next field.
                let original = fields
                    .next()
                    .map(|b| String::from_utf8_lossy(b).into_owned());
                if let Some(mut e) = parse_ordinary(&record) {
                    e.original_path = original;
                    status.entries.push(e);
                }
            }
            Some(b'u') => {
                if let Some(e) = parse_unmerged(&record) {
                    status.entries.push(e);
                }
            }
            Some(b'?') => status.entries.push(StatusEntry {
                path: record[2..].to_string(),
                original_path: None,
                index: Change::Unmodified,
                worktree: Change::Unmodified,
                conflicted: false,
                untracked: true,
                ignored: false,
            }),
            Some(b'!') => status.entries.push(StatusEntry {
                path: record[2..].to_string(),
                original_path: None,
                index: Change::Unmodified,
                worktree: Change::Unmodified,
                conflicted: false,
                untracked: false,
                ignored: true,
            }),
            _ => {}
        }
    }

    status
}

fn parse_header(record: &str, status: &mut Status) {
    if let Some(head) = record.strip_prefix("# branch.head ") {
        // git reports the literal string "(detached)" rather than omitting it.
        status.branch = (head != "(detached)").then(|| head.to_string());
    } else if let Some(up) = record.strip_prefix("# branch.upstream ") {
        status.upstream = Some(up.to_string());
    } else if let Some(ab) = record.strip_prefix("# branch.ab ") {
        for part in ab.split_whitespace() {
            match part.as_bytes().first() {
                Some(b'+') => status.ahead = part[1..].parse().unwrap_or(0),
                Some(b'-') => status.behind = part[1..].parse().unwrap_or(0),
                _ => {}
            }
        }
    }
}

/// `1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>` — and `2` adds a score field.
fn parse_ordinary(record: &str) -> Option<StatusEntry> {
    let is_rename = record.starts_with('2');
    let mut parts = record.splitn(if is_rename { 10 } else { 9 }, ' ');

    parts.next()?; // record type
    let xy = parts.next()?.as_bytes();
    for _ in 0..6 {
        parts.next()?; // sub, mH, mI, mW, hH, hI
    }
    if is_rename {
        parts.next()?; // rename/copy score
    }
    let path = parts.next()?.to_string();

    Some(StatusEntry {
        path,
        original_path: None,
        index: Change::from_code(*xy.first()?),
        worktree: Change::from_code(*xy.get(1)?),
        conflicted: false,
        untracked: false,
        ignored: false,
    })
}

/// `u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>`
fn parse_unmerged(record: &str) -> Option<StatusEntry> {
    let mut parts = record.splitn(11, ' ');
    parts.next()?;
    let xy = parts.next()?.as_bytes();
    for _ in 0..8 {
        parts.next()?;
    }
    let path = parts.next()?.to_string();

    Some(StatusEntry {
        path,
        original_path: None,
        index: Change::from_code(*xy.first()?),
        worktree: Change::from_code(*xy.get(1)?),
        conflicted: true,
        untracked: false,
        ignored: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::tests::fixture;
    use crate::stage;
    use crate::Repo;

    // --- parsing, no process involved ---------------------------------------

    fn joined(records: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for r in records {
            out.extend_from_slice(r.as_bytes());
            out.push(0);
        }
        out
    }

    #[test]
    fn reads_branch_and_ahead_behind_from_the_header() {
        let s = parse(&joined(&[
            "# branch.oid abc123",
            "# branch.head main",
            "# branch.upstream origin/main",
            "# branch.ab +3 -2",
        ]));
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert_eq!(s.upstream.as_deref(), Some("origin/main"));
        assert_eq!((s.ahead, s.behind), (3, 2));
        assert!(s.is_clean());
    }

    #[test]
    fn detached_head_reports_no_branch() {
        let s = parse(&joined(&["# branch.head (detached)"]));
        assert_eq!(s.branch, None, "\"(detached)\" is a sentinel, not a name");
    }

    #[test]
    fn separates_staged_from_unstaged_on_the_xy_pair() {
        let s = parse(&joined(&[
            // staged modification, clean worktree
            "1 M. N... 100644 100644 100644 aaa bbb staged.rs",
            // unstaged modification only
            "1 .M N... 100644 100644 100644 ccc ddd dirty.rs",
            // both
            "1 MM N... 100644 100644 100644 eee fff both.rs",
        ]));

        let staged: Vec<_> = s.staged().map(|e| e.path.as_str()).collect();
        assert_eq!(staged, ["staged.rs", "both.rs"]);

        let unstaged: Vec<_> = s.unstaged().map(|e| e.path.as_str()).collect();
        assert_eq!(unstaged, ["dirty.rs", "both.rs"]);
    }

    #[test]
    fn a_rename_record_consumes_the_following_original_path() {
        // The extra NUL-separated field is why records cannot just be mapped.
        let s = parse(&joined(&[
            "2 R. N... 100644 100644 100644 aaa bbb R100 new.rs",
            "old.rs",
            "1 .M N... 100644 100644 100644 ccc ddd after.rs",
        ]));

        assert_eq!(s.entries.len(), 2, "the original path is not its own entry");
        assert_eq!(s.entries[0].path, "new.rs");
        assert_eq!(s.entries[0].original_path.as_deref(), Some("old.rs"));
        assert_eq!(s.entries[0].index, Change::Renamed);
        assert_eq!(
            s.entries[1].path, "after.rs",
            "parsing must resynchronise after a rename"
        );
    }

    #[test]
    fn unmerged_entries_are_flagged_conflicted() {
        let s = parse(&joined(&[
            "u UU N... 100644 100644 100644 100644 aaa bbb ccc conflict.rs",
        ]));
        let e = &s.entries[0];
        assert!(e.conflicted);
        assert_eq!(e.path, "conflict.rs");
        assert_eq!(s.conflicted().count(), 1);
        assert!(
            !e.is_staged(),
            "a conflicted path is not staged, whatever XY says"
        );
        assert!(e.is_unstaged(), "it must still show as needing attention");
    }

    #[test]
    fn untracked_and_ignored_are_distinguished() {
        let s = parse(&joined(&["? new.rs", "! target/"]));
        assert!(s.entries[0].untracked && !s.entries[0].ignored);
        assert!(s.entries[1].ignored && !s.entries[1].untracked);
        assert_eq!(
            s.unstaged().count(),
            1,
            "ignored files are not pending work"
        );
    }

    #[test]
    fn a_path_containing_spaces_survives() {
        let s = parse(&joined(&[
            "1 .M N... 100644 100644 100644 aaa bbb my documents/a file.txt",
        ]));
        assert_eq!(s.entries[0].path, "my documents/a file.txt");
    }

    #[test]
    fn empty_output_is_a_clean_tree() {
        assert!(parse(b"").is_clean());
    }

    // --- against the real git binary ----------------------------------------

    #[test]
    fn reports_real_staged_and_unstaged_changes() {
        let dir = fixture(1);
        let repo = Repo::open(dir.path()).unwrap();
        let wd = repo.workdir().unwrap().to_path_buf();

        std::fs::write(wd.join("staged.txt"), "a\n").unwrap();
        stage::stage_file(&repo, "staged.txt").unwrap();
        std::fs::write(wd.join("untracked.txt"), "b\n").unwrap();
        std::fs::write(wd.join("f.txt"), "modified\n").unwrap();

        let s = status(&repo).unwrap();
        assert_eq!(s.branch.as_deref(), Some("main"));

        let staged: Vec<_> = s.staged().map(|e| e.path.as_str()).collect();
        assert_eq!(staged, ["staged.txt"]);

        let unstaged: Vec<_> = s.unstaged().map(|e| e.path.as_str()).collect();
        assert!(unstaged.contains(&"f.txt"), "{unstaged:?}");
        assert!(unstaged.contains(&"untracked.txt"), "{unstaged:?}");
    }

    #[test]
    fn a_clean_tree_reports_clean() {
        let dir = fixture(1);
        let repo = Repo::open(dir.path()).unwrap();
        assert!(status(&repo).unwrap().is_clean());
    }
}
