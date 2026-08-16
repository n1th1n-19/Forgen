//! Commit history: a lazy walker plus the windowed model the UI binds to.

pub mod window;

pub use window::HistoryWindow;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::{CommitRow, GitError, ObjectId, Repo};

/// A revwalk that yields ids only, in topological/date order.
///
/// Ids and not [`CommitRow`]s on purpose: an id is 20 inline bytes, while a row
/// carries three heap-allocated `String`s. Walking a 1.3M-commit repository to
/// build the spine therefore costs ~26MB, whereas hydrating every row would
/// cost gigabytes. Hydration happens per visible row, in [`window`].
pub struct Walker<'r> {
    inner: Box<dyn Iterator<Item = Result<gix::ObjectId, GitError>> + 'r>,
}

impl<'r> Walker<'r> {
    /// Walk from HEAD.
    pub fn from_head(repo: &'r Repo) -> Result<Self, GitError> {
        let head = repo
            .inner()
            .head_id()
            .map_err(|e| GitError::Walk(e.to_string()))?;
        Self::from_tips(repo, std::iter::once(head.detach()))
    }

    pub fn from_tips(
        repo: &'r Repo,
        tips: impl IntoIterator<Item = gix::ObjectId>,
    ) -> Result<Self, GitError> {
        let walk = repo
            .inner()
            .rev_walk(tips)
            .all()
            .map_err(|e| GitError::Walk(e.to_string()))?;

        let iter = walk.map(|res| {
            res.map(|info| info.id)
                .map_err(|e| GitError::Walk(e.to_string()))
        });

        Ok(Self {
            inner: Box::new(iter),
        })
    }
}

impl Iterator for Walker<'_> {
    type Item = Result<ObjectId, GitError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|r| r.map(ObjectId::from))
    }
}

/// Load the display fields for one commit.
///
/// Only the message *summary* is kept. Full bodies are read on selection: at a
/// few hundred bytes each they would dominate the realized-row footprint and
/// nothing in the list view can show them.
pub fn hydrate(repo: &Repo, id: ObjectId) -> Result<CommitRow, GitError> {
    let oid = gix::ObjectId::from_bytes_or_panic(&id.0);
    let commit = repo
        .inner()
        .find_object(oid)
        .map_err(|e| GitError::Object(e.to_string()))?
        .try_into_commit()
        .map_err(|e| GitError::Object(e.to_string()))?;

    let message = commit
        .message()
        .map_err(|e| GitError::Object(e.to_string()))?;
    let summary = message.summary().to_string();

    let author = commit
        .author()
        .map_err(|e| GitError::Object(e.to_string()))?;

    // Git stores seconds since the epoch; negative values are possible in
    // repositories with a rewritten or hostile history, so clamp rather than
    // letting an `unwrap` take the process down.
    let secs = author.time().map(|t| t.seconds).unwrap_or(0);
    let time = if secs >= 0 {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::from_secs(secs.unsigned_abs()))
            .unwrap_or(UNIX_EPOCH)
    };

    Ok(CommitRow {
        id,
        summary,
        author_name: author.name.to_string(),
        author_email: author.email.to_string(),
        time,
        parents: commit
            .parent_ids()
            .map(|p| ObjectId::from(p.detach()))
            .collect(),
    })
}

/// Ensure `.git/objects/info/commit-graph` exists.
///
/// The commit-graph turns parent lookup and generation-number comparison into
/// O(1) reads instead of decompressing commit objects, which is what makes
/// seeking to an arbitrary scrollbar position affordable. Written by shelling
/// out because generating it is a write path, and git's own writer is the
/// reference implementation.
pub fn ensure_commit_graph(repo: &Repo) -> Result<(), GitError> {
    let status = std::process::Command::new("git")
        .args(["commit-graph", "write", "--reachable"])
        .current_dir(repo.git_dir())
        .output()?;

    if !status.status.success() {
        // Non-fatal: history still works, just with slower seeks.
        tracing::warn!(
            stderr = %String::from_utf8_lossy(&status.stderr),
            "commit-graph write failed; history seeks will be slower"
        );
    }
    Ok(())
}

/// Timestamp helper shared by the UI. Kept here so `SystemTime` handling has
/// exactly one implementation.
pub fn unix_seconds(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::tests::fixture;

    #[test]
    fn walks_every_commit_newest_first() {
        let dir = fixture(5);
        let repo = Repo::open(dir.path()).unwrap();
        let ids: Vec<_> = Walker::from_head(&repo)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(ids.len(), 5);

        let newest = hydrate(&repo, ids[0]).unwrap();
        assert_eq!(newest.summary, "commit 4", "walk must start at HEAD");

        let oldest = hydrate(&repo, ids[4]).unwrap();
        assert_eq!(oldest.summary, "commit 0");
        assert!(oldest.parents.is_empty(), "root commit has no parents");
    }

    #[test]
    fn hydrate_reads_author_and_parent_links() {
        let dir = fixture(2);
        let repo = Repo::open(dir.path()).unwrap();
        let ids: Vec<_> = Walker::from_head(&repo)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();

        let row = hydrate(&repo, ids[0]).unwrap();
        assert_eq!(row.author_name, "Fixture");
        assert_eq!(row.author_email, "fixture@example.invalid");
        assert_eq!(row.parents, vec![ids[1]]);
        assert!(row.time > UNIX_EPOCH);
    }

    #[test]
    fn commit_graph_write_is_idempotent_and_non_fatal() {
        let dir = fixture(3);
        let repo = Repo::open(dir.path()).unwrap();
        ensure_commit_graph(&repo).unwrap();
        ensure_commit_graph(&repo).unwrap();

        // History must still read correctly with the graph in place.
        let n = Walker::from_head(&repo).unwrap().count();
        assert_eq!(n, 3);
    }
}
