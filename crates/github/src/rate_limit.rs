//! Rate-limit accounting from response headers.
//!
//! GitHub reports the budget on every response, so there is no need to spend a
//! request asking `/rate_limit`. Tracking it lets the UI warn before the app
//! starts failing, which matters most on a GitHub App token where the budget is
//! shared across everything the user has open.

use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct RateLimit {
    pub limit: Option<u32>,
    pub remaining: Option<u32>,
    /// Unix seconds at which the window resets.
    pub reset_at: Option<u64>,
    /// Requests used by the most recent call. GitHub charges GraphQL queries
    /// more than one point, so this is not always 1.
    pub used: Option<u32>,
}

impl RateLimit {
    pub fn unknown() -> Self {
        Self::default()
    }

    pub fn from_headers(h: &reqwest::header::HeaderMap) -> Self {
        let num = |name: &str| -> Option<u32> { h.get(name)?.to_str().ok()?.trim().parse().ok() };
        Self {
            limit: num("x-ratelimit-limit"),
            remaining: num("x-ratelimit-remaining"),
            reset_at: h
                .get("x-ratelimit-reset")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse().ok()),
            used: num("x-ratelimit-used"),
        }
    }

    pub fn seconds_until_reset(&self) -> u64 {
        let Some(reset) = self.reset_at else { return 0 };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        reset.saturating_sub(now)
    }

    pub fn is_exhausted(&self) -> bool {
        self.remaining == Some(0)
    }

    /// True once less than 10% of the budget is left.
    ///
    /// Warning at a fraction rather than a fixed count because the budget
    /// differs by an order of magnitude between token types: 5000/hour for a
    /// user token, 60/hour unauthenticated. A "fewer than 100 left" rule would
    /// warn constantly on one and never on the other.
    pub fn is_low(&self) -> bool {
        match (self.remaining, self.limit) {
            (Some(r), Some(l)) if l > 0 => (r as u64) * 10 < (l as u64),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn parses_the_standard_header_set() {
        let l = RateLimit::from_headers(&headers(&[
            ("x-ratelimit-limit", "5000"),
            ("x-ratelimit-remaining", "4998"),
            ("x-ratelimit-reset", "1700000000"),
            ("x-ratelimit-used", "2"),
        ]));
        assert_eq!(l.limit, Some(5000));
        assert_eq!(l.remaining, Some(4998));
        assert_eq!(l.reset_at, Some(1_700_000_000));
        assert_eq!(l.used, Some(2));
        assert!(!l.is_exhausted());
        assert!(!l.is_low());
    }

    #[test]
    fn missing_headers_yield_unknown_rather_than_zero() {
        let l = RateLimit::from_headers(&HeaderMap::new());
        assert_eq!(l, RateLimit::unknown());
        // Unknown must not read as exhausted, or the app blocks itself.
        assert!(!l.is_exhausted());
        assert!(!l.is_low());
    }

    #[test]
    fn low_is_a_fraction_so_it_works_for_both_budget_sizes() {
        let user = RateLimit {
            limit: Some(5000),
            remaining: Some(400),
            ..Default::default()
        };
        assert!(user.is_low(), "400 of 5000 is under 10%");

        let anon = RateLimit {
            limit: Some(60),
            remaining: Some(5),
            ..Default::default()
        };
        assert!(anon.is_low(), "5 of 60 is under 10%");

        let plenty = RateLimit {
            limit: Some(5000),
            remaining: Some(4000),
            ..Default::default()
        };
        assert!(!plenty.is_low());
    }

    #[test]
    fn exhausted_is_zero_remaining() {
        let l = RateLimit {
            limit: Some(5000),
            remaining: Some(0),
            ..Default::default()
        };
        assert!(l.is_exhausted());
    }

    #[test]
    fn a_reset_in_the_past_reports_zero_not_an_underflow() {
        let l = RateLimit {
            reset_at: Some(1),
            ..Default::default()
        };
        assert_eq!(l.seconds_until_reset(), 0);
    }

    #[test]
    fn garbage_header_values_are_ignored_not_fatal() {
        let l = RateLimit::from_headers(&headers(&[
            ("x-ratelimit-limit", "not-a-number"),
            ("x-ratelimit-remaining", ""),
        ]));
        assert_eq!(l.limit, None);
        assert_eq!(l.remaining, None);
    }
}
