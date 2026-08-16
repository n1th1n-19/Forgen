//! The account roster.
//!
//! Tokens are **not** here — they live in the Secret Service. This table only
//! records which identities exist so the app can show an account switcher
//! without unlocking the keyring first.

use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::params;

use crate::{Db, DbError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountRow {
    pub host: String,
    pub login: String,
    pub is_default: bool,
}

impl Db {
    /// Insert or update an account. Setting `is_default` clears the previous
    /// default for that host in the same transaction, so the partial unique
    /// index can never be violated by a half-applied change.
    pub fn upsert_account(&self, host: &str, login: &str, is_default: bool) -> Result<(), DbError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        // The guard is bound rather than used inline: the transaction borrows
        // the connection, so a temporary guard would be dropped before the
        // transaction it owns is committed.
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        if is_default {
            tx.execute(
                "UPDATE accounts SET is_default = 0 WHERE host = ?1",
                params![host],
            )?;
        }
        tx.execute(
            "INSERT INTO accounts (host, login, is_default, added_at)
                  VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (host, login)
                  DO UPDATE SET is_default = excluded.is_default",
            params![host, login, is_default as i64, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn accounts(&self) -> Result<Vec<AccountRow>, DbError> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT host, login, is_default FROM accounts ORDER BY host, login")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(AccountRow {
                    host: r.get(0)?,
                    login: r.get(1)?,
                    is_default: r.get::<_, i64>(2)? != 0,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn default_account(&self, host: &str) -> Result<Option<AccountRow>, DbError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT host, login, is_default FROM accounts
              WHERE host = ?1 AND is_default = 1",
        )?;
        let mut rows = stmt.query_map(params![host], |r| {
            Ok(AccountRow {
                host: r.get(0)?,
                login: r.get(1)?,
                is_default: r.get::<_, i64>(2)? != 0,
            })
        })?;
        rows.next().transpose().map_err(Into::into)
    }

    /// Forget an account. The caller is responsible for deleting the matching
    /// keyring entry — dropping the row without the token would strand a
    /// credential in the keyring with nothing referencing it.
    pub fn remove_account(&self, host: &str, login: &str) -> Result<(), DbError> {
        self.lock().execute(
            "DELETE FROM accounts WHERE host = ?1 AND login = ?2",
            params![host, login],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    #[test]
    fn upsert_lists_and_finds_the_default() {
        let db = db();
        db.upsert_account("github.com", "n1th1n-19", true).unwrap();
        db.upsert_account("github.com", "alt-account", false)
            .unwrap();

        let all = db.accounts().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(
            db.default_account("github.com").unwrap().unwrap().login,
            "n1th1n-19"
        );
    }

    #[test]
    fn promoting_a_new_default_demotes_the_old_one() {
        let db = db();
        db.upsert_account("github.com", "first", true).unwrap();
        db.upsert_account("github.com", "second", true).unwrap();

        assert_eq!(
            db.default_account("github.com").unwrap().unwrap().login,
            "second"
        );
        // The partial unique index would have rejected two defaults outright,
        // so reaching here at all proves the demote-then-insert order holds.
        let defaults = db
            .accounts()
            .unwrap()
            .into_iter()
            .filter(|a| a.is_default)
            .count();
        assert_eq!(defaults, 1);
    }

    #[test]
    fn hosts_keep_independent_defaults() {
        let db = db();
        db.upsert_account("github.com", "me", true).unwrap();
        db.upsert_account("git.corp.example", "me", true).unwrap();

        assert!(db.default_account("github.com").unwrap().is_some());
        assert!(db.default_account("git.corp.example").unwrap().is_some());
        assert_eq!(db.accounts().unwrap().len(), 2, "same login, two hosts");
    }

    #[test]
    fn re_upserting_does_not_duplicate() {
        let db = db();
        db.upsert_account("github.com", "me", true).unwrap();
        db.upsert_account("github.com", "me", true).unwrap();
        assert_eq!(db.accounts().unwrap().len(), 1);
    }

    #[test]
    fn removing_an_account_leaves_the_others() {
        let db = db();
        db.upsert_account("github.com", "a", true).unwrap();
        db.upsert_account("github.com", "b", false).unwrap();

        db.remove_account("github.com", "b").unwrap();
        let all = db.accounts().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].login, "a");
    }

    #[test]
    fn no_default_for_an_unknown_host_is_none_not_an_error() {
        let db = db();
        assert!(db.default_account("nowhere.invalid").unwrap().is_none());
    }
}
