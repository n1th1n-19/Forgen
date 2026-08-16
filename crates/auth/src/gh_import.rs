//! Adopt an existing `gh` CLI login.
//!
//! Most people who want this app already have `gh` authenticated. Reusing that
//! token turns first run into zero steps instead of a browser round trip.
//!
//! The imported token is a *GitHub CLI* OAuth token: it does not expire and has
//! no refresh token, so [`Token::needs_refresh`] correctly reports false for it.
//! Its scopes are whatever `gh` was granted, which may be narrower than
//! [`crate::BASE_SCOPES`] — the caller should verify before relying on one.

use std::process::Command;

use crate::{AuthError, Secret, Token};

/// Read the token `gh` holds for `host`, if any.
///
/// Returns `Ok(None)` when `gh` is absent or not logged in to that host — both
/// are ordinary states on a fresh machine, not failures.
pub fn token_for(host: &str) -> Result<Option<Secret>, AuthError> {
    let out = match Command::new("gh")
        .args(["auth", "token", "--hostname", host])
        .output()
    {
        Ok(o) => o,
        // `gh` not installed. Expected; not an error worth surfacing.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(AuthError::Malformed {
                host: host.to_owned(),
                detail: format!("could not run gh: {e}"),
            })
        }
    };

    if !out.status.success() {
        // Exit 1 means "not logged in to this host".
        return Ok(None);
    }

    let token = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    Ok((!token.is_empty()).then(|| Secret::new(token)))
}

/// Scopes `gh` was granted on `host`, parsed from `gh auth status`.
///
/// Best-effort: the output format is human-facing and has changed between
/// releases, so a parse miss yields an empty list rather than an error. The
/// authoritative check is the `X-OAuth-Scopes` header on a real API call.
pub fn scopes_for(host: &str) -> Vec<String> {
    let Ok(out) = Command::new("gh")
        .args(["auth", "status", "--hostname", host])
        .output()
    else {
        return Vec::new();
    };

    // `gh` prints status on stderr in some versions, stdout in others.
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    text.lines()
        .find_map(|l| l.split_once("Token scopes:"))
        .map(|(_, rest)| {
            rest.split(',')
                .map(|s| s.trim().trim_matches('\'').to_owned())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Build a [`Token`] from an existing `gh` login.
pub fn import(host: &str) -> Result<Option<Token>, AuthError> {
    Ok(token_for(host)?.map(|access| Token {
        access,
        refresh: None,
        expires_at: None,
        scopes: scopes_for(host),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_gh_or_logged_out_is_none_not_an_error() {
        // Whatever this machine's state, the contract holds: never Err for the
        // ordinary "no gh / not logged in" cases.
        let got = token_for("definitely-not-a-real-host.invalid");
        assert!(matches!(got, Ok(None)), "got {got:?}");
    }

    #[test]
    fn scope_parsing_handles_the_quoted_comma_list() {
        // Shape of the real line:
        //   - Token scopes: 'gist', 'read:org', 'repo'
        let line = "  - Token scopes: 'gist', 'read:org', 'repo'";
        let parsed: Vec<String> = line
            .split_once("Token scopes:")
            .map(|(_, rest)| {
                rest.split(',')
                    .map(|s| s.trim().trim_matches('\'').to_owned())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(parsed, ["gist", "read:org", "repo"]);
    }
}
