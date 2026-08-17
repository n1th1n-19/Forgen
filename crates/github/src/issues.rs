//! Issues.
//!
//! The list endpoint has a trap: `/repos/{owner}/{repo}/issues` returns **pull
//! requests as well**, because GitHub models a pull request as an issue with
//! extra fields. The only marker is the presence of a `pull_request` object.
//! A client that forgets shows every open PR twice — once under Issues and
//! once under Pull Requests — and lets you "close" a PR from the wrong screen.
//! Filtering is done here so no caller has to remember.

use serde::{Deserialize, Serialize};

use crate::{Client, GhError, Response};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IssueState {
    Open,
    Closed,
    All,
}

impl IssueState {
    fn as_param(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::All => "all",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub body: Option<String>,
    pub user: Option<crate::models::User>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub html_url: Option<String>,
    pub comments: Option<u32>,
    #[serde(default)]
    pub labels: Vec<crate::pulls::Label>,
    #[serde(default)]
    pub assignees: Vec<crate::models::User>,
    /// Present only when this "issue" is really a pull request. Never rendered;
    /// its existence is the whole signal.
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
}

impl Issue {
    /// True when the API handed back a pull request wearing an issue's shape.
    pub fn is_pull_request(&self) -> bool {
        self.pull_request.is_some()
    }

    pub fn is_open(&self) -> bool {
        self.state == "open"
    }

    pub fn author(&self) -> &str {
        self.user
            .as_ref()
            .map(|u| u.login.as_str())
            .unwrap_or("ghost")
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct IssueComment {
    pub id: u64,
    pub body: String,
    pub user: Option<crate::models::User>,
    pub created_at: Option<String>,
}

impl IssueComment {
    pub fn author(&self) -> &str {
        self.user
            .as_ref()
            .map(|u| u.login.as_str())
            .unwrap_or("ghost")
    }
}

impl Client {
    /// Issues for a repository, pull requests excluded.
    pub async fn issues(
        &self,
        owner: &str,
        repo: &str,
        state: IssueState,
    ) -> Result<Response<Vec<Issue>>, GhError> {
        let Response { data, provenance } = self
            .get::<Vec<Issue>>(&format!(
                "/repos/{owner}/{repo}/issues?state={}&sort=updated&direction=desc&per_page=50",
                state.as_param()
            ))
            .await?;

        Ok(Response {
            data: data.into_iter().filter(|i| !i.is_pull_request()).collect(),
            provenance,
        })
    }

    pub async fn issue(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Response<Issue>, GhError> {
        self.get(&format!("/repos/{owner}/{repo}/issues/{number}"))
            .await
    }

    pub async fn issue_comments(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Response<Vec<IssueComment>>, GhError> {
        self.get(&format!(
            "/repos/{owner}/{repo}/issues/{number}/comments?per_page=100"
        ))
        .await
    }

    pub async fn comment_on_issue(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<(), GhError> {
        if body.trim().is_empty() {
            return Err(GhError::Api {
                status: 422,
                message: "an empty comment says nothing".into(),
            });
        }
        self.post_no_content(
            &format!("/repos/{owner}/{repo}/issues/{number}/comments"),
            serde_json::json!({ "body": body }),
        )
        .await
    }

    /// Close or reopen.
    pub async fn set_issue_state(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        open: bool,
    ) -> Result<(), GhError> {
        self.patch_json(
            &format!("/repos/{owner}/{repo}/issues/{number}"),
            serde_json::json!({ "state": if open { "open" } else { "closed" } }),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed pair from a real response: one genuine issue, one pull request
    /// that the same endpoint returns alongside it.
    const MIXED: &str = r#"[
      {
        "number": 12, "title": "Crash on open", "state": "open",
        "body": "steps to reproduce", "user": {"login":"reporter","id":1},
        "comments": 3, "labels": [{"name":"bug","color":"d73a4a"}]
      },
      {
        "number": 13, "title": "Fix the crash", "state": "open",
        "user": {"login":"contributor","id":2},
        "pull_request": {"url":"https://api.github.com/repos/o/r/pulls/13"}
      }
    ]"#;

    #[test]
    fn pull_requests_are_identified_by_the_marker_field() {
        let items: Vec<Issue> = serde_json::from_str(MIXED).unwrap();
        assert_eq!(items.len(), 2, "the endpoint really does return both");
        assert!(!items[0].is_pull_request());
        assert!(
            items[1].is_pull_request(),
            "a pull request arrives on the issues endpoint wearing an issue's shape"
        );
    }

    #[test]
    fn filtering_leaves_only_real_issues() {
        let items: Vec<Issue> = serde_json::from_str(MIXED).unwrap();
        let filtered: Vec<_> = items.into_iter().filter(|i| !i.is_pull_request()).collect();

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].number, 12);
        assert_eq!(filtered[0].title, "Crash on open");
    }

    #[test]
    fn reads_the_fields_the_list_shows() {
        let items: Vec<Issue> = serde_json::from_str(MIXED).unwrap();
        let i = &items[0];
        assert_eq!(i.author(), "reporter");
        assert!(i.is_open());
        assert_eq!(i.comments, Some(3));
        assert_eq!(i.labels[0].name, "bug");
        assert!(i.assignees.is_empty(), "missing assignees default to empty");
    }

    #[test]
    fn a_deleted_author_reads_as_ghost_rather_than_blank() {
        let i: Issue =
            serde_json::from_str(r#"{"number":1,"title":"t","state":"closed","user":null}"#)
                .unwrap();
        assert_eq!(i.author(), "ghost");
        assert!(!i.is_open());
    }

    #[test]
    fn comments_parse_with_or_without_an_author() {
        let cs: Vec<IssueComment> = serde_json::from_str(
            r#"[{"id":1,"body":"me too","user":{"login":"someone","id":3},
                 "created_at":"2026-01-01T00:00:00Z"},
                {"id":2,"body":"orphaned","user":null}]"#,
        )
        .unwrap();
        assert_eq!(cs[0].author(), "someone");
        assert_eq!(cs[1].author(), "ghost");
        assert_eq!(cs[0].body, "me too");
    }

    #[test]
    fn state_params_map_to_the_api_strings() {
        assert_eq!(IssueState::Open.as_param(), "open");
        assert_eq!(IssueState::Closed.as_param(), "closed");
        assert_eq!(IssueState::All.as_param(), "all");
    }
}
