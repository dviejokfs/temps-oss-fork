//! Optional client linking a self-hosted Temps instance to a managed backend.
//!
//! # The rule this crate exists to keep
//!
//! **Local is primary. The managed backend is a mirror.** Nothing here may
//! block, slow, or fail the instance's own work. If the backend is down,
//! unreachable, unpaid or misconfigured, the instance keeps deploying, keeps
//! serving and keeps storing telemetry locally — it simply buffers what it
//! would have mirrored, and says so.
//!
//! Every operation therefore either succeeds, or degrades to a *reported*
//! state. There is no path where the instance is worse off than if it had
//! never connected.
//!
//! # What leaves the machine
//!
//! Only what is in [`temps_cloud_protocol`]: telemetry batches, heartbeats and
//! enrollment. No source, no environment variables, no secrets. An operator can
//! read the protocol crate and know exactly what is sent.

#![forbid(unsafe_code)]

pub mod flusher;
pub mod link;
pub mod spool;
pub mod state;
pub mod status;

pub use link::{CloudLink, FlushOutcome};
pub use state::EnrollmentState;
pub use status::{LinkStatus, MirrorHealth};

use std::time::Duration;

use temps_cloud_protocol::{EnrollRequest, EnrollResponse, IngestAck, SpanRecord, TelemetryBatch};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendUrl(url::Url);

impl BackendUrl {
    /// Parse a production managed-backend origin.
    ///
    /// The value comes from trusted host configuration, never an HTTP request.
    /// HTTPS is mandatory and credentials, query strings and fragments are
    /// rejected so bearer-token requests cannot be redirected or disguised.
    pub fn production(value: &str) -> Result<Self, CloudError> {
        Self::parse(value, false)
    }

    /// Explicit local-development escape hatch. Only loopback HTTP(S) origins
    /// are accepted; this must never become a general insecure-HTTP toggle.
    pub fn loopback_development(value: &str) -> Result<Self, CloudError> {
        Self::parse(value, true)
    }

    fn parse(value: &str, allow_loopback_http: bool) -> Result<Self, CloudError> {
        let parsed = url::Url::parse(value).map_err(|e| CloudError::InvalidBackendUrl {
            reason: e.to_string(),
        })?;

        if !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(CloudError::InvalidBackendUrl {
                reason: "credentials, query strings and fragments are not allowed".into(),
            });
        }
        if parsed.path() != "/" && !parsed.path().is_empty() {
            return Err(CloudError::InvalidBackendUrl {
                reason: "the backend URL must be an origin without a path".into(),
            });
        }

        let loopback = parsed
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case("localhost"))
            || matches!(parsed.host(), Some(url::Host::Ipv4(ip)) if ip.is_loopback())
            || matches!(parsed.host(), Some(url::Host::Ipv6(ip)) if ip.is_loopback());

        match parsed.scheme() {
            "https" => {}
            "http" if allow_loopback_http && loopback => {}
            "http" => {
                return Err(CloudError::InvalidBackendUrl {
                    reason: "HTTP is allowed only for an explicit loopback development backend"
                        .into(),
                })
            }
            other => {
                return Err(CloudError::InvalidBackendUrl {
                    reason: format!("unsupported scheme {other:?}; HTTPS is required"),
                })
            }
        }

        Ok(Self(parsed))
    }

    fn endpoint(&self, path: &str) -> url::Url {
        let mut endpoint = self.0.clone();
        endpoint.set_path(path);
        endpoint
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// How long any single call to the backend may take.
///
/// Deliberately short. This runs alongside the instance's own work, and a slow
/// backend must never become the instance's latency.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Error)]
pub enum CloudError {
    #[error("Invalid managed backend URL: {reason}")]
    InvalidBackendUrl { reason: String },

    #[error("Failed to configure the managed-backend HTTP client: {reason}")]
    ClientConfiguration { reason: String },

    #[error("Not linked to an account. Paste an enrollment code to connect one.")]
    NotEnrolled,

    #[error("Enrollment was refused: {detail}")]
    EnrollmentRefused { detail: String },

    #[error("Credential rejected by the backend — re-enroll this instance")]
    CredentialRejected,

    /// Transient. The caller keeps the batch spooled and tries again.
    #[error("Managed backend unreachable ({reason}); {spooled_bytes} bytes buffered locally")]
    Unreachable { reason: String, spooled_bytes: u64 },

    #[error("Backend rejected the payload: {detail}")]
    Rejected { detail: String },

    #[error("Backend acknowledgement did not match submission {submission_id}: {detail}")]
    InvalidAcknowledgement { submission_id: Uuid, detail: String },
}

impl CloudError {
    /// Whether retrying the same payload later could succeed.
    ///
    /// Drives the spool: retryable failures keep data, permanent ones must not
    /// buffer forever behind a problem no amount of waiting will fix.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            CloudError::Unreachable { .. }
                | CloudError::CredentialRejected
                | CloudError::InvalidAcknowledgement { .. }
        )
    }
}

pub struct CloudClient {
    http: reqwest::Client,
    backend: BackendUrl,
}

impl CloudClient {
    pub fn new(backend: BackendUrl) -> Result<Self, CloudError> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| CloudError::ClientConfiguration {
                reason: e.to_string(),
            })?;
        Ok(Self { http, backend })
    }

    /// Exchange an operator-pasted code for a long-lived instance token.
    pub async fn enroll(
        &self,
        code: &str,
        instance_id: Uuid,
        agent_version: &str,
    ) -> Result<EnrollResponse, CloudError> {
        let res = self
            .http
            .post(self.backend.endpoint("/v1/enroll"))
            .json(&EnrollRequest {
                enrollment_code: code.trim().to_uppercase(),
                instance_id,
                agent_version: agent_version.to_string(),
            })
            .send()
            .await
            .map_err(|e| CloudError::Unreachable {
                reason: e.to_string(),
                spooled_bytes: 0,
            })?;

        if res.status().is_success() {
            return res
                .json::<EnrollResponse>()
                .await
                .map_err(|e| CloudError::EnrollmentRefused {
                    detail: format!("unreadable response: {e}"),
                });
        }

        // Surface the backend's own wording — "this code has expired" is far
        // more useful to a lone operator than "enrollment failed".
        let detail = res
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| v["detail"].as_str().map(String::from))
            .unwrap_or_else(|| "no detail provided".into());
        Err(CloudError::EnrollmentRefused { detail })
    }

    /// Revoke an instance credential before removing the local copy.
    pub async fn revoke(&self, token: &str) -> Result<(), CloudError> {
        let res = self
            .http
            .post(self.backend.endpoint("/v1/revoke"))
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| CloudError::Unreachable {
                reason: e.to_string(),
                spooled_bytes: 0,
            })?;

        let status = res.status();
        if status.is_success() {
            return Ok(());
        }
        match status.as_u16() {
            401 | 403 => Err(CloudError::CredentialRejected),
            429 | 500..=599 => Err(CloudError::Unreachable {
                reason: format!("backend returned {status}"),
                spooled_bytes: 0,
            }),
            _ => {
                let detail = res
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|value| value["detail"].as_str().map(String::from))
                    .unwrap_or_else(|| format!("backend returned {status}"));
                Err(CloudError::Rejected { detail })
            }
        }
    }

    /// Mirror a batch of spans. Never called on a request path.
    pub async fn ship(
        &self,
        token: &str,
        submission_id: Uuid,
        spans: Vec<SpanRecord>,
    ) -> Result<IngestAck, CloudError> {
        let span_count = spans.len();
        let res = self
            .http
            .post(self.backend.endpoint("/v1/telemetry"))
            .bearer_auth(token)
            .json(&TelemetryBatch {
                submission_id,
                spans,
            })
            .send()
            .await
            .map_err(|e| CloudError::Unreachable {
                reason: e.to_string(),
                spooled_bytes: 0,
            })?;

        let status = res.status();
        if status.is_success() {
            let ack =
                res.json::<IngestAck>()
                    .await
                    .map_err(|e| CloudError::InvalidAcknowledgement {
                        submission_id,
                        detail: format!("unreadable ack: {e}"),
                    })?;
            if ack.submission_id != submission_id {
                return Err(CloudError::InvalidAcknowledgement {
                    submission_id,
                    detail: format!("response named submission {}", ack.submission_id),
                });
            }
            if ack.processed_spans != span_count {
                return Err(CloudError::InvalidAcknowledgement {
                    submission_id,
                    detail: format!("processed {} of {span_count} spans", ack.processed_spans),
                });
            }
            return Ok(ack);
        }

        match status.as_u16() {
            401 | 403 => Err(CloudError::CredentialRejected),
            // 5xx and 429 are the backend's problem, not the payload's: keep it.
            429 | 500..=599 => Err(CloudError::Unreachable {
                reason: format!("backend returned {status}"),
                spooled_bytes: 0,
            }),
            _ => {
                let detail = res
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|v| v["detail"].as_str().map(String::from))
                    .unwrap_or_else(|| format!("backend returned {status}"));
                Err(CloudError::Rejected { detail })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_transient_failures_are_retryable() {
        assert!(CloudError::Unreachable {
            reason: "timeout".into(),
            spooled_bytes: 0
        }
        .is_retryable());

        // These must NOT buffer forever: no amount of waiting fixes a revoked
        // credential or a payload the backend refuses.
        assert!(CloudError::CredentialRejected.is_retryable());
        assert!(!CloudError::NotEnrolled.is_retryable());
        assert!(!CloudError::Rejected {
            detail: "bad".into()
        }
        .is_retryable());
    }

    #[test]
    fn production_backends_require_a_clean_https_origin() {
        assert!(BackendUrl::production("https://cloud.test").is_ok());
        for invalid in [
            "http://cloud.test",
            "https://user@cloud.test",
            "https://cloud.test/path",
            "https://cloud.test?query=1",
            "https://cloud.test#fragment",
        ] {
            assert!(
                BackendUrl::production(invalid).is_err(),
                "accepted {invalid}"
            );
        }
    }

    #[test]
    fn development_http_is_restricted_to_loopback() {
        assert!(BackendUrl::loopback_development("http://127.0.0.1:1234").is_ok());
        assert!(BackendUrl::loopback_development("http://localhost:1234").is_ok());
        assert!(BackendUrl::loopback_development("http://192.168.1.2:1234").is_err());
    }

    #[test]
    fn errors_tell_the_operator_what_to_do() {
        // These strings are the entire support channel for a self-hosted user.
        assert!(CloudError::NotEnrolled
            .to_string()
            .contains("enrollment code"));
        assert!(CloudError::CredentialRejected
            .to_string()
            .contains("re-enroll"));
    }
}
