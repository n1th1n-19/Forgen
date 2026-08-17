//! GitHub Actions: workflow runs, their jobs, and logs.
//!
//! Logs are the awkward part. `/actions/jobs/{id}/logs` answers **302** with a
//! short-lived redirect to blob storage, and that redirect URL is
//! pre-authenticated — following it with an `Authorization` header attached
//! makes the storage backend reject the request. So logs are fetched with a
//! client that does not redirect, and the `Location` is then fetched bare.

use serde::{Deserialize, Serialize};

use crate::{Client, GhError, Response};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WorkflowRun {
    pub id: u64,
    pub name: Option<String>,
    /// `queued`, `in_progress`, `completed`.
    pub status: Option<String>,
    /// Only meaningful once `status` is `completed`: `success`, `failure`,
    /// `cancelled`, `skipped`, `timed_out`, `action_required`.
    pub conclusion: Option<String>,
    pub head_branch: Option<String>,
    pub head_sha: Option<String>,
    pub event: Option<String>,
    pub created_at: Option<String>,
    pub html_url: Option<String>,
    pub run_number: Option<u64>,
}

impl WorkflowRun {
    pub fn is_running(&self) -> bool {
        matches!(self.status.as_deref(), Some("queued") | Some("in_progress"))
    }

    /// One word for the state, collapsing status and conclusion.
    ///
    /// They cannot be read independently: a run that is still going has a
    /// `conclusion` of null, and reading that as "no failure" would show a
    /// green tick on a job that has not finished.
    pub fn outcome(&self) -> &str {
        match (self.status.as_deref(), self.conclusion.as_deref()) {
            (Some("completed"), Some(c)) => c,
            (Some("completed"), None) => "unknown",
            (Some(s), _) => s,
            (None, _) => "unknown",
        }
    }

    pub fn failed(&self) -> bool {
        matches!(self.outcome(), "failure" | "timed_out" | "action_required")
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Job {
    pub id: u64,
    pub name: String,
    pub status: Option<String>,
    pub conclusion: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    #[serde(default)]
    pub steps: Vec<Step>,
}

impl Job {
    pub fn outcome(&self) -> &str {
        match (self.status.as_deref(), self.conclusion.as_deref()) {
            (Some("completed"), Some(c)) => c,
            (Some(s), _) => s,
            _ => "unknown",
        }
    }

    pub fn failed(&self) -> bool {
        matches!(self.outcome(), "failure" | "timed_out")
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Step {
    pub name: String,
    pub status: Option<String>,
    pub conclusion: Option<String>,
    pub number: Option<u32>,
}

impl Step {
    pub fn failed(&self) -> bool {
        matches!(
            self.conclusion.as_deref(),
            Some("failure") | Some("timed_out")
        )
    }
}

#[derive(Deserialize)]
struct RunsEnvelope {
    #[serde(default = "Vec::new")]
    workflow_runs: Vec<WorkflowRun>,
}

#[derive(Deserialize)]
struct JobsEnvelope {
    #[serde(default = "Vec::new")]
    jobs: Vec<Job>,
}

impl Client {
    /// Recent workflow runs, newest first.
    pub async fn workflow_runs(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<Response<Vec<WorkflowRun>>, GhError> {
        let Response { data, provenance } = self
            .get::<RunsEnvelope>(&format!("/repos/{owner}/{repo}/actions/runs?per_page=30"))
            .await?;
        Ok(Response {
            data: data.workflow_runs,
            provenance,
        })
    }

    pub async fn run_jobs(
        &self,
        owner: &str,
        repo: &str,
        run_id: u64,
    ) -> Result<Response<Vec<Job>>, GhError> {
        let Response { data, provenance } = self
            .get::<JobsEnvelope>(&format!(
                "/repos/{owner}/{repo}/actions/runs/{run_id}/jobs?per_page=100"
            ))
            .await?;
        Ok(Response {
            data: data.jobs,
            provenance,
        })
    }

    /// Plain-text log for one job.
    ///
    /// Two requests by necessity. The API answers 302 with a pre-authenticated
    /// blob-storage URL; sending our `Authorization` header along to that URL
    /// makes the storage backend reject it, so the redirect is not followed
    /// automatically and the location is fetched with no credentials.
    pub async fn job_log(&self, owner: &str, repo: &str, job_id: u64) -> Result<String, GhError> {
        let url = format!(
            "{}/repos/{owner}/{repo}/actions/jobs/{job_id}/logs",
            self.api_base_pub()
        );

        let no_redirect = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;

        let resp = no_redirect
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", concat!("forqen/", env!("CARGO_PKG_VERSION")))
            .bearer_auth(self.token_pub().expose())
            .send()
            .await?;

        let status = resp.status().as_u16();
        if status == 404 {
            // Logs expire, and a job that never started has none.
            return Err(GhError::Api {
                status,
                message: "no logs available for this job — they may have expired".into(),
            });
        }

        // A 200 means the body is the log itself, which some deployments do.
        if status == 200 {
            return resp.text().await.map_err(GhError::Network);
        }

        let Some(location) = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
        else {
            return Err(GhError::Api {
                status,
                message: "log redirect had no Location header".into(),
            });
        };

        // Bare request: the redirect URL carries its own signature, and an
        // Authorization header alongside it is what makes storage refuse.
        let log = reqwest::Client::new()
            .get(&location)
            .send()
            .await?
            .text()
            .await?;
        Ok(log)
    }

    /// Re-run only the jobs that failed.
    pub async fn rerun_failed_jobs(
        &self,
        owner: &str,
        repo: &str,
        run_id: u64,
    ) -> Result<(), GhError> {
        self.post_no_content(
            &format!("/repos/{owner}/{repo}/actions/runs/{run_id}/rerun-failed-jobs"),
            serde_json::Value::Null,
        )
        .await
    }

    pub async fn cancel_run(&self, owner: &str, repo: &str, run_id: u64) -> Result<(), GhError> {
        self.post_no_content(
            &format!("/repos/{owner}/{repo}/actions/runs/{run_id}/cancel"),
            serde_json::Value::Null,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(status: &str, conclusion: Option<&str>) -> WorkflowRun {
        WorkflowRun {
            id: 1,
            name: Some("CI".into()),
            status: Some(status.into()),
            conclusion: conclusion.map(str::to_string),
            head_branch: Some("main".into()),
            head_sha: Some("abc".into()),
            event: Some("push".into()),
            created_at: None,
            html_url: None,
            run_number: Some(7),
        }
    }

    #[test]
    fn a_running_job_is_not_reported_as_successful() {
        // conclusion is null while a run is in flight; reading it alone would
        // put a green tick on a job that has not finished.
        let r = run("in_progress", None);
        assert!(r.is_running());
        assert_eq!(r.outcome(), "in_progress");
        assert!(!r.failed());

        let q = run("queued", None);
        assert!(q.is_running());
        assert_eq!(q.outcome(), "queued");
    }

    #[test]
    fn completed_runs_report_their_conclusion() {
        assert_eq!(run("completed", Some("success")).outcome(), "success");
        assert_eq!(run("completed", Some("failure")).outcome(), "failure");
        assert_eq!(run("completed", Some("cancelled")).outcome(), "cancelled");
        assert!(!run("completed", Some("success")).is_running());
    }

    #[test]
    fn failure_covers_the_states_that_need_attention() {
        for c in ["failure", "timed_out", "action_required"] {
            assert!(run("completed", Some(c)).failed(), "{c} needs attention");
        }
        for c in ["success", "skipped", "cancelled"] {
            assert!(!run("completed", Some(c)).failed(), "{c} does not");
        }
    }

    #[test]
    fn a_completed_run_with_no_conclusion_is_unknown_not_success() {
        assert_eq!(run("completed", None).outcome(), "unknown");
        assert!(!run("completed", None).failed());
    }

    #[test]
    fn runs_unwrap_from_their_envelope() {
        // The endpoint nests the array under workflow_runs rather than
        // returning it directly, unlike most list endpoints.
        let e: RunsEnvelope = serde_json::from_str(
            r#"{"total_count":1,"workflow_runs":[
                 {"id":31972523763,"name":"CI","status":"completed",
                  "conclusion":"failure","head_branch":"main","run_number":5}]}"#,
        )
        .unwrap();
        assert_eq!(e.workflow_runs.len(), 1);
        assert!(e.workflow_runs[0].failed());
        assert_eq!(e.workflow_runs[0].run_number, Some(5));
    }

    #[test]
    fn an_empty_envelope_yields_no_runs() {
        let e: RunsEnvelope = serde_json::from_str(r#"{"total_count":0}"#).unwrap();
        assert!(e.workflow_runs.is_empty());
    }

    #[test]
    fn jobs_carry_their_steps_and_report_the_failing_one() {
        let e: JobsEnvelope = serde_json::from_str(
            r#"{"total_count":1,"jobs":[{
                 "id":95227041279,"name":"engine","status":"completed",
                 "conclusion":"failure","steps":[
                   {"name":"Format","status":"completed","conclusion":"success","number":1},
                   {"name":"Clippy","status":"completed","conclusion":"failure","number":2},
                   {"name":"Test","status":"completed","conclusion":"skipped","number":3}]}]}"#,
        )
        .unwrap();

        let job = &e.jobs[0];
        assert!(job.failed());
        assert_eq!(job.outcome(), "failure");

        let failing: Vec<&str> = job
            .steps
            .iter()
            .filter(|s| s.failed())
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(failing, ["Clippy"], "the failing step is what to jump to");
    }

    #[test]
    fn a_job_with_no_steps_still_parses() {
        let e: JobsEnvelope =
            serde_json::from_str(r#"{"jobs":[{"id":1,"name":"j","status":"queued"}]}"#).unwrap();
        assert!(e.jobs[0].steps.is_empty());
        assert_eq!(e.jobs[0].outcome(), "queued");
    }
}
