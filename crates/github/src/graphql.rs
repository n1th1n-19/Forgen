//! Minimal GraphQL transport.
//!
//! Deliberately not a generated client. `cynic` and `graphql_client` bring a
//! build-time schema download and a proc-macro layer; forqen sends a handful of
//! hand-written queries, and for that the machinery costs more than it saves.
//! Responses land in the same `serde` structs everything else uses.
//!
//! GraphQL is used only where REST is genuinely worse — review threads, which
//! REST returns flat with `in_reply_to_id` and expects the client to rebuild.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::{Client, GhError};

#[derive(Serialize)]
struct Request<'a> {
    query: &'a str,
    variables: serde_json::Value,
}

#[derive(Deserialize)]
struct Envelope<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

#[derive(Deserialize, Debug)]
struct GraphQlError {
    message: String,
    #[serde(default)]
    #[serde(rename = "type")]
    kind: Option<String>,
}

impl Client {
    /// POST a GraphQL query and decode `data` into `T`.
    ///
    /// Not routed through the ETag cache. GraphQL is a POST to a single
    /// endpoint, so there is no per-resource URL to key a cache on, and GitHub
    /// does not send validators for it. The REST paths carry the caching.
    pub async fn graphql<T: DeserializeOwned>(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<T, GhError> {
        let url = self.graphql_url();

        let resp = self
            .http()
            .post(&url)
            .header("Accept", "application/vnd.github+json")
            .bearer_auth(self.token().expose())
            .json(&Request { query, variables })
            .send()
            .await
            .map_err(GhError::Network)?;

        self.record_limits_pub(resp.headers());

        let status = resp.status().as_u16();
        if status == 401 {
            return Err(GhError::Unauthorized);
        }

        let body = resp.bytes().await.map_err(GhError::Network)?;
        let envelope: Envelope<T> =
            serde_json::from_slice(&body).map_err(|e| GhError::Decode(e.to_string()))?;

        // A GraphQL error is reported with HTTP 200 and an `errors` array, so
        // the status alone never indicates failure. Partial data can accompany
        // errors; surfacing the error is the safer reading, because acting on
        // half a response is worse than retrying.
        if !envelope.errors.is_empty() {
            let message = envelope
                .errors
                .iter()
                .map(|e| match &e.kind {
                    Some(k) => format!("{k}: {}", e.message),
                    None => e.message.clone(),
                })
                .collect::<Vec<_>>()
                .join("; ");
            return Err(GhError::Api { status, message });
        }

        envelope
            .data
            .ok_or_else(|| GhError::Decode("response contained neither data nor errors".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize, Debug, PartialEq, Eq)]
    struct Probe {
        value: String,
    }

    fn decode(body: &str) -> Result<Probe, String> {
        let envelope: Envelope<Probe> =
            serde_json::from_str(body).map_err(|e| format!("decode: {e}"))?;
        if !envelope.errors.is_empty() {
            return Err(envelope
                .errors
                .iter()
                .map(|e| e.message.clone())
                .collect::<Vec<_>>()
                .join("; "));
        }
        envelope.data.ok_or_else(|| "no data".to_string())
    }

    #[test]
    fn decodes_a_successful_response() {
        assert_eq!(
            decode(r#"{"data":{"value":"hello"}}"#).unwrap(),
            Probe {
                value: "hello".into()
            }
        );
    }

    #[test]
    fn an_errors_array_is_a_failure_even_with_http_200() {
        let err = decode(
            r#"{"data":null,"errors":[{"type":"NOT_FOUND","message":"Could not resolve to a Repository"}]}"#,
        )
        .unwrap_err();
        assert!(err.contains("Could not resolve"), "{err}");
    }

    #[test]
    fn errors_win_over_partial_data() {
        // GraphQL can return both. Acting on half a response is worse than
        // reporting the failure and retrying.
        let err = decode(r#"{"data":{"value":"partial"},"errors":[{"message":"rate limited"}]}"#)
            .unwrap_err();
        assert_eq!(err, "rate limited");
    }

    #[test]
    fn several_errors_are_all_reported() {
        let err = decode(r#"{"errors":[{"message":"first"},{"message":"second"}]}"#).unwrap_err();
        assert!(err.contains("first") && err.contains("second"), "{err}");
    }

    #[test]
    fn a_response_with_neither_data_nor_errors_is_an_error() {
        assert_eq!(decode("{}").unwrap_err(), "no data");
    }
}
