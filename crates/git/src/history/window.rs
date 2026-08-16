//! The windowed commit model — the single decision that sets forqen's memory
//! ceiling.
//!
//! A naive client collects `Vec<CommitRow>` for the whole history. On
//! `torvalds/linux` that is ~1.3M rows, each holding three heap `String`s, and
//! RSS lands near a gigabyte before a pixel is drawn. GTK's `GtkColumnView`
//! only ever asks for the rows it is about to display, so the model can hold a
//! sparse map of realized rows and drop the rest.
//!
//! Two structures, deliberately different in cost:
//!
//! * **the spine** — `Vec<ObjectId>`, 20 inline bytes per commit, grows as the
//!   walk proceeds and is never evicted. ~26MB at 1.3M commits. This is what
//!   makes `row(i)` O(1) and lets the scrollbar jump anywhere.
//! * **realized rows** — `HashMap<usize, CommitRow>`, capped at [`Self::budget`]
//!   entries. These hold the strings, so these are what eviction targets.

use std::collections::HashMap;
use std::ops::Range;

use crate::{CommitRow, GitError, ObjectId, Repo};

use super::{hydrate, Walker};

/// Realized rows kept alive. A 4K display shows well under 100 rows; the rest
/// is overscan so a flick-scroll does not re-hydrate everything it passes.
const DEFAULT_BUDGET: usize = 512;

/// Rows hydrated either side of the requested range, so scrolling by one row
/// does not trigger a fetch every time.
const OVERSCAN: usize = 50;

pub struct HistoryWindow {
    /// Commit ids in walk order. Append-only; index is the row number.
    spine: Vec<ObjectId>,
    /// Rows currently materialized, keyed by row index.
    ///
    /// This is the only structure eviction touches. There is deliberately no
    /// parallel insertion-order list: the policy below ranks by distance from
    /// the viewport, not by age, so a second container would be state that can
    /// only ever fall out of sync with this one.
    rows: HashMap<usize, CommitRow>,
    /// Maximum entries in `rows`.
    budget: usize,
    /// The most recently requested viewport, so eviction can avoid discarding
    /// rows that are on screen right now.
    viewport: Range<usize>,
    /// True once the walk has reached the root commit.
    exhausted: bool,
}

impl HistoryWindow {
    pub fn new() -> Self {
        Self::with_budget(DEFAULT_BUDGET)
    }

    pub fn with_budget(budget: usize) -> Self {
        assert!(budget > 0, "a zero budget can hold no visible row");
        Self {
            spine: Vec::new(),
            rows: HashMap::new(),
            budget,
            viewport: 0..0,
            exhausted: false,
        }
    }

    /// Rows known so far. Grows as the walk advances, so the UI shows a
    /// scrollbar that settles rather than blocking on a full walk at open.
    pub fn len(&self) -> usize {
        self.spine.len()
    }

    pub fn is_empty(&self) -> bool {
        self.spine.is_empty()
    }

    pub fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    /// Number of rows currently holding heap strings. The quantity the memory
    /// budget actually constrains.
    pub fn realized(&self) -> usize {
        self.rows.len()
    }

    pub fn viewport(&self) -> &Range<usize> {
        &self.viewport
    }

    pub fn budget(&self) -> usize {
        self.budget
    }

    /// Consume `walker` entirely, filling the spine.
    ///
    /// Takes the walker by value and refuses to run on a non-empty spine, both
    /// deliberately. The earlier signature accepted `&mut Walker` plus a row
    /// target so the spine could be built across several calls — but a `Walker`
    /// borrows its repository and cannot outlive one call, so callers built a
    /// *fresh* walker each time. A fresh walker restarts at HEAD, so every call
    /// re-appended ids from the beginning: the spine filled with duplicates and
    /// grew without bound, and RSS passed 600MB on a 50k-commit repository.
    ///
    /// That was a caller mistake the old signature invited. This one cannot be
    /// called twice, so it cannot be made.
    pub fn fill_spine(&mut self, walker: Walker<'_>) -> Result<(), GitError> {
        assert!(
            self.spine.is_empty(),
            "fill_spine on a populated spine would duplicate ids; \
             build a fresh HistoryWindow instead"
        );
        for id in walker {
            self.spine.push(id?);
        }
        self.exhausted = true;
        Ok(())
    }

    /// Make `range` (plus overscan) available, hydrating what is missing and
    /// evicting down to budget afterwards.
    pub fn ensure(&mut self, repo: &Repo, range: Range<usize>) -> Result<(), GitError> {
        self.viewport = range.start..range.end.min(self.spine.len());

        let lo = range.start.saturating_sub(OVERSCAN);
        let hi = (range.end + OVERSCAN).min(self.spine.len());

        for i in lo..hi {
            if self.rows.contains_key(&i) {
                continue;
            }
            let row = hydrate(repo, self.spine[i])?;
            self.rows.insert(i, row);
        }

        self.evict();
        Ok(())
    }

    /// A realized row, or `None` if it has not been hydrated yet.
    ///
    /// The UI treats `None` as "draw a placeholder and request it" rather than
    /// blocking the main loop on a hydrate.
    pub fn row(&self, index: usize) -> Option<&CommitRow> {
        self.rows.get(&index)
    }

    pub fn id(&self, index: usize) -> Option<ObjectId> {
        self.spine.get(index).copied()
    }

    /// True when `index` is inside the viewport last passed to [`Self::ensure`].
    pub fn is_visible(&self, index: usize) -> bool {
        self.viewport.contains(&index)
    }

    /// Drop realized rows until `self.rows.len() <= self.budget`.
    ///
    /// This function *is* the memory ceiling, and it runs after every `ensure`,
    /// so it sits on the scroll path.
    ///
    /// **Policy: anchor plus bidirectional band.** Rows are ranked by distance
    /// from the current viewport and the furthest go first, so what survives is
    /// a band centred on what the user is looking at.
    ///
    /// The obvious alternative — pure FIFO on realization order — is shorter but
    /// thrashes on direction change: the rows just scrolled past are the oldest,
    /// so they are evicted first, and scrolling back up re-hydrates every one of
    /// them. Ranking by distance instead of age costs nothing extra in memory
    /// and makes reversing direction free.
    ///
    /// The invariant that matters most: a row inside `self.viewport` is never
    /// evicted. Dropping an on-screen row makes GTK request it again
    /// immediately, and the resulting re-hydrate loop presents as a pinned CPU
    /// core rather than as a crash, which is considerably harder to diagnose.
    ///
    /// Cost is `O(n log n)` on the realized set, which is bounded by
    /// `budget + 2 * OVERSCAN` — a few hundred entries, so a sort of tens of
    /// microseconds. If that ever shows up in a scroll profile, a
    /// `select_nth_unstable` partition gets it to `O(n)` without changing the
    /// policy.
    fn evict(&mut self) {
        if self.rows.len() <= self.budget {
            return;
        }

        let mut candidates: Vec<usize> = self
            .rows
            .keys()
            .copied()
            .filter(|i| !self.viewport.contains(i))
            .collect();

        // Furthest from the viewport first.
        candidates.sort_unstable_by_key(|&i| std::cmp::Reverse(self.distance_from_viewport(i)));

        // If the viewport alone exceeds the budget, this evicts every
        // non-visible row and stops. Going further would mean discarding rows
        // GTK is actively drawing, so the budget yields to correctness — a
        // viewport that large is a misconfigured budget, not a runtime state to
        // handle silently.
        let excess = self.rows.len() - self.budget;
        for i in candidates.into_iter().take(excess) {
            self.rows.remove(&i);
        }
    }

    /// Rows between `index` and the nearest edge of the viewport. Zero inside.
    fn distance_from_viewport(&self, index: usize) -> usize {
        if index < self.viewport.start {
            self.viewport.start - index
        } else if index >= self.viewport.end {
            index - self.viewport.end + 1
        } else {
            0
        }
    }
}

impl Default for HistoryWindow {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::tests::fixture;

    fn loaded(commits: usize, budget: usize) -> (tempfile::TempDir, Repo, HistoryWindow) {
        let dir = fixture(commits);
        let repo = Repo::open(dir.path()).unwrap();
        let mut win = HistoryWindow::with_budget(budget);
        win.fill_spine(Walker::from_head(&repo).unwrap()).unwrap();
        (dir, repo, win)
    }

    // --- spine behaviour ----------------------------------------------------

    #[test]
    fn fill_spine_walks_the_whole_history_exactly_once() {
        let (_d, _r, win) = loaded(20, 512);
        assert_eq!(win.len(), 20);
        assert!(win.is_exhausted());
    }

    /// Regression: the spine must never contain a commit twice.
    ///
    /// The previous API let a caller extend the spine from a freshly built
    /// walker, which restarts at HEAD — every call re-appended the same ids.
    /// It presented as unbounded RSS growth rather than as visibly duplicated
    /// rows, so this checks the data directly.
    #[test]
    fn spine_contains_no_duplicate_ids() {
        let (_d, _r, win) = loaded(50, 512);

        let mut seen = std::collections::HashSet::new();
        for i in 0..win.len() {
            let id = win.id(i).expect("id within len");
            assert!(seen.insert(id), "commit {id:?} appears twice at row {i}");
        }
        assert_eq!(seen.len(), 50);
    }

    #[test]
    #[should_panic(expected = "would duplicate ids")]
    fn filling_a_populated_spine_is_refused() {
        let dir = fixture(5);
        let repo = Repo::open(dir.path()).unwrap();
        let mut win = HistoryWindow::new();

        win.fill_spine(Walker::from_head(&repo).unwrap()).unwrap();
        // The mistake the old signature invited: a second fill from a fresh
        // walker. It must abort rather than silently double the history.
        win.fill_spine(Walker::from_head(&repo).unwrap()).unwrap();
    }

    #[test]
    fn spine_gives_random_access_without_hydrating() {
        let (_d, _r, win) = loaded(10, 4);
        assert!(
            win.id(9).is_some(),
            "scrollbar needs O(1) access to any row"
        );
        assert_eq!(win.realized(), 0, "the spine must not hydrate rows");
        assert!(win.id(10).is_none());
    }

    // --- the contract for evict() -------------------------------------------
    // These pin the invariants, not the numbers: a different eviction policy
    // must still satisfy every one of them.

    #[test]
    fn ensure_materializes_the_requested_range() {
        let (_d, repo, mut win) = loaded(30, 512);
        win.ensure(&repo, 0..10).unwrap();
        for i in 0..10 {
            assert!(win.row(i).is_some(), "row {i} should be realized");
        }
        assert_eq!(win.row(0).unwrap().summary, "commit 29");
    }

    #[test]
    fn realized_rows_never_exceed_the_budget() {
        let (_d, repo, mut win) = loaded(400, 64);
        // Scroll the whole history in viewport-sized steps.
        for start in (0..350).step_by(10) {
            win.ensure(&repo, start..start + 10).unwrap();
            assert!(
                win.realized() <= win.budget(),
                "budget {} exceeded at {start}: {} realized",
                win.budget(),
                win.realized()
            );
        }
    }

    #[test]
    fn eviction_never_drops_an_on_screen_row() {
        // Budget deliberately tight relative to viewport + overscan, so the
        // policy is forced to choose.
        let (_d, repo, mut win) = loaded(400, 60);
        for start in (0..350).step_by(25) {
            let range = start..start + 20;
            win.ensure(&repo, range.clone()).unwrap();
            for i in range {
                assert!(
                    win.row(i).is_some(),
                    "row {i} is in the viewport and must survive eviction — \
                     dropping it makes GTK re-request it forever"
                );
            }
        }
    }

    #[test]
    fn live_tracking_stays_consistent_so_the_budget_keeps_being_enforced() {
        let (_d, repo, mut win) = loaded(400, 64);
        for start in (0..300).step_by(7) {
            win.ensure(&repo, start..start + 15).unwrap();
        }
        // Re-request an early range after long scrolling: if `live` accumulated
        // stale indices, eviction has quietly become a no-op by now.
        win.ensure(&repo, 0..15).unwrap();
        assert!(win.realized() <= win.budget());
    }

    #[test]
    fn scrolling_back_does_not_lose_correctness() {
        let (_d, repo, mut win) = loaded(200, 64);
        win.ensure(&repo, 0..20).unwrap();
        win.ensure(&repo, 150..170).unwrap();
        win.ensure(&repo, 0..20).unwrap();

        // Whatever the policy, the data must be right after a round trip.
        assert_eq!(win.row(0).unwrap().summary, "commit 199");
        assert_eq!(win.row(19).unwrap().summary, "commit 180");
    }
}
