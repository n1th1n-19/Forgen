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

    /// Build a throwaway repo with `n` commits. Uses the real `git` binary so
    /// the fixture is unarguably valid rather than whatever we think gix writes.
    pub(crate) fn fixture(n: usize) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let run = |args: &[&str]| {
            let ok = Command::new("git")
                .args(args)
                .current_dir(p)
                .env("GIT_AUTHOR_NAME", "Fixture")
                .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
                .env("GIT_COMMITTER_NAME", "Fixture")
                .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
                .output()
                .unwrap();
            assert!(ok.status.success(), "git {args:?} failed");
        };
        run(&["init", "-q", "-b", "main"]);
        for i in 0..n {
            std::fs::write(p.join("f.txt"), format!("{i}\n")).unwrap();
            run(&["add", "f.txt"]);
            run(&["commit", "-q", "-m", &format!("commit {i}")]);
        }
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
