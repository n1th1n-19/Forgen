//! Response types.
//!
//! Deliberately partial: only fields forqen renders are declared. Serde ignores
//! the rest, which means a new field in a GitHub response is a no-op here
//! rather than a deserialization failure — and the cached bodies stay small
//! because we never re-serialize what we did not ask for.

use serde::{Deserialize, Serialize};

use crate::{Client, GhError, Response};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct User {
    pub login: String,
    pub id: u64,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Repository {
    pub id: u64,
    pub name: String,
    pub full_name: String,
    pub private: bool,
    pub fork: bool,
    pub description: Option<String>,
    pub default_branch: Option<String>,
    /// HTTPS clone URL. SSH is in `ssh_url`; which one the clone dialog offers
    /// depends on whether an ssh-agent is reachable.
    pub clone_url: Option<String>,
    pub ssh_url: Option<String>,
    pub updated_at: Option<String>,
    pub stargazers_count: Option<u32>,
    pub language: Option<String>,
}

impl Client {
    /// The authenticated user. Also the cheapest way to validate a pasted PAT.
    pub async fn current_user(&self) -> Result<Response<User>, GhError> {
        self.get("/user").await
    }

    /// Repositories the user can push to, most recently updated first.
    ///
    /// `affiliation` excludes repos the user merely stars or watches, which
    /// otherwise flood the clone dialog. 100 per page is GitHub's maximum, so
    /// this is one request for most accounts.
    pub async fn my_repos(&self, page: u32) -> Result<Response<Vec<Repository>>, GhError> {
        self.get(&format!(
            "/user/repos?per_page=100&page={page}&sort=updated&affiliation=owner,collaborator,organization_member"
        ))
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_ignores_unknown_fields() {
        // A trimmed real payload plus a field we do not model.
        let json = r#"{
            "id": 1296269,
            "name": "forqen",
            "full_name": "n1th1n-19/forqen",
            "private": false,
            "fork": false,
            "description": null,
            "default_branch": "main",
            "clone_url": "https://github.com/n1th1n-19/forqen.git",
            "ssh_url": "git@github.com:n1th1n-19/forqen.git",
            "some_field_github_added_last_tuesday": {"nested": true}
        }"#;
        let r: Repository = serde_json::from_str(json).unwrap();
        assert_eq!(r.full_name, "n1th1n-19/forqen");
        assert_eq!(r.default_branch.as_deref(), Some("main"));
        assert_eq!(r.description, None);
        // Not present in the payload at all — must default to None, not fail.
        assert_eq!(r.stargazers_count, None);
    }

    #[test]
    fn user_round_trips_through_the_cache_encoding() {
        let u = User {
            login: "n1th1n-19".into(),
            id: 42,
            name: Some("Walter".into()),
            avatar_url: None,
        };
        // Bodies are cached as raw bytes and decoded on the way out, so the
        // types must survive that trip unchanged.
        let bytes = serde_json::to_vec(&u).unwrap();
        let back: User = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(u, back);
    }
}
