//! Pull request review: threads, comments, submitting a review, merging.
//!
//! Threads come from GraphQL because REST cannot express them. The REST
//! endpoint returns every review comment in one flat list, each reply carrying
//! an `in_reply_to_id`, and leaves the client to rebuild the conversation — a
//! reconstruction that has no answer when a parent has been deleted, and that
//! cannot tell you whether a thread is resolved or has gone outdated because
//! the line it was anchored to has since changed. GraphQL returns all three
//! directly.
//!
//! Writes go back through REST where REST is adequate: submitting a review and
//! merging are single calls with clear semantics either way, and the REST
//! versions need no node-id lookup.

use serde::{Deserialize, Serialize};

use crate::{Client, GhError};

/// One conversation anchored to a line of a diff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewThread {
    pub id: String,
    pub path: String,
    /// Line in the diff this thread is anchored to. `None` once the thread is
    /// outdated — the anchor no longer exists in the current diff.
    pub line: Option<u32>,
    pub is_resolved: bool,
    /// The anchored line has changed since the comment was written, so the
    /// thread no longer applies to the diff as it stands.
    pub is_outdated: bool,
    pub comments: Vec<ReviewComment>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewComment {
    pub id: String,
    pub author: String,
    pub body: String,
    pub created_at: String,
    /// The diff fragment GitHub stores with the comment, so a thread can be
    /// shown in context even when it has gone outdated.
    pub diff_hunk: Option<String>,
}

/// What a submitted review says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewVerdict {
    Comment,
    Approve,
    RequestChanges,
}

impl ReviewVerdict {
    fn as_event(self) -> &'static str {
        match self {
            Self::Comment => "COMMENT",
            Self::Approve => "APPROVE",
            Self::RequestChanges => "REQUEST_CHANGES",
        }
    }
}

/// Which side of a split diff a comment is anchored to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffSide {
    /// A removed or unchanged line, addressed in the pre-image.
    Left,
    /// An added or unchanged line, addressed in the post-image.
    Right,
}

impl DiffSide {
    fn as_str(self) -> &'static str {
        match self {
            Self::Left => "LEFT",
            Self::Right => "RIGHT",
        }
    }
}

/// A comment to attach to a review.
#[derive(Clone, Debug, Serialize)]
pub struct DraftComment {
    pub path: String,
    /// Line number in the file, on `side`.
    pub line: u32,
    pub side: &'static str,
    pub body: String,
}

impl DraftComment {
    pub fn new(
        path: impl Into<String>,
        line: u32,
        side: DiffSide,
        body: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            line,
            side: side.as_str(),
            body: body.into(),
        }
    }
}

const THREADS_QUERY: &str = r#"
query($owner:String!, $repo:String!, $number:Int!, $cursor:String) {
  repository(owner:$owner, name:$repo) {
    pullRequest(number:$number) {
      reviewThreads(first:50, after:$cursor) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id
          path
          line
          isResolved
          isOutdated
          comments(first:50) {
            nodes {
              id
              body
              createdAt
              diffHunk
              author { login }
            }
          }
        }
      }
    }
  }
}
"#;

// --- wire types, kept private so the shape of the query cannot leak out ------

#[derive(Deserialize)]
struct ThreadsData {
    repository: Option<RepoNode>,
}

#[derive(Deserialize)]
struct RepoNode {
    #[serde(rename = "pullRequest")]
    pull_request: Option<PrNode>,
}

#[derive(Deserialize)]
struct PrNode {
    #[serde(rename = "reviewThreads")]
    review_threads: Connection<ThreadNode>,
}

#[derive(Deserialize)]
struct Connection<T> {
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
    #[serde(default = "Vec::new")]
    nodes: Vec<T>,
}

#[derive(Deserialize)]
struct PageInfo {
    #[serde(rename = "hasNextPage")]
    has_next_page: bool,
    #[serde(rename = "endCursor")]
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
struct ThreadNode {
    id: String,
    path: String,
    line: Option<u32>,
    #[serde(rename = "isResolved")]
    is_resolved: bool,
    #[serde(rename = "isOutdated")]
    is_outdated: bool,
    comments: CommentConnection,
}

#[derive(Deserialize)]
struct CommentConnection {
    #[serde(default = "Vec::new")]
    nodes: Vec<CommentNode>,
}

#[derive(Deserialize)]
struct CommentNode {
    id: String,
    body: String,
    #[serde(rename = "createdAt")]
    created_at: String,
    #[serde(rename = "diffHunk")]
    diff_hunk: Option<String>,
    author: Option<AuthorNode>,
}

#[derive(Deserialize)]
struct AuthorNode {
    login: String,
}

impl From<ThreadNode> for ReviewThread {
    fn from(n: ThreadNode) -> Self {
        Self {
            id: n.id,
            path: n.path,
            line: n.line,
            is_resolved: n.is_resolved,
            is_outdated: n.is_outdated,
            comments: n
                .comments
                .nodes
                .into_iter()
                .map(|c| ReviewComment {
                    id: c.id,
                    // A deleted account leaves author null. "ghost" is what
                    // GitHub's own UI shows, and an empty name reads as a bug.
                    author: c.author.map(|a| a.login).unwrap_or_else(|| "ghost".into()),
                    body: c.body,
                    created_at: c.created_at,
                    diff_hunk: c.diff_hunk,
                })
                .collect(),
        }
    }
}

impl Client {
    /// Every review thread on a pull request, following pagination.
    pub async fn review_threads(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Vec<ReviewThread>, GhError> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;

        loop {
            let data: ThreadsData = self
                .graphql(
                    THREADS_QUERY,
                    serde_json::json!({
                        "owner": owner,
                        "repo": repo,
                        "number": number,
                        "cursor": cursor,
                    }),
                )
                .await?;

            let Some(pr) = data.repository.and_then(|r| r.pull_request) else {
                // The repository or pull request is gone, or the token cannot
                // see it. Either way there are no threads, which is not an
                // error worth failing a review over.
                break;
            };

            let conn = pr.review_threads;
            out.extend(conn.nodes.into_iter().map(ReviewThread::from));

            match (conn.page_info.has_next_page, conn.page_info.end_cursor) {
                (true, Some(next)) => cursor = Some(next),
                // `hasNextPage` without a cursor would loop forever.
                _ => break,
            }
        }

        Ok(out)
    }

    /// Submit a review, optionally with inline comments.
    ///
    /// One call rather than creating a pending review and adding comments to
    /// it: a partially-built pending review left behind by a crash is invisible
    /// in most clients and blocks starting another.
    pub async fn submit_review(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        verdict: ReviewVerdict,
        body: &str,
        comments: &[DraftComment],
    ) -> Result<(), GhError> {
        // GitHub rejects an APPROVE or REQUEST_CHANGES with an empty body far
        // less clearly than this does.
        if verdict == ReviewVerdict::RequestChanges && body.trim().is_empty() {
            return Err(GhError::Api {
                status: 422,
                message: "requesting changes needs a message explaining what to change".into(),
            });
        }

        let mut payload = serde_json::json!({
            "event": verdict.as_event(),
            "body": body,
        });
        if !comments.is_empty() {
            payload["comments"] =
                serde_json::to_value(comments).map_err(|e| GhError::Decode(e.to_string()))?;
        }

        self.post_no_content(
            &format!("/repos/{owner}/{repo}/pulls/{number}/reviews"),
            payload,
        )
        .await
    }

    /// Reply to an existing thread.
    pub async fn reply_to_thread(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        comment_id: u64,
        body: &str,
    ) -> Result<(), GhError> {
        self.post_no_content(
            &format!("/repos/{owner}/{repo}/pulls/{number}/comments/{comment_id}/replies"),
            serde_json::json!({ "body": body }),
        )
        .await
    }

    /// How to combine a pull request's commits.
    pub async fn merge_pull(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        method: MergeMethod,
        title: Option<&str>,
    ) -> Result<(), GhError> {
        let mut payload = serde_json::json!({ "merge_method": method.as_str() });
        if let Some(t) = title {
            payload["commit_title"] = serde_json::Value::String(t.to_string());
        }
        self.put_no_content(
            &format!("/repos/{owner}/{repo}/pulls/{number}/merge"),
            payload,
        )
        .await
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

impl MergeMethod {
    fn as_str(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Squash => "squash",
            Self::Rebase => "rebase",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Vec<ReviewThread> {
        let data: ThreadsData = serde_json::from_str(json).unwrap();
        data.repository
            .and_then(|r| r.pull_request)
            .map(|p| {
                p.review_threads
                    .nodes
                    .into_iter()
                    .map(ReviewThread::from)
                    .collect()
            })
            .unwrap_or_default()
    }

    const SAMPLE: &str = r#"{
      "repository": { "pullRequest": { "reviewThreads": {
        "pageInfo": { "hasNextPage": false, "endCursor": null },
        "nodes": [
          {
            "id": "T_1", "path": "src/main.rs", "line": 42,
            "isResolved": false, "isOutdated": false,
            "comments": { "nodes": [
              {"id":"C_1","body":"this leaks","createdAt":"2026-01-01T00:00:00Z",
               "diffHunk":"@@ -1 +1 @@","author":{"login":"reviewer"}},
              {"id":"C_2","body":"good catch","createdAt":"2026-01-01T01:00:00Z",
               "diffHunk":"@@ -1 +1 @@","author":{"login":"author"}}
            ]}
          }
        ]
      }}}
    }"#;

    #[test]
    fn threads_keep_their_replies_in_order() {
        let threads = parse(SAMPLE);
        assert_eq!(threads.len(), 1);

        let t = &threads[0];
        assert_eq!(t.path, "src/main.rs");
        assert_eq!(t.line, Some(42));
        assert!(!t.is_resolved && !t.is_outdated);
        assert_eq!(t.comments.len(), 2);
        assert_eq!(t.comments[0].author, "reviewer");
        assert_eq!(t.comments[1].body, "good catch");
    }

    #[test]
    fn an_outdated_thread_has_no_line() {
        // GitHub nulls `line` once the anchor no longer exists in the diff.
        let json = r#"{"repository":{"pullRequest":{"reviewThreads":{
          "pageInfo":{"hasNextPage":false,"endCursor":null},
          "nodes":[{"id":"T_2","path":"a.rs","line":null,
            "isResolved":false,"isOutdated":true,
            "comments":{"nodes":[{"id":"C_9","body":"stale","createdAt":"x",
              "diffHunk":"@@ -1 +1 @@","author":{"login":"r"}}]}}]
        }}}}"#;
        let t = &parse(json)[0];
        assert!(t.is_outdated);
        assert_eq!(t.line, None);
        assert!(
            t.comments[0].diff_hunk.is_some(),
            "the stored hunk is the only context an outdated thread has left"
        );
    }

    #[test]
    fn a_deleted_author_becomes_ghost() {
        let json = r#"{"repository":{"pullRequest":{"reviewThreads":{
          "pageInfo":{"hasNextPage":false,"endCursor":null},
          "nodes":[{"id":"T_3","path":"a.rs","line":1,
            "isResolved":true,"isOutdated":false,
            "comments":{"nodes":[{"id":"C_3","body":"hi","createdAt":"x",
              "diffHunk":null,"author":null}]}}]
        }}}}"#;
        let t = &parse(json)[0];
        assert_eq!(
            t.comments[0].author, "ghost",
            "an empty author name reads as a rendering bug"
        );
        assert!(t.is_resolved);
    }

    #[test]
    fn a_missing_pull_request_yields_no_threads_rather_than_failing() {
        assert!(parse(r#"{"repository":{"pullRequest":null}}"#).is_empty());
        assert!(parse(r#"{"repository":null}"#).is_empty());
    }

    #[test]
    fn an_empty_thread_list_parses() {
        let json = r#"{"repository":{"pullRequest":{"reviewThreads":{
          "pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[]}}}}"#;
        assert!(parse(json).is_empty());
    }

    #[test]
    fn verdicts_and_merge_methods_map_to_the_api_strings() {
        assert_eq!(ReviewVerdict::Approve.as_event(), "APPROVE");
        assert_eq!(ReviewVerdict::RequestChanges.as_event(), "REQUEST_CHANGES");
        assert_eq!(ReviewVerdict::Comment.as_event(), "COMMENT");
        assert_eq!(MergeMethod::Squash.as_str(), "squash");
        assert_eq!(MergeMethod::Rebase.as_str(), "rebase");
        assert_eq!(MergeMethod::Merge.as_str(), "merge");
    }

    #[test]
    fn draft_comments_serialize_with_the_side_the_api_expects() {
        let c = DraftComment::new("src/a.rs", 12, DiffSide::Right, "nit");
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["path"], "src/a.rs");
        assert_eq!(v["line"], 12);
        assert_eq!(v["side"], "RIGHT");
        assert_eq!(v["body"], "nit");

        let left = DraftComment::new("src/a.rs", 3, DiffSide::Left, "was wrong");
        assert_eq!(serde_json::to_value(&left).unwrap()["side"], "LEFT");
    }
}
