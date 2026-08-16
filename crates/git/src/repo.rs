//! Repository handle and the memory knobs that go with it.

use std::path::Path;

use crate::GitError;

/// An open repository.
///
/// Held per open tab and **dropped when the tab closes**. There is deliberately
/// no global repo registry: a cached `gix::Repository` keeps its pack cache and
/// mmapped packfiles alive, so a "helpful" cache of recently-opened repos is
/// really a leak with a friendly name.
pub struct Repo {
    inner: gix::Repository,
}

impl Repo {
    /// Open the repository containing `path`, walking up to find `.git`.
    pub fn open(path: &Path) -> Result<Self, GitError> {
        let inner = gix::discover(path).map_err(|e| match e {
            gix::discover::Error::Discover(_) => GitError::NotARepo {
                path: path.display().to_string(),
            },
            other => GitError::Open(other.to_string()),
        })?;

        let mut repo = Self { inner };
        repo.apply_memory_limits();
        Ok(repo)
    }

    /// Cap gix's in-process object cache.
    ///
    /// gix sizes this from `GITOXIDE_PACK_CACHE_MEMORY` or uses an unbounded
    /// default, which is exactly wrong for a long-lived GUI: scrolling a large
    /// history touches enormous numbers of objects and the cache grows to match.
    /// A fixed budget trades a little decompression work for a flat RSS curve.
    fn apply_memory_limits(&mut self) {
        const OBJECT_CACHE_BYTES: usize = 16 * 1024 * 1024;
        self.inner.object_cache_size(OBJECT_CACHE_BYTES);
    }

    pub fn inner(&self) -> &gix::Repository {
        &self.inner
    }

    /// Absolute path to the working tree, or `None` for a bare repository.
    pub fn workdir(&self) -> Option<&Path> {
        self.inner.workdir()
    }

    pub fn git_dir(&self) -> &Path {
        self.inner.git_dir()
    }

    /// Short name of the checked-out branch, or `None` when detached.
    pub fn current_branch(&self) -> Option<String> {
        self.inner
            .head_ref()
            .ok()
            .flatten()
            .map(|r| r.name().shorten().to_string())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::process::Command;

    /// Build a throwaway repo with `n` commits, newest message `commit n-1`,
    /// each commit writing `f.txt`.
    ///
    /// Built with `git fast-import` rather than a loop of `git add` + `git
    /// commit`. Still the real git binary, so the fixture is unarguably valid —
    /// but one process instead of three per commit. At the sizes the eviction
    /// tests use that was ~1200 spawns, which was slow everywhere and flaky on
    /// CI: a 400-commit fixture intermittently produced a repository where the
    /// revwalk yielded an id that could not then be read back.
    ///
    /// The exact cause was never reproduced locally. What is certain is that
    /// this version does the same work in a single deterministic pass, and that
    /// the fixture is not what those tests are trying to exercise.
    pub(crate) fn fixture(n: usize) -> tempfile::TempDir {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();

        let ok = Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(p)
            .output()
            .unwrap();
        assert!(ok.status.success(), "git init failed");

        if n == 0 {
            return dir;
        }

        let mut child = Command::new("git")
            .args(["fast-import", "--quiet"])
            .current_dir(p)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("git fast-import");
        {
            let mut w = std::io::BufWriter::new(child.stdin.as_mut().expect("piped stdin"));
            for i in 0..n {
                writeln!(w, "commit refs/heads/main").unwrap();
                writeln!(w, "mark :{}", i + 1).unwrap();
                // Fixed timestamps keep the fixture byte-identical between runs.
                writeln!(
                    w,
                    "committer Fixture <fixture@example.invalid> {} +0000",
                    1_600_000_000 + i
                )
                .unwrap();
                let msg = format!("commit {i}");
                writeln!(w, "data {}", msg.len()).unwrap();
                writeln!(w, "{msg}").unwrap();
                if i > 0 {
                    writeln!(w, "from :{i}").unwrap();
                }
                let blob = format!("{i}\n");
                writeln!(w, "M 100644 inline f.txt").unwrap();
                writeln!(w, "data {}", blob.len()).unwrap();
                write!(w, "{blob}").unwrap();
                writeln!(w).unwrap();
            }
            writeln!(w, "done").unwrap();
            w.flush().unwrap();
        }
        assert!(
            child.wait().expect("fast-import completes").success(),
            "git fast-import failed"
        );

        // fast-import writes objects and the ref but leaves the index and
        // working tree empty; tests that stage or diff need f.txt on disk.
        let ok = Command::new("git")
            .args(["reset", "-q", "--hard", "main"])
            .current_dir(p)
            .output()
            .unwrap();
        assert!(ok.status.success(), "git reset --hard failed");

        dir
    }

    #[test]
    fn opens_a_repo_and_reports_its_branch() {
        let dir = fixture(1);
        let repo = Repo::open(dir.path()).unwrap();
        assert_eq!(repo.current_branch().as_deref(), Some("main"));
        assert!(repo.workdir().is_some());
    }

    #[test]
    fn opens_from_a_subdirectory_by_walking_up() {
        let dir = fixture(1);
        let sub = dir.path().join("a/b/c");
        std::fs::create_dir_all(&sub).unwrap();
        assert!(Repo::open(&sub).is_ok(), "discovery must walk up to .git");
    }

    #[test]
    fn a_plain_directory_is_not_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            Repo::open(dir.path()),
            Err(GitError::NotARepo { .. })
        ));
    }
}
