//! Token persistence via the freedesktop Secret Service.
//!
//! There is no file-backed fallback on purpose. A token in `~/.config` is
//! readable by every process running as the user, including anything that got
//! in through a compromised dependency. If no keyring is present we say so and
//! stop, rather than quietly downgrading the user's security.

use keyring::Entry;

use crate::{Account, AuthError, Token};

const SERVICE: &str = "forqen";

fn entry(account: &Account) -> Result<Entry, AuthError> {
    Entry::new(SERVICE, &account.keyring_key()).map_err(classify)
}

/// Turn "the session bus has no Secret Service" into an actionable error,
/// and leave everything else as-is.
fn classify(e: keyring::Error) -> AuthError {
    match e {
        keyring::Error::PlatformFailure(_) | keyring::Error::NoStorageAccess(_) => {
            AuthError::NoKeyring
        }
        other => AuthError::Keyring(other),
    }
}

pub fn save(account: &Account, token: &Token) -> Result<(), AuthError> {
    // `Secret`'s Serialize is the real value — this is the one place that is
    // intended, and it goes to the keyring, never to a log.
    let blob = serde_json::to_string(token).map_err(|e| AuthError::Malformed {
        host: account.host.clone(),
        detail: e.to_string(),
    })?;
    entry(account)?.set_password(&blob).map_err(classify)
}

pub fn load(account: &Account) -> Result<Token, AuthError> {
    let blob = match entry(account)?.get_password() {
        Ok(b) => b,
        Err(keyring::Error::NoEntry) => return Err(AuthError::NotFound(account.keyring_key())),
        Err(e) => return Err(classify(e)),
    };
    serde_json::from_str(&blob).map_err(|e| AuthError::Malformed {
        host: account.host.clone(),
        detail: e.to_string(),
    })
}

/// Remove stored credentials. Missing entries are not an error — logging out
/// twice should succeed both times.
pub fn delete(account: &Account) -> Result<(), AuthError> {
    match entry(account)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(classify(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Secret;

    fn acct() -> Account {
        Account {
            host: "github.com".into(),
            login: "forqen-test-account".into(),
            is_default_for_host: true,
        }
    }

    /// Round-trips through the real Secret Service, so it is ignored by default
    /// — CI has no session bus. Run locally with:
    ///   cargo test -p auth -- --ignored
    #[test]
    #[ignore = "requires a running Secret Service"]
    fn save_load_delete_round_trip() {
        let a = acct();
        let t = Token {
            access: Secret::new("ghu_roundtrip"),
            refresh: Some(Secret::new("ghr_roundtrip")),
            expires_at: None,
            scopes: vec!["repo".into()],
        };

        save(&a, &t).unwrap();
        let got = load(&a).unwrap();
        assert_eq!(got.access.expose(), "ghu_roundtrip");
        assert_eq!(got.scopes, ["repo"]);

        delete(&a).unwrap();
        assert!(matches!(load(&a), Err(AuthError::NotFound(_))));
        // Deleting again must not error.
        delete(&a).unwrap();
    }
}
