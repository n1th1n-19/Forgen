//! Local SQLite store: the account roster and the HTTP revalidation cache.
//!
//! Two jobs, both about not going to the network:
//!
//! * **accounts** — which identities exist, on which hosts. Tokens live in the
//!   keyring, never here; this table holds only the pointer.
//! * **http_cache** — ETag plus body per URL. GitHub does not count a `304 Not
//!   Modified` against the rate limit, so revalidating is nearly free. This is
//!   what makes a polled notifications inbox affordable, and what lets the app
//!   render real data with a staleness banner when offline instead of an error.

pub mod accounts;
pub mod cache;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use rusqlite::Connection;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("database error: {0}")]
    Sql(#[from] rusqlite::Error),

    #[error("could not locate a data directory for forqen")]
    NoDataDir,

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// The store.
///
/// The connection is behind a `Mutex` because `rusqlite::Connection` is `Send`
/// but not `Sync`, and this is shared as `Arc<Db>` across the tokio runtime —
/// an API fetch on a worker thread and a cache read on the main loop touch the
/// same handle. Without it, `Arc<Db>` makes every future holding one non-`Send`,
/// which surfaces as an inscrutable error at the `tokio::spawn` call site
/// rather than here where the cause is.
///
/// A single lock rather than a pool: the workload is a handful of small reads
/// and writes per user action, and SQLite serializes writes internally anyway.
///
/// ponytail: one global lock. If a background sync ever contends with UI reads,
/// move to `r2d2_sqlite` — the `lock()` chokepoint below is the only thing that
/// would need to change.
pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    /// Open (creating if needed) the store at the XDG data location.
    pub fn open_default() -> Result<Self, DbError> {
        let path = default_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Self::open(&path)
    }

    pub fn open(path: &Path) -> Result<Self, DbError> {
        let conn = Connection::open(path)?;
        Self::from_conn(conn)
    }

    pub fn open_in_memory() -> Result<Self, DbError> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> Result<Self, DbError> {
        // WAL so a long-running read (rendering a cached PR list) never blocks
        // a background writer landing a fetch. NORMAL synchronous is right for
        // a cache: the worst case of a hard power loss is a refetch.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        let db = Self {
            conn: Mutex::new(conn),
        };
        db.migrate()?;
        Ok(db)
    }

    /// The one place the connection is unlocked.
    ///
    /// Panics on a poisoned lock. That only happens if another thread panicked
    /// mid-transaction, at which point the cache's consistency is unknown and
    /// continuing would be worse than stopping.
    pub(crate) fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().expect("database mutex poisoned")
    }

    /// Schema migrations, applied in order and tracked by `user_version`.
    ///
    /// A plain integer rather than a migrations table: there is exactly one
    /// writer process and the whole database is discardable cache plus a small
    /// roster, so the ceremony of a migrations framework buys nothing here.
    fn migrate(&self) -> Result<(), DbError> {
        let version: i64 = self
            .lock()
            .query_row("PRAGMA user_version", [], |r| r.get(0))?;

        const MIGRATIONS: &[&str] = &[
            // v1 — accounts and the HTTP cache.
            r#"
            CREATE TABLE accounts (
                host       TEXT NOT NULL,
                login      TEXT NOT NULL,
                is_default INTEGER NOT NULL DEFAULT 0,
                added_at   INTEGER NOT NULL,
                PRIMARY KEY (host, login)
            );

            -- At most one default per host, enforced by the schema rather than
            -- by remembering to clear the old one at every call site.
            CREATE UNIQUE INDEX accounts_one_default_per_host
                ON accounts (host) WHERE is_default = 1;

            CREATE TABLE http_cache (
                url           TEXT PRIMARY KEY,
                etag          TEXT,
                last_modified TEXT,
                body          BLOB NOT NULL,
                fetched_at    INTEGER NOT NULL
            );

            CREATE INDEX http_cache_by_age ON http_cache (fetched_at);
            "#,
        ];

        for (i, sql) in MIGRATIONS.iter().enumerate() {
            let target = i as i64 + 1;
            if version < target {
                self.lock().execute_batch(sql)?;
                self.lock().pragma_update(None, "user_version", target)?;
                tracing::info!(version = target, "applied schema migration");
            }
        }
        Ok(())
    }

    /// Borrow the connection directly. Holds the lock for as long as the guard
    /// lives, so callers should keep the expression short.
    pub fn conn(&self) -> MutexGuard<'_, Connection> {
        self.lock()
    }
}

fn default_path() -> Result<PathBuf, DbError> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .ok_or(DbError::NoDataDir)?;
    Ok(base.join("forqen/forqen.db"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_is_idempotent_and_sets_user_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");

        let db = Db::open(&path).unwrap();
        let v: i64 = db
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 1);
        drop(db);

        // Reopening must not re-run migrations or fail on existing tables.
        let db = Db::open(&path).unwrap();
        let v: i64 = db
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 1);
    }

    #[test]
    fn default_path_follows_xdg() {
        // Reading process env, so keep it to assertions about shape.
        let p = default_path().unwrap();
        assert!(p.ends_with("forqen/forqen.db"), "{}", p.display());
    }
}
