//! Authentication for forqen: OAuth device flow, keyring storage, multi-account.
//!
//! No UI types here. Everything in this crate is testable headless.

pub mod device_flow;
pub mod gh_import;
pub mod store;

use std::fmt;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// Public client id of the forqen GitHub App.
///
/// Deliberately not a secret. The device flow exists precisely so desktop apps
/// need not ship a `client_secret` — one embedded in a distributed binary is
/// readable with `strings` and is therefore not a secret at all.
///
/// Set at build time so a fork or an Enterprise deployment can point at its own
/// App without patching source:
///
/// ```sh
/// FORQEN_CLIENT_ID=Iv23li... cargo build --release
/// ```
///
/// The fallback is a placeholder, not a working id. Browser sign-in fails
/// against it — see [`client_id_is_configured`], which the login dialog checks
/// so the user gets an explanation instead of an opaque OAuth error.
pub const CLIENT_ID: &str = match option_env!("FORQEN_CLIENT_ID") {
    Some(id) => id,
    None => PLACEHOLDER_CLIENT_ID,
};

const PLACEHOLDER_CLIENT_ID: &str = "Iv23liFORQENPLACEHOLDER";

/// Whether a real GitHub App id was compiled in.
///
/// Checked before starting a device flow: without it the request fails with
/// `incorrect_client_credentials`, which tells the user nothing about the
/// actual cause.
pub fn client_id_is_configured() -> bool {
    CLIENT_ID != PLACEHOLDER_CLIENT_ID
}

pub const DEFAULT_HOST: &str = "github.com";

/// Scopes requested at login. `workflow` is deliberately absent — it is
/// requested incrementally the first time the Actions view is opened.
pub const BASE_SCOPES: &[&str] = &["repo", "read:org", "gist", "notifications", "user:email"];

/// A token string that never renders its contents.
///
/// `Debug` and `Display` are both redacted, so a token cannot reach a log,
/// a `tracing` field, or a panic message by accident. Reading the real value
/// requires calling [`Secret::expose`], which is greppable in review.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Yield the underlying token. Every call site should be obvious in review.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// A set of credentials for one account on one host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Token {
    pub access: Secret,
    /// Present for GitHub App tokens, absent for OAuth App tokens and PATs.
    pub refresh: Option<Secret>,
    /// Absent means the token does not expire (OAuth App token or PAT).
    pub expires_at: Option<SystemTime>,
    pub scopes: Vec<String>,
}

impl Token {
    /// Refresh proactively rather than waiting for a 401, so a long-running
    /// operation does not fail midway on an expiry we could see coming.
    const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);

    pub fn needs_refresh(&self, now: SystemTime) -> bool {
        match self.expires_at {
            Some(exp) => exp
                .checked_sub(Self::REFRESH_MARGIN)
                .is_none_or(|deadline| now >= deadline),
            None => false,
        }
    }
}

/// One authenticated identity. Multi-account is modelled from day one because
/// retrofitting it means migrating the schema and rewriting every API call site.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Account {
    /// `github.com`, or an Enterprise Server hostname.
    pub host: String,
    pub login: String,
    pub is_default_for_host: bool,
}

impl Account {
    /// Keyring entry name. Host is included so `github.com` and a GHES instance
    /// can hold accounts with the same login without colliding.
    pub fn keyring_key(&self) -> String {
        format!("{}:{}", self.host, self.login)
    }

    /// Base URL for OAuth endpoints. GHES serves them off its own hostname.
    pub fn oauth_base(&self) -> String {
        format!("https://{}", self.host)
    }

    /// Base URL for the REST API. github.com uses a separate api. subdomain;
    /// GHES nests the API under `/api/v3` on the same host.
    pub fn api_base(&self) -> String {
        if self.host == DEFAULT_HOST {
            "https://api.github.com".into()
        } else {
            format!("https://{}/api/v3", self.host)
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("network error talking to {host}: {source}")]
    Network {
        host: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("GitHub rejected the request: {0}")]
    Oauth(String),

    /// The user did not approve in time. Recoverable: start a new flow.
    #[error("the login code expired before it was approved")]
    Expired,

    /// The user pressed "Cancel" on GitHub's approval page.
    #[error("login was denied")]
    Denied,

    #[error("polled too many times without a decision")]
    PollLimit,

    /// No Secret Service on the session bus. Deliberately fatal: silently
    /// falling back to a plaintext file would hand out tokens to any process
    /// that can read $HOME.
    #[error(
        "no system keyring available. forqen stores tokens in the Secret Service \
         and will not write them to disk in plaintext. Install and start \
         gnome-keyring (or KWallet), then try again."
    )]
    NoKeyring,

    #[error("keyring error: {0}")]
    Keyring(#[from] keyring::Error),

    #[error("no credentials stored for {0}")]
    NotFound(String),

    #[error("malformed response from {host}: {detail}")]
    Malformed { host: String, detail: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_never_renders_its_contents() {
        let s = Secret::new("ghu_supersecrettokenvalue");
        assert_eq!(format!("{s:?}"), "Secret(<redacted>)");
        assert_eq!(format!("{s}"), "<redacted>");
        assert!(!format!("{s:?} {s}").contains("supersecret"));

        // The redaction must survive being nested in the structs we actually log.
        let tok = Token {
            access: s.clone(),
            refresh: Some(Secret::new("ghr_refreshvalue")),
            expires_at: None,
            scopes: vec!["repo".into()],
        };
        let rendered = format!("{tok:?}");
        assert!(!rendered.contains("supersecret"), "{rendered}");
        assert!(!rendered.contains("refreshvalue"), "{rendered}");
    }

    #[test]
    fn non_expiring_tokens_never_ask_for_refresh() {
        let tok = Token {
            access: Secret::new("pat"),
            refresh: None,
            expires_at: None,
            scopes: vec![],
        };
        assert!(!tok.needs_refresh(SystemTime::now()));
    }

    #[test]
    fn refresh_fires_inside_the_margin_not_outside() {
        let now = SystemTime::now();
        let mk = |d: Duration| Token {
            access: Secret::new("t"),
            refresh: Some(Secret::new("r")),
            expires_at: Some(now + d),
            scopes: vec![],
        };

        assert!(!mk(Duration::from_secs(30 * 60)).needs_refresh(now));
        // Inside the 5-minute margin: refresh even though it has not expired.
        assert!(mk(Duration::from_secs(60)).needs_refresh(now));
        assert!(mk(Duration::from_secs(0)).needs_refresh(now));
    }

    #[test]
    fn already_expired_token_refreshes_rather_than_underflowing() {
        // expires_at - 5min underflows past the epoch; must not panic and must
        // still report that a refresh is due.
        let tok = Token {
            access: Secret::new("t"),
            refresh: Some(Secret::new("r")),
            expires_at: Some(SystemTime::UNIX_EPOCH),
            scopes: vec![],
        };
        assert!(tok.needs_refresh(SystemTime::now()));
    }

    #[test]
    fn api_base_differs_between_dotcom_and_enterprise() {
        let dotcom = Account {
            host: DEFAULT_HOST.into(),
            login: "n1th1n-19".into(),
            is_default_for_host: true,
        };
        assert_eq!(dotcom.api_base(), "https://api.github.com");
        assert_eq!(dotcom.oauth_base(), "https://github.com");

        let ghes = Account {
            host: "git.corp.example".into(),
            login: "n1th1n-19".into(),
            is_default_for_host: true,
        };
        assert_eq!(ghes.api_base(), "https://git.corp.example/api/v3");
        assert_eq!(ghes.oauth_base(), "https://git.corp.example");

        // Same login on two hosts must not share a keyring entry.
        assert_ne!(dotcom.keyring_key(), ghes.keyring_key());
    }
}
