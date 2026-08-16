//! Pull requests.
//!
//! The list and metadata come from REST; review threads come from GraphQL.
//!
//! That split is not stylistic. REST v3 returns review comments as a flat list
//! with an `in_reply_to_id` on each reply, so reconstructing conversations
//! means sorting, grouping and guessing at the ones whose parent has been
//! deleted. GraphQL's `reviewThreads` returns them already threaded, already
//! marked resolved or outdated, and lets one request fetch the PR, its files,
//! its threads and its checks together — five round trips against a rate limit
//! that is shared with everything else the app is doing.

use serde::{Deserialize, Serialize};

use crate::{Client, GhError, Response};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PullState {
    Open,
    Closed,
    All,
}

impl PullState {
    fn as_param(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::All => "all",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PullRef {
    /// Branch name.
    #[serde(rename = "ref")]
    pub branch: String,
    pub sha: String,
    /// Absent when the fork the PR came from has been deleted — which happens,
    /// and is why this cannot be assumed present.
    pub repo: Option<PullRepo>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PullRepo {
    pub name: String,
    pub full_name: String,
    pub clone_url: Option<String>,
    pub ssh_url: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub draft: Option<bool>,
    pub body: Option<String>,
    pub user: Option<crate::models::User>,
    pub head: PullRef,
    pub base: PullRef,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub html_url: Option<String>,
    pub comments: Option<u32>,
    /// Only present on the single-PR endpoint, never on the list.
    pub mergeable: Option<bool>,
    pub additions: Option<u32>,
    pub deletions: Option<u32>,
    pub changed_files: Option<u32>,
    #[serde(default)]
    pub labels: Vec<Label>,
}

impl PullRequest {
    pub fn is_draft(&self) -> bool {
        self.draft.unwrap_or(false)
    }

    /// True when the PR comes from a fork rather than a branch of the same
    /// repository. Checking out a fork PR needs a different refspec.
    pub fn is_from_fork(&self) -> bool {
        match (&self.head.repo, &self.base.repo) {
            (Some(h), Some(b)) => h.full_name != b.full_name,
            // A deleted head repo was a fork; a same-repo branch would still
            // be there.
            (None, _) => true,
            _ => false,
        }
    }

    /// Local branch name to check this PR out into.
    ///
    /// Namespaced by number rather than using the head branch name: two open
    /// PRs from different forks routinely share a branch name like `patch-1`,
    /// and checking the second one out would collide with the first.
    pub fn local_branch(&self) -> String {
        format!("pr/{}", self.number)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Label {
    pub name: String,
    /// Six hex digits, no leading `#`.
    pub color: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PullFile {
    pub filename: String,
    pub status: String,
    pub additions: u32,
    pub deletions: u32,
    /// Unified diff for this file. Absent for binary files, and omitted
    /// entirely by GitHub once a diff grows past its size limit.
    pub patch: Option<String>,
    pub previous_filename: Option<String>,
}

/// Combined CI state for a commit.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CheckStatus {
    /// `success`, `pending`, or `failure`.
    pub state: String,
    pub total_count: u32,
    #[serde(default)]
    pub statuses: Vec<CheckRun>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CheckRun {
    pub state: String,
    pub context: String,
    pub description: Option<String>,
    pub target_url: Option<String>,
}

impl Client {
    /// Pull requests for a repository, most recently updated first.
    pub async fn pulls(
        &self,
        owner: &str,
        repo: &str,
        state: PullState,
    ) -> Result<Response<Vec<PullRequest>>, GhError> {
        self.get(&format!(
            "/repos/{owner}/{repo}/pulls?state={}&sort=updated&direction=desc&per_page=50",
            state.as_param()
        ))
        .await
    }

    /// One pull request, with the fields the list omits — `mergeable`,
    /// `additions`, `deletions`, `changed_files`.
    pub async fn pull(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Response<PullRequest>, GhError> {
        self.get(&format!("/repos/{owner}/{repo}/pulls/{number}"))
            .await
    }

    /// Files changed by a pull request.
    ///
    /// Capped at 300 files by GitHub regardless of `per_page`; a PR larger than
    /// that has to be read locally after checking it out.
    pub async fn pull_files(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Response<Vec<PullFile>>, GhError> {
        self.get(&format!(
            "/repos/{owner}/{repo}/pulls/{number}/files?per_page=100"
        ))
        .await
    }

    /// Combined CI status for a commit.
    pub async fn check_status(
        &self,
        owner: &str,
        repo: &str,
        sha: &str,
    ) -> Result<Response<CheckStatus>, GhError> {
        self.get(&format!("/repos/{owner}/{repo}/commits/{sha}/status"))
            .await
    }
}

/// Split `owner/name` out of a git remote URL.
///
/// Handles both `https://github.com/o/r.git` and the scp-like
/// `git@github.com:o/r.git`, which has no scheme and so is not a parseable URL.
/// Returns `None` for anything that is not a recognisable forge remote, since
/// a local path or an unrelated host has no owner/name to find.
pub fn parse_remote(url: &str) -> Option<(String, String)> {
    let trimmed = url.trim().trim_end_matches('/');
    let path = match trimmed.split_once("://") {
        // https://host/owner/name — drop the scheme, then the host.
        Some((_, rest)) => rest.split_once('/')?.1,
        // git@host:owner/name
        None => trimmed.split_once(':')?.1,
    };

    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, name) = path.rsplit_once('/')?;
    // `owner` may still carry a leading path segment on self-hosted setups;
    // take the last one.
    let owner = owner.rsplit('/').next()?;

    (!owner.is_empty() && !name.is_empty()).then(|| (owner.to_string(), name.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_https_and_ssh_remotes() {
        let cases = [
            "https://github.com/n1th1n-19/forqen.git",
            "https://github.com/n1th1n-19/forqen",
            "git@github.com:n1th1n-19/forqen.git",
            "ssh://git@github.com/n1th1n-19/forqen.git",
        ];
        for url in cases {
            assert_eq!(
                parse_remote(url),
                Some(("n1th1n-19".into(), "forqen".into())),
                "failed on {url}"
            );
        }
    }

    #[test]
    fn rejects_remotes_with_no_owner_and_name() {
        assert_eq!(parse_remote("/srv/git/repo.git"), None);
        assert_eq!(parse_remote(""), None);
        assert_eq!(parse_remote("https://github.com/"), None);
    }

    #[test]
    fn handles_a_self_hosted_host_with_a_path_prefix() {
        assert_eq!(
            parse_remote("https://git.corp.example/gitlab/team/project.git"),
            Some(("team".into(), "project".into())),
            "only the last two segments are owner and name"
        );
    }

    #[test]
    fn a_fork_pull_request_is_recognised() {
        let mk = |head: Option<&str>, base: &str| PullRequest {
            number: 7,
            title: "t".into(),
            state: "open".into(),
            draft: None,
            body: None,
            user: None,
            head: PullRef {
                branch: "patch-1".into(),
                sha: "a".into(),
                repo: head.map(|f| PullRepo {
                    name: "forqen".into(),
                    full_name: f.into(),
                    clone_url: None,
                    ssh_url: None,
                }),
            },
            base: PullRef {
                branch: "main".into(),
                sha: "b".into(),
                repo: Some(PullRepo {
                    name: "forqen".into(),
                    full_name: base.into(),
                    clone_url: None,
                    ssh_url: None,
                }),
            },
            created_at: None,
            updated_at: None,
            html_url: None,
            comments: None,
            mergeable: None,
            additions: None,
            deletions: None,
            changed_files: None,
            labels: vec![],
        };

        assert!(!mk(Some("me/forqen"), "me/forqen").is_from_fork());
        assert!(mk(Some("someone/forqen"), "me/forqen").is_from_fork());
        assert!(
            mk(None, "me/forqen").is_from_fork(),
            "a deleted head repo was a fork; a same-repo branch would still exist"
        );
    }

    #[test]
    fn local_branch_is_namespaced_by_number() {
        let json = r#"{
            "number": 42,
            "title": "Fix it",
            "state": "open",
            "head": {"ref": "patch-1", "sha": "aaa"},
            "base": {"ref": "main", "sha": "bbb"}
        }"#;
        let pr: PullRequest = serde_json::from_str(json).unwrap();
        assert_eq!(
            pr.local_branch(),
            "pr/42",
            "two forks routinely both use `patch-1`; the number is what is unique"
        );
    }

    #[test]
    fn a_minimal_pull_request_payload_deserializes() {
        // The list endpoint omits mergeable, additions, deletions and
        // changed_files entirely — they must be optional, not defaulted to zero.
        let json = r#"{
            "number": 1,
            "title": "Hello",
            "state": "open",
            "draft": false,
            "head": {"ref": "topic", "sha": "aaa", "repo": null},
            "base": {"ref": "main", "sha": "bbb", "repo": null}
        }"#;
        let pr: PullRequest = serde_json::from_str(json).unwrap();
        assert_eq!(pr.number, 1);
        assert!(!pr.is_draft());
        assert_eq!(pr.mergeable, None);
        assert_eq!(pr.changed_files, None);
        assert!(pr.labels.is_empty(), "missing labels default to empty");
    }

    #[test]
    fn a_file_without_a_patch_is_still_valid() {
        // Binary files and over-large diffs arrive with no `patch` field.
        let f: PullFile = serde_json::from_str(
            r#"{"filename":"logo.png","status":"modified","additions":0,"deletions":0}"#,
        )
        .unwrap();
        assert_eq!(f.patch, None);
        assert_eq!(f.filename, "logo.png");
    }

    #[test]
    fn check_status_without_runs_defaults_to_empty() {
        let s: CheckStatus =
            serde_json::from_str(r#"{"state":"pending","total_count":0}"#).unwrap();
        assert_eq!(s.state, "pending");
        assert!(s.statuses.is_empty());
    }
}
