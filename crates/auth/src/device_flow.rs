//! OAuth 2.0 Device Authorization Grant (RFC 8628) against GitHub.
//!
//! Chosen over the authorization-code flow because a desktop app cannot hold a
//! `client_secret`, and because this flow needs no localhost listener and no
//! custom URI scheme — it therefore also works over SSH on a headless machine.
//!
//! The network I/O and the decision logic are deliberately separate:
//! [`handle_poll_response`] is a pure function over a parsed response, so the
//! whole retry policy is unit-testable without a socket.

use std::time::{Duration, SystemTime};

use serde::Deserialize;

use crate::{AuthError, Secret, Token};

/// Hard ceiling on poll attempts. GitHub codes live 15 minutes at a 5s
/// interval (~180 polls); this bounds a pathological `slow_down` storm.
const MAX_ATTEMPTS: u32 = 300;

/// Floor for the poll interval, applied even if GitHub reports something lower.
const MIN_INTERVAL: Duration = Duration::from_secs(5);

/// Minimum backoff added on `slow_down` when GitHub supplies no new interval.
/// RFC 8628 §3.5 requires the interval to increase by at least 5 seconds.
const SLOW_DOWN_BUMP: Duration = Duration::from_secs(5);

/// Ceiling on the poll interval. A device code lives ~15 minutes, so backing
/// off past this only guarantees we miss the approval window entirely.
const MAX_INTERVAL: Duration = Duration::from_secs(120);

/// What the user must do to approve, plus what we need to poll with.
#[derive(Clone, Debug)]
pub struct DeviceCode {
    /// Shown to the user, e.g. `WDJB-MJHT`. Short and typo-resistant.
    pub user_code: String,
    /// Where the user enters it.
    pub verification_uri: String,
    /// Sent back when polling. Not shown to the user, so it is a [`Secret`].
    pub device_code: Secret,
    pub interval: Duration,
    pub expires_at: SystemTime,
}

#[derive(Deserialize)]
struct DeviceCodeRaw {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
}

/// Step 1: ask GitHub for a code pair.
pub async fn start(
    http: &reqwest::Client,
    oauth_base: &str,
    client_id: &str,
    scopes: &[&str],
) -> Result<DeviceCode, AuthError> {
    let host = oauth_base.to_owned();
    let resp = http
        .post(format!("{oauth_base}/login/device/code"))
        .header("Accept", "application/json")
        .form(&[("client_id", client_id), ("scope", &scopes.join(" "))])
        .send()
        .await
        .map_err(|source| AuthError::Network {
            host: host.clone(),
            source,
        })?;

    let raw: DeviceCodeRaw = resp.json().await.map_err(|e| AuthError::Malformed {
        host: host.clone(),
        detail: e.to_string(),
    })?;

    Ok(DeviceCode {
        user_code: raw.user_code,
        verification_uri: raw.verification_uri,
        device_code: Secret::new(raw.device_code),
        interval: Duration::from_secs(raw.interval).max(MIN_INTERVAL),
        expires_at: SystemTime::now() + Duration::from_secs(raw.expires_in),
    })
}

/// A parsed poll response, normalised away from GitHub's wire format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PollResponse {
    /// The user approved. Credentials attached.
    Approved(Token),
    /// `authorization_pending` — the user has not finished yet. Normal.
    Pending,
    /// `slow_down` — we polled too fast. GitHub may supply a new interval.
    /// Ignoring this gets the *App* rate-limited for every user, not just this one.
    SlowDown { suggested: Option<Duration> },
    /// `expired_token` — the code aged out before approval.
    Expired,
    /// `access_denied` — the user pressed Cancel.
    Denied,
    /// Anything else (`incorrect_client_credentials`, `device_flow_disabled`, …).
    Fatal(String),
}

/// Mutable retry state carried across poll attempts.
#[derive(Clone, Debug)]
pub struct PollState {
    pub interval: Duration,
    pub attempts: u32,
    pub expires_at: SystemTime,
}

impl PollState {
    pub fn new(code: &DeviceCode) -> Self {
        Self {
            interval: code.interval,
            attempts: 0,
            expires_at: code.expires_at,
        }
    }
}

/// What the polling loop should do next.
#[derive(Debug)]
pub enum Next {
    /// Sleep this long, then poll again.
    Wait(Duration),
    /// Login succeeded.
    Done(Box<Token>),
    /// Give up with this error.
    Fail(AuthError),
}

/// Decide what to do after one poll. Pure: no I/O, no clock reads beyond `now`.
///
/// This function is both the login UX and the rate-limit posture. Polling too
/// eagerly gets the **App** rate-limited, which breaks login for every forqen
/// user rather than just the one logging in; backing off too hard leaves
/// someone who already approved staring at a spinner.
///
/// The policy implemented here:
///
/// * `Pending` keeps the current interval. GitHub's suggested interval is
///   already tuned, and inventing extra backoff on the normal path only makes
///   a fast approval feel slow.
/// * `SlowDown` always increases the interval — it is a directive, not a hint.
///   GitHub's `suggested` value wins when present; otherwise the RFC 8628
///   minimum bump of 5s applies. Capped at [`MAX_INTERVAL`] so a malformed or
///   hostile value cannot back off past the code's own lifetime.
/// * Expiry is checked before the attempt ceiling, so an aged-out code reports
///   the reason the user can act on rather than an internal limit.
///
/// Swap the `Pending` branch for an escalating backoff if App-level rate
/// limiting ever shows up in practice; the tests pin the invariants, not the
/// exact numbers.
pub fn handle_poll_response(resp: PollResponse, state: &mut PollState, now: SystemTime) -> Next {
    // Terminal outcomes first — these are answers, not reasons to keep waiting.
    let slow_down = match resp {
        PollResponse::Approved(token) => return Next::Done(Box::new(token)),
        PollResponse::Expired => return Next::Fail(AuthError::Expired),
        PollResponse::Denied => return Next::Fail(AuthError::Denied),
        PollResponse::Fatal(e) => return Next::Fail(AuthError::Oauth(e)),
        PollResponse::SlowDown { suggested } => Some(suggested),
        PollResponse::Pending => None,
    };

    // A code past its deadline will never be approved. Fail with the reason the
    // user can act on rather than spinning to the attempt ceiling first.
    if now >= state.expires_at {
        return Next::Fail(AuthError::Expired);
    }
    if state.attempts >= MAX_ATTEMPTS {
        return Next::Fail(AuthError::PollLimit);
    }
    state.attempts += 1;

    if let Some(suggested) = slow_down {
        let floor = state.interval + SLOW_DOWN_BUMP;
        state.interval = suggested.unwrap_or(floor).max(floor).min(MAX_INTERVAL);
    }

    Next::Wait(state.interval)
}

/// Wire format of the token endpoint. Success and error share one shape.
#[derive(Deserialize)]
struct PollRaw {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    scope: Option<String>,
    error: Option<String>,
    interval: Option<u64>,
}

impl PollRaw {
    fn normalise(self, now: SystemTime) -> PollResponse {
        if let Some(access) = self.access_token {
            return PollResponse::Approved(Token {
                access: Secret::new(access),
                refresh: self.refresh_token.map(Secret::new),
                expires_at: self.expires_in.map(|s| now + Duration::from_secs(s)),
                scopes: self
                    .scope
                    .unwrap_or_default()
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect(),
            });
        }
        match self.error.as_deref() {
            Some("authorization_pending") => PollResponse::Pending,
            Some("slow_down") => PollResponse::SlowDown {
                suggested: self.interval.map(Duration::from_secs),
            },
            Some("expired_token") => PollResponse::Expired,
            Some("access_denied") => PollResponse::Denied,
            Some(other) => PollResponse::Fatal(other.to_owned()),
            None => PollResponse::Fatal("response had neither a token nor an error".into()),
        }
    }
}

async fn poll_once(
    http: &reqwest::Client,
    oauth_base: &str,
    client_id: &str,
    device_code: &Secret,
) -> Result<PollResponse, AuthError> {
    let host = oauth_base.to_owned();
    let resp = http
        .post(format!("{oauth_base}/login/oauth/access_token"))
        .header("Accept", "application/json")
        .form(&[
            ("client_id", client_id),
            ("device_code", device_code.expose()),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ])
        .send()
        .await
        .map_err(|source| AuthError::Network {
            host: host.clone(),
            source,
        })?;

    let raw: PollRaw = resp.json().await.map_err(|e| AuthError::Malformed {
        host,
        detail: e.to_string(),
    })?;

    Ok(raw.normalise(SystemTime::now()))
}

/// Step 2: poll until the user approves, denies, or the code expires.
pub async fn poll_until_complete(
    http: &reqwest::Client,
    oauth_base: &str,
    client_id: &str,
    code: &DeviceCode,
) -> Result<Token, AuthError> {
    let mut state = PollState::new(code);
    loop {
        tokio::time::sleep(state.interval).await;
        let resp = poll_once(http, oauth_base, client_id, &code.device_code).await?;

        // Note the deliberate absence of the response in this log line: an
        // `Approved` variant carries live credentials.
        tracing::debug!(attempt = state.attempts, "device flow poll");

        match handle_poll_response(resp, &mut state, SystemTime::now()) {
            Next::Done(token) => return Ok(*token),
            Next::Fail(e) => return Err(e),
            Next::Wait(d) => state.interval = d,
        }
    }
}

/// Exchange a refresh token for a fresh access token. GitHub App user tokens
/// live 8 hours, so this runs far more often than the device flow itself.
pub async fn refresh(
    http: &reqwest::Client,
    oauth_base: &str,
    client_id: &str,
    refresh_token: &Secret,
) -> Result<Token, AuthError> {
    let host = oauth_base.to_owned();
    let resp = http
        .post(format!("{oauth_base}/login/oauth/access_token"))
        .header("Accept", "application/json")
        .form(&[
            ("client_id", client_id),
            ("refresh_token", refresh_token.expose()),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await
        .map_err(|source| AuthError::Network {
            host: host.clone(),
            source,
        })?;

    let raw: PollRaw = resp.json().await.map_err(|e| AuthError::Malformed {
        host,
        detail: e.to_string(),
    })?;

    match raw.normalise(SystemTime::now()) {
        PollResponse::Approved(t) => Ok(t),
        PollResponse::Fatal(e) => Err(AuthError::Oauth(e)),
        // A refresh token that is itself expired or revoked lands here; the
        // caller must fall back to a full device flow.
        other => Err(AuthError::Oauth(format!(
            "unexpected refresh response: {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> PollState {
        PollState {
            interval: Duration::from_secs(5),
            attempts: 0,
            expires_at: SystemTime::now() + Duration::from_secs(900),
        }
    }

    fn token() -> Token {
        Token {
            access: Secret::new("ghu_x"),
            refresh: Some(Secret::new("ghr_x")),
            expires_at: None,
            scopes: vec![],
        }
    }

    // --- wire parsing: covered now, independent of the TODO above -----------

    #[test]
    fn parses_success_into_approved_with_scopes_split() {
        let raw: PollRaw = serde_json::from_str(
            r#"{"access_token":"ghu_a","refresh_token":"ghr_b",
                "expires_in":28800,"scope":"repo read:org gist"}"#,
        )
        .unwrap();
        let now = SystemTime::now();
        match raw.normalise(now) {
            PollResponse::Approved(t) => {
                assert_eq!(t.access.expose(), "ghu_a");
                assert_eq!(t.refresh.unwrap().expose(), "ghr_b");
                assert_eq!(t.scopes, ["repo", "read:org", "gist"]);
                assert!(t.expires_at.unwrap() > now);
            }
            other => panic!("expected Approved, got {other:?}"),
        }
    }

    #[test]
    fn parses_each_error_code_to_its_variant() {
        let cases = [
            (
                r#"{"error":"authorization_pending"}"#,
                PollResponse::Pending,
            ),
            (
                r#"{"error":"slow_down","interval":10}"#,
                PollResponse::SlowDown {
                    suggested: Some(Duration::from_secs(10)),
                },
            ),
            (
                r#"{"error":"slow_down"}"#,
                PollResponse::SlowDown { suggested: None },
            ),
            (r#"{"error":"expired_token"}"#, PollResponse::Expired),
            (r#"{"error":"access_denied"}"#, PollResponse::Denied),
        ];
        for (json, want) in cases {
            let raw: PollRaw = serde_json::from_str(json).unwrap();
            assert_eq!(raw.normalise(SystemTime::now()), want, "input: {json}");
        }
    }

    #[test]
    fn a_response_with_neither_token_nor_error_is_fatal_not_silently_pending() {
        let raw: PollRaw = serde_json::from_str("{}").unwrap();
        assert!(matches!(
            raw.normalise(SystemTime::now()),
            PollResponse::Fatal(_)
        ));
    }

    // --- the contract for handle_poll_response ------------------------------
    // These pin the invariants, not the timings: a different backoff policy
    // must still satisfy every one of them.

    #[test]
    fn approved_finishes() {
        let mut s = state();
        assert!(matches!(
            handle_poll_response(PollResponse::Approved(token()), &mut s, SystemTime::now()),
            Next::Done(_)
        ));
    }

    #[test]
    fn pending_keeps_waiting_and_counts_the_attempt() {
        let mut s = state();
        assert!(matches!(
            handle_poll_response(PollResponse::Pending, &mut s, SystemTime::now()),
            Next::Wait(_)
        ));
        assert_eq!(
            s.attempts, 1,
            "attempts must advance or the ceiling never trips"
        );
    }

    #[test]
    fn slow_down_must_actually_increase_the_interval() {
        let mut s = state();
        let before = s.interval;
        let next = handle_poll_response(
            PollResponse::SlowDown { suggested: None },
            &mut s,
            SystemTime::now(),
        );
        match next {
            Next::Wait(d) => assert!(
                d > before,
                "slow_down without a suggestion must still back off: {before:?} -> {d:?}"
            ),
            other => panic!("expected Wait, got {other:?}"),
        }
    }

    #[test]
    fn slow_down_honours_githubs_suggested_interval() {
        let mut s = state();
        let next = handle_poll_response(
            PollResponse::SlowDown {
                suggested: Some(Duration::from_secs(17)),
            },
            &mut s,
            SystemTime::now(),
        );
        match next {
            Next::Wait(d) => assert!(d >= Duration::from_secs(17), "got {d:?}"),
            other => panic!("expected Wait, got {other:?}"),
        }
    }

    #[test]
    fn terminal_responses_map_to_their_errors() {
        let now = SystemTime::now();
        assert!(matches!(
            handle_poll_response(PollResponse::Expired, &mut state(), now),
            Next::Fail(AuthError::Expired)
        ));
        assert!(matches!(
            handle_poll_response(PollResponse::Denied, &mut state(), now),
            Next::Fail(AuthError::Denied)
        ));
        assert!(matches!(
            handle_poll_response(
                PollResponse::Fatal("device_flow_disabled".into()),
                &mut state(),
                now
            ),
            Next::Fail(AuthError::Oauth(_))
        ));
    }

    #[test]
    fn a_code_past_its_deadline_fails_instead_of_polling_on() {
        let now = SystemTime::now();
        let mut s = PollState {
            expires_at: now - Duration::from_secs(1),
            ..state()
        };
        assert!(
            matches!(
                handle_poll_response(PollResponse::Pending, &mut s, now),
                Next::Fail(AuthError::Expired)
            ),
            "an aged-out code must fail fast, not spin to the attempt ceiling"
        );
    }

    #[test]
    fn the_attempt_ceiling_is_enforced() {
        let mut s = PollState {
            attempts: MAX_ATTEMPTS,
            ..state()
        };
        assert!(matches!(
            handle_poll_response(PollResponse::Pending, &mut s, SystemTime::now()),
            Next::Fail(AuthError::PollLimit)
        ));
    }
}
