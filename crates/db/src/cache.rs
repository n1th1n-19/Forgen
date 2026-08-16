//! HTTP revalidation cache.
//!
//! The point is not to avoid the round trip — it is to avoid the *rate limit*.
//! GitHub excludes `304 Not Modified` from the primary rate limit, so a request
//! carrying `If-None-Match` costs latency but no budget. Polling notifications
//! every 60 seconds is affordable only because of this.
//!
//! Storing the body alongside the validator is what makes offline mode work:
//! with no network the app renders the last good payload behind a staleness
//! banner rather than an error page.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, OptionalExtension};

use crate::{Db, DbError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedResponse {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub body: Vec<u8>,
    pub fetched_at: SystemTime,
}

impl CachedResponse {
    pub fn age(&self, now: SystemTime) -> Duration {
        now.duration_since(self.fetched_at).unwrap_or_default()
    }
}

impl Db {
    /// Look up a cached response so its validators can be attached to the next
    /// request.
    pub fn cached(&self, url: &str) -> Result<Option<CachedResponse>, DbError> {
        let row = self
            .lock()
            .query_row(
                "SELECT etag, last_modified, body, fetched_at
                   FROM http_cache WHERE url = ?1",
                params![url],
                |r| {
                    Ok(CachedResponse {
                        etag: r.get(0)?,
                        last_modified: r.get(1)?,
                        body: r.get(2)?,
                        fetched_at: UNIX_EPOCH
                            + Duration::from_secs(r.get::<_, i64>(3)?.max(0) as u64),
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Store a `200 OK` response.
    pub fn store(
        &self,
        url: &str,
        etag: Option<&str>,
        last_modified: Option<&str>,
        body: &[u8],
    ) -> Result<(), DbError> {
        self.lock().execute(
            "INSERT INTO http_cache (url, etag, last_modified, body, fetched_at)
                  VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (url) DO UPDATE SET
                  etag          = excluded.etag,
                  last_modified = excluded.last_modified,
                  body          = excluded.body,
                  fetched_at    = excluded.fetched_at",
            params![url, etag, last_modified, body, now_secs()],
        )?;
        Ok(())
    }

    /// Record that a `304` confirmed the stored copy.
    ///
    /// Only `fetched_at` moves. Rewriting the body on a 304 would be pointless
    /// work, and the freshness timestamp is what the staleness banner reads.
    pub fn touch(&self, url: &str) -> Result<(), DbError> {
        self.lock().execute(
            "UPDATE http_cache SET fetched_at = ?2 WHERE url = ?1",
            params![url, now_secs()],
        )?;
        Ok(())
    }

    /// Drop entries older than `max_age`, returning how many went.
    ///
    /// Called at startup rather than on a timer: an unbounded cache is a disk
    /// leak, but evicting mid-session would throw away exactly the pages the
    /// user is moving between.
    pub fn prune(&self, max_age: Duration) -> Result<usize, DbError> {
        let cutoff = now_secs() - max_age.as_secs() as i64;
        let n = self.lock().execute(
            "DELETE FROM http_cache WHERE fetched_at < ?1",
            params![cutoff],
        )?;
        Ok(n)
    }

    pub fn invalidate(&self, url: &str) -> Result<(), DbError> {
        self.lock()
            .execute("DELETE FROM http_cache WHERE url = ?1", params![url])?;
        Ok(())
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    const URL: &str = "https://api.github.com/user/repos";

    #[test]
    fn stores_and_returns_validators_with_the_body() {
        let db = db();
        db.store(URL, Some("W/\"abc\""), None, b"[{\"name\":\"forqen\"}]")
            .unwrap();

        let got = db.cached(URL).unwrap().unwrap();
        assert_eq!(got.etag.as_deref(), Some("W/\"abc\""));
        assert_eq!(got.body, b"[{\"name\":\"forqen\"}]");
        assert!(got.age(SystemTime::now()) < Duration::from_secs(5));
    }

    #[test]
    fn an_uncached_url_is_none() {
        assert!(db()
            .cached("https://api.github.com/nope")
            .unwrap()
            .is_none());
    }

    #[test]
    fn storing_again_replaces_rather_than_duplicating() {
        let db = db();
        db.store(URL, Some("v1"), None, b"old").unwrap();
        db.store(URL, Some("v2"), None, b"new").unwrap();

        let got = db.cached(URL).unwrap().unwrap();
        assert_eq!(got.etag.as_deref(), Some("v2"));
        assert_eq!(got.body, b"new");

        let n: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM http_cache", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "url is the primary key");
    }

    #[test]
    fn touch_refreshes_the_timestamp_without_touching_the_body() {
        let db = db();
        db.store(URL, Some("v1"), None, b"payload").unwrap();

        // Backdate so the touch is observable.
        db.conn()
            .execute("UPDATE http_cache SET fetched_at = 0", [])
            .unwrap();
        assert!(
            db.cached(URL).unwrap().unwrap().age(SystemTime::now()) > Duration::from_secs(1000)
        );

        db.touch(URL).unwrap();
        let got = db.cached(URL).unwrap().unwrap();
        assert!(got.age(SystemTime::now()) < Duration::from_secs(5));
        assert_eq!(got.body, b"payload", "a 304 must not disturb the body");
        assert_eq!(got.etag.as_deref(), Some("v1"));
    }

    #[test]
    fn prune_drops_only_entries_past_the_cutoff() {
        let db = db();
        db.store("https://a", Some("1"), None, b"a").unwrap();
        db.store("https://b", Some("1"), None, b"b").unwrap();
        db.conn()
            .execute(
                "UPDATE http_cache SET fetched_at = 0 WHERE url = 'https://a'",
                [],
            )
            .unwrap();

        let removed = db.prune(Duration::from_secs(3600)).unwrap();
        assert_eq!(removed, 1);
        assert!(db.cached("https://a").unwrap().is_none());
        assert!(db.cached("https://b").unwrap().is_some());
    }

    #[test]
    fn invalidate_removes_the_entry() {
        let db = db();
        db.store(URL, None, None, b"x").unwrap();
        db.invalidate(URL).unwrap();
        assert!(db.cached(URL).unwrap().is_none());
    }
}
