//! GitHub API client.
//!
//! Built directly on `reqwest` rather than on a wrapper crate, because the
//! central design constraint here is *conditional requests* — attaching
//! `If-None-Match` and handling `304 Not Modified` — and the convenience
//! wrappers hide exactly that layer. A `304` does not count against GitHub's
//! primary rate limit, so revalidation is the difference between a polled
//! notifications inbox being free and being impossible.

pub mod models;
pub mod rate_limit;

use std::sync::{Arc, Mutex};

use serde::de::DeserializeOwned;

use auth::{Account, Secret};
use db::Db;

pub use rate_limit::RateLimit;

const USER_AGENT: &str = concat!("forqen/", env!("CARGO_PKG_VERSION"));

/// GitHub's REST media type. Pinning the API version keeps a server-side
/// default change from silently altering response shapes.
const ACCEPT: &str = "application/vnd.github+json";
const API_VERSION: &str = "2022-11-28";

#[derive(Debug, thiserror::Error)]
pub enum GhError {
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),

    #[error("github returned {status}: {message}")]
    Api { status: u16, message: String },

    #[error("authentication failed or the token was revoked")]
    Unauthorized,

    #[error("rate limited; resets in {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },

    #[error("could not decode response: {0}")]
    Decode(String),

    #[error("cache error: {0}")]
    Db(#[from] db::DbError),

    /// A `304` arrived but nothing was stored to satisfy it. Means the cache
    /// entry was pruned between the request being built and the response
    /// landing; the caller should retry unconditionally.
    #[error("server said not-modified but no cached body exists for {0}")]
    MissingCacheEntry(String),
}

/// Where a response came from. The UI shows a staleness banner for anything
/// that is not `Fresh`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provenance {
    /// A `200` off the wire.
    Fresh,
    /// A `304` — the cached body is confirmed current.
    Revalidated,
    /// The network was unreachable; this is the last known good body.
    OfflineCache,
}

pub struct Response<T> {
    pub data: T,
    pub provenance: Provenance,
}

pub struct Client {
    http: reqwest::Client,
    account: Account,
    token: Secret,
    cache: Arc<Db>,
    limit: Mutex<RateLimit>,
}

impl Client {
    pub fn new(account: Account, token: Secret, cache: Arc<Db>) -> Result<Self, GhError> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            // GitHub closes idle connections at 60s; staying under that avoids
            // a reconnect storm on a polled inbox.
            .pool_idle_timeout(std::time::Duration::from_secs(50))
            .build()?;

        Ok(Self {
            http,
            account,
            token,
            cache,
            limit: Mutex::new(RateLimit::unknown()),
        })
    }

    pub fn account(&self) -> &Account {
        &self.account
    }

    pub fn rate_limit(&self) -> RateLimit {
        *self.limit.lock().expect("rate limit mutex poisoned")
    }

    /// GET a JSON resource, revalidating against the cache.
    ///
    /// `path` is relative to the host's API base, e.g. `"/user/repos"`.
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<Response<T>, GhError> {
        let url = format!("{}{}", self.account.api_base(), path);
        let cached = self.cache.cached(&url)?;

        let mut req = self
            .http
            .get(&url)
            .header("Accept", ACCEPT)
            .header("X-GitHub-Api-Version", API_VERSION)
            .bearer_auth(self.token.expose());

        if let Some(c) = &cached {
            if let Some(etag) = &c.etag {
                req = req.header("If-None-Match", etag.clone());
            } else if let Some(lm) = &c.last_modified {
                req = req.header("If-Modified-Since", lm.clone());
            }
        }

        let resp = match req.send().await {
            Ok(r) => r,
            // Offline. Serving the stored body beats an error page — the user
            // can still read what they fetched last time.
            Err(e) if e.is_connect() || e.is_timeout() => {
                let Some(c) = cached else {
                    return Err(GhError::Network(e));
                };
                tracing::warn!(%url, "offline; serving cached body");
                return Ok(Response {
                    data: decode(&c.body)?,
                    provenance: Provenance::OfflineCache,
                });
            }
            Err(e) => return Err(GhError::Network(e)),
        };

        self.record_limits(resp.headers());

        match resp.status().as_u16() {
            304 => {
                let Some(c) = cached else {
                    return Err(GhError::MissingCacheEntry(url));
                };
                self.cache.touch(&url)?;
                Ok(Response {
                    data: decode(&c.body)?,
                    provenance: Provenance::Revalidated,
                })
            }
            200 => {
                let etag = header(resp.headers(), "etag");
                let last_modified = header(resp.headers(), "last-modified");
                let body = resp.bytes().await?;
                self.cache
                    .store(&url, etag.as_deref(), last_modified.as_deref(), &body)?;
                Ok(Response {
                    data: decode(&body)?,
                    provenance: Provenance::Fresh,
                })
            }
            401 => Err(GhError::Unauthorized),
            403 | 429 => {
                // 403 is overloaded: primary rate limit, secondary rate limit,
                // or a genuine permission denial. The headers disambiguate.
                let retry = header(resp.headers(), "retry-after")
                    .and_then(|v| v.parse().ok())
                    .or_else(|| {
                        let l = self.rate_limit();
                        (l.remaining == Some(0)).then(|| l.seconds_until_reset())
                    });
                match retry {
                    Some(secs) => Err(GhError::RateLimited {
                        retry_after_secs: secs,
                    }),
                    None => Err(api_error(403, resp.text().await.unwrap_or_default())),
                }
            }
            status => Err(api_error(status, resp.text().await.unwrap_or_default())),
        }
    }

    fn record_limits(&self, headers: &reqwest::header::HeaderMap) {
        let parsed = RateLimit::from_headers(headers);
        *self.limit.lock().expect("rate limit mutex poisoned") = parsed;
    }
}

fn header(h: &reqwest::header::HeaderMap, name: &str) -> Option<String> {
    h.get(name)?.to_str().ok().map(str::to_owned)
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, GhError> {
    serde_json::from_slice(bytes).map_err(|e| GhError::Decode(e.to_string()))
}

/// GitHub error bodies carry a human-readable `message`; surface that rather
/// than the raw JSON, which is what ends up in a toast.
fn api_error(status: u16, body: String) -> GhError {
    let message = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("message")?.as_str().map(str::to_owned))
        .unwrap_or(body);
    GhError::Api { status, message }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_error_prefers_githubs_message_field() {
        let e = api_error(
            404,
            r#"{"message":"Not Found","documentation_url":"..."}"#.into(),
        );
        match e {
            GhError::Api { status, message } => {
                assert_eq!(status, 404);
                assert_eq!(message, "Not Found");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn api_error_falls_back_to_the_raw_body_when_it_is_not_json() {
        let e = api_error(502, "<html>bad gateway</html>".into());
        match e {
            GhError::Api { message, .. } => assert!(message.contains("bad gateway")),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn decode_surfaces_a_readable_error() {
        let r: Result<models::User, _> = decode(b"not json");
        assert!(matches!(r, Err(GhError::Decode(_))));
    }
}
