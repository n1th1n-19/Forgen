//! Shared UI state.
//!
//! `Rc<RefCell<_>>` rather than `Arc<Mutex<_>>` on purpose: everything here is
//! touched only from the GTK main loop. A thread-safe wrapper would advertise a
//! sharing story that does not exist and invite someone to reach for this from
//! a worker, where the `gix::Repository` it holds is not welcome anyway.

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;

use git::history::{HistoryWindow, Walker};
use git::{refs, GitError, Repo};

/// One open repository plus its history model.
pub struct RepoState {
    pub repo: Repo,
    pub window: HistoryWindow,
    pub refs: Vec<refs::Ref>,
}

impl RepoState {
    pub fn open(path: &Path, row_budget: usize) -> Result<Self, GitError> {
        let repo = Repo::open(path)?;
        let refs = refs::list(&repo)?;

        let mut window = HistoryWindow::with_budget(row_budget);

        // The spine is walked in full, once, here.
        //
        // An earlier version chunked this across idle ticks to keep the open
        // snappy, rebuilding the walker each chunk because `Walker` borrows the
        // repository. That was wrong, not merely wasteful: a fresh walker
        // restarts at HEAD, so every chunk re-appended ids from the beginning.
        // The spine filled with duplicates and grew without bound — RSS climbed
        // past 600MB on a 50k-commit repository and kept going.
        //
        // Chunking correctly needs the walker to outlive one call, which means
        // either a self-referential struct over the repository or moving the
        // walk to a worker thread. Neither is worth it yet: ids are 20 bytes
        // each and the walk is bounded by packfile reads, so 50k commits land
        // in milliseconds.
        //
        // ponytail: blocks the main loop for the length of one full revwalk.
        // At ~1.3M commits (the kernel) that becomes perceptible — when it
        // does, move this to a worker with a channel feeding `push_ids`, and
        // keep the walker owned there.
        window.fill_spine(Walker::from_head(&repo)?)?;

        Ok(Self { repo, window, refs })
    }

    /// Repository name and current branch, for the sidebar header.
    pub fn name_and_branch(&self) -> (String, String) {
        let name = self
            .repo
            .workdir()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "repository".into());
        let branch = self
            .repo
            .current_branch()
            .unwrap_or_else(|| "detached HEAD".into());
        (name, branch)
    }
}

/// Application-wide state. Cloneable handle; the data lives once.
#[derive(Clone)]
pub struct AppState {
    inner: Rc<RefCell<Option<RepoState>>>,
    row_budget: Rc<Cell<usize>>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            inner: Rc::new(RefCell::new(None)),
            row_budget: Rc::new(Cell::new(512)),
        }
    }

    /// Set the realized-row budget applied to the next repository opened.
    pub fn set_row_budget(&self, budget: usize) {
        self.row_budget.set(budget.max(1));
    }

    pub fn open_repo(&self, path: &Path) -> Result<(), GitError> {
        let state = RepoState::open(path, self.row_budget.get())?;
        *self.inner.borrow_mut() = Some(state);
        Ok(())
    }

    /// Close the repository and release its packfile mappings.
    ///
    /// Explicit rather than implicit: a "recently opened" cache of live
    /// `Repo` handles would keep every pack mmap and object cache alive, which
    /// is a leak wearing a convenience feature's clothes.
    pub fn close_repo(&self) {
        *self.inner.borrow_mut() = None;
    }

    pub fn is_open(&self) -> bool {
        self.inner.borrow().is_some()
    }

    /// Run `f` against the open repository, if there is one.
    pub fn with<T>(&self, f: impl FnOnce(&mut RepoState) -> T) -> Option<T> {
        self.inner.borrow_mut().as_mut().map(f)
    }

    pub fn rows(&self) -> u32 {
        self.inner
            .borrow()
            .as_ref()
            .map(|s| s.window.len() as u32)
            .unwrap_or(0)
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}
