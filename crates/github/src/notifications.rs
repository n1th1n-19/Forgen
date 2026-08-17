//! The notifications inbox.
//!
//! This is what the ETag cache was built for. GitHub excludes `304 Not
//! Modified` from the primary rate limit, so a poll that finds nothing new
//! costs latency and no budget — which is the only reason polling every minute
//! is affordable at all. Without conditional requests, a 60-second poll would
//! spend 60 of an hourly 5000 requests doing nothing.
//!
//! GitHub also returns an `X-Poll-Interval` header saying how often it is
//! willing to be asked. Ignoring it is how an application gets rate limited at
//! the *secondary* level, which applies per-app rather than per-user.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{Client, GhError, Response};

/// GitHub's floor if it sends no `X-Poll-Interval`.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Notification {
    pub id: String,
    pub unread: bool,
    /// `subscribed`, `mention`, `review_requested`, `assign`, `author`, …
    pub reason: String,
    pub updated_at: Option<String>,
    pub subject: Subject,
    pub repository: NotificationRepo,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Subject {
    pub title: String,
    /// API URL of the thing this is about. Absent for a Discussion, which has
    /// no REST representation.
    pub url: Option<String>,
    /// `PullRequest`, `Issue`, `Release`, `Discussion`, `CheckSuite`, …
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct NotificationRepo {
    pub name: String,
    pub full_name: String,
}

impl Notification {
    /// Split `owner/name` from the repository.
    pub fn owner_and_repo(&self) -> Option<(&str, &str)> {
        self.repository.full_name.split_once('/')
    }

    /// The pull request or issue number this points at.
    ///
    /// Parsed from the subject URL because the payload has no number field.
    /// `None` for subjects that have none — a Release, or a Discussion, whose
    /// URL is absent entirely.
    pub fn number(&self) -> Option<u64> {
        let url = self.subject.url.as_ref()?;
        if !matches!(self.subject.kind.as_str(), "PullRequest" | "Issue") {
            return None;
        }
        url.rsplit('/').next()?.parse().ok()
    }

    /// Whether this is something the user was specifically asked about, rather
    /// than something they merely watch. Used to sort the inbox: a review
    /// request buried under fifty watched-repository updates is a review that
    /// does not happen.
    pub fn is_direct(&self) -> bool {
        matches!(
            self.reason.as_str(),
            "review_requested" | "mention" | "assign" | "team_mention"
        )
    }
}

/// A page of notifications plus how long to wait before asking again.
pub struct Inbox {
    pub notifications: Vec<Notification>,
    pub poll_after: Duration,
    pub provenance: crate::Provenance,
}

impl Client {
    /// Fetch the inbox.
    ///
    /// `all` includes notifications already read; the default is unread only,
    /// which is what an inbox is for.
    pub async fn notifications(&self, all: bool) -> Result<Inbox, GhError> {
        let path = format!("/notifications?all={}&per_page=50", all);
        let Response { data, provenance } = self.get::<Vec<Notification>>(&path).await?;

        Ok(Inbox {
            notifications: data,
            poll_after: self.poll_interval(),
            provenance,
        })
    }

    /// Mark one thread as read.
    pub async fn mark_read(&self, thread_id: &str) -> Result<(), GhError> {
        self.patch_no_content(&format!("/notifications/threads/{thread_id}"))
            .await
    }

    /// Stop receiving notifications for a thread.
    ///
    /// Distinct from marking read: read means "seen", unsubscribed means "stop
    /// telling me", and conflating them is why inboxes fill back up.
    pub async fn unsubscribe(&self, thread_id: &str) -> Result<(), GhError> {
        self.put_no_content(
            &format!("/notifications/threads/{thread_id}/subscription"),
            serde_json::json!({ "ignored": true }),
        )
        .await
    }
}

/// Parse `X-Poll-Interval`, falling back to GitHub's documented minimum.
pub fn parse_poll_interval(headers: &reqwest::header::HeaderMap) -> Duration {
    headers
        .get("x-poll-interval")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        // Never poll faster than the floor even if the header says something
        // absurd: the penalty lands on the application, not this user.
        .filter(|d| *d >= DEFAULT_POLL_INTERVAL)
        .unwrap_or(DEFAULT_POLL_INTERVAL)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};

    fn notification(kind: &str, url: Option<&str>, reason: &str) -> Notification {
        Notification {
            id: "42".into(),
            unread: true,
            reason: reason.into(),
            updated_at: None,
            subject: Subject {
                title: "Something happened".into(),
                url: url.map(str::to_string),
                kind: kind.into(),
            },
            repository: NotificationRepo {
                name: "forqen".into(),
                full_name: "n1th1n-19/forqen".into(),
            },
        }
    }

    #[test]
    fn extracts_a_pull_request_number_from_the_subject_url() {
        let n = notification(
            "PullRequest",
            Some("https://api.github.com/repos/n1th1n-19/forqen/pulls/2382"),
            "review_requested",
        );
        assert_eq!(n.number(), Some(2382));
        assert_eq!(n.owner_and_repo(), Some(("n1th1n-19", "forqen")));
    }

    #[test]
    fn extracts_an_issue_number_too() {
        let n = notification(
            "Issue",
            Some("https://api.github.com/repos/o/r/issues/17"),
            "subscribed",
        );
        assert_eq!(n.number(), Some(17));
    }

    #[test]
    fn subjects_without_a_number_report_none() {
        // A release URL ends in an id that is not a PR or issue number, so the
        // kind check has to gate the parse or it returns a plausible lie.
        let release = notification(
            "Release",
            Some("https://api.github.com/repos/o/r/releases/98765"),
            "subscribed",
        );
        assert_eq!(release.number(), None);

        // A Discussion has no REST URL at all.
        let discussion = notification("Discussion", None, "subscribed");
        assert_eq!(discussion.number(), None);
    }

    #[test]
    fn direct_reasons_are_distinguished_from_watching() {
        for reason in ["review_requested", "mention", "assign", "team_mention"] {
            assert!(
                notification("PullRequest", None, reason).is_direct(),
                "{reason} is someone asking this user specifically"
            );
        }
        for reason in ["subscribed", "author", "comment", "state_change"] {
            assert!(
                !notification("PullRequest", None, reason).is_direct(),
                "{reason}"
            );
        }
    }

    #[test]
    fn poll_interval_honours_the_header() {
        let mut h = HeaderMap::new();
        h.insert("x-poll-interval", HeaderValue::from_static("120"));
        assert_eq!(parse_poll_interval(&h), Duration::from_secs(120));
    }

    #[test]
    fn poll_interval_never_goes_below_the_floor() {
        // A header asking to be polled every second would get the application
        // secondary-rate-limited for everyone using it.
        let mut h = HeaderMap::new();
        h.insert("x-poll-interval", HeaderValue::from_static("1"));
        assert_eq!(parse_poll_interval(&h), DEFAULT_POLL_INTERVAL);

        assert_eq!(
            parse_poll_interval(&HeaderMap::new()),
            DEFAULT_POLL_INTERVAL
        );
    }

    #[test]
    fn a_garbage_poll_header_falls_back_rather_than_panicking() {
        let mut h = HeaderMap::new();
        h.insert("x-poll-interval", HeaderValue::from_static("soon"));
        assert_eq!(parse_poll_interval(&h), DEFAULT_POLL_INTERVAL);
    }

    #[test]
    fn a_real_payload_deserializes() {
        // Trimmed from an actual /notifications response.
        let json = r#"[{
          "id": "1234",
          "unread": true,
          "reason": "review_requested",
          "updated_at": "2026-08-17T09:00:00Z",
          "subject": {
            "title": "Add exercise async1",
            "url": "https://api.github.com/repos/rust-lang/rustlings/pulls/2382",
            "latest_comment_url": null,
            "type": "PullRequest"
          },
          "repository": {
            "id": 1, "name": "rustlings", "full_name": "rust-lang/rustlings",
            "private": false
          }
        }]"#;
        let items: Vec<Notification> = serde_json::from_str(json).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].subject.title, "Add exercise async1");
        assert_eq!(items[0].number(), Some(2382));
        assert!(items[0].is_direct());
        assert_eq!(items[0].owner_and_repo(), Some(("rust-lang", "rustlings")));
    }
}
