//! Git engine. Never imports `gtk` — every behaviour here is testable headless.
//!
//! Two execution paths sit behind one API:
//!
//! * **gix, in-process** — reads: revwalk, objects, refs, status, diff, blame.
//! * **the `git` binary** — rebase, signed commits, hook-running commits,
//!   push/fetch negotiation, LFS, filters.
//!
//! The split is not a stopgap. `gix-rebase` is published at `0.0.0` (an empty
//! placeholder), and re-implementing hook execution, gitattributes filters and
//! commit signing is how a client silently corrupts someone's repository.
//! Shelling out is both more correct and cheaper in memory, since the child
//! process's heap dies with the child.

pub mod branch;
pub mod commit;
pub mod diff;
pub mod history;
pub mod merge;
pub mod rebase;
pub mod refs;
pub mod remote;
pub mod repo;
pub mod stage;
pub mod stash;
pub mod status;

pub use repo::Repo;

use std::time::SystemTime;

/// One row of the commit list. Deliberately flat and owned: this crosses a
/// thread boundary from the rayon pool to the GTK main loop, so it must not
/// borrow from the repository or hold a `gix` handle.
///
/// Field choice is a memory decision. At ~1.3M commits, every extra `String`
/// here is another allocation per realized row — which is survivable only
/// because [`history::window`] keeps the realized set tiny.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitRow {
    pub id: ObjectId,
    /// First line of the message. The body is fetched on selection, not here —
    /// full messages would dominate the row's footprint.
    pub summary: String,
    pub author_name: String,
    pub author_email: String,
    pub time: SystemTime,
    pub parents: Vec<ObjectId>,
}

/// A 20-byte SHA-1, inline. Not a `String`: the hex form is 40 bytes plus a
/// heap allocation plus a pointer, and we hold a lot of these.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId(pub [u8; 20]);

impl ObjectId {
    pub fn to_hex(self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The abbreviated form shown in the UI.
    pub fn short(self) -> String {
        self.to_hex()[..7].to_owned()
    }
}

impl std::fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl From<gix::ObjectId> for ObjectId {
    fn from(id: gix::ObjectId) -> Self {
        let mut out = [0u8; 20];
        // gix supports SHA-256 repos in principle; forqen is SHA-1 only for now
        // and truncating is the honest behaviour until SHA-256 is a real target.
        let bytes = id.as_bytes();
        let n = bytes.len().min(20);
        out[..n].copy_from_slice(&bytes[..n]);
        Self(out)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("{path} is not a git repository")]
    NotARepo { path: String },

    #[error("failed to open repository: {0}")]
    Open(String),

    #[error("failed to walk history: {0}")]
    Walk(String),

    #[error("object {0} is missing or unreadable")]
    Object(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_id_renders_hex_and_short_form() {
        let mut raw = [0u8; 20];
        raw[0] = 0xde;
        raw[1] = 0xad;
        raw[2] = 0xbe;
        raw[3] = 0xef;
        let id = ObjectId(raw);
        assert_eq!(id.to_hex().len(), 40);
        assert!(id.to_hex().starts_with("deadbeef"));
        assert_eq!(id.short(), "deadbee");
    }
}
