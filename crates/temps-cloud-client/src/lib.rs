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

use std::{path::Path, time::Duration};

use sha2::{Digest, Sha256};
use temps_cloud_protocol::{
    BackupArtifact, BackupCompleted, BackupTarget, BackupTargetRequest, EnrollRequest,
    EnrollResponse, IngestAck, ManagedAiAnalysisRequest, ManagedAiAnalysisResponse,
    ManagedAiCapability, ManagedAiChatRequest, ManagedAiChatResponse, ManagedNotificationAccepted,
    ManagedNotificationRequest, NativeSnapshot, NativeSnapshotRequest, SpanRecord, TelemetryBatch,
    WalGObjectCompleted, WalGObjectTarget, WalGObjectTargetRequest, WalGSnapshot,
    WalGSnapshotCompleted, WalGSnapshotRequest,
};
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
const AI_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

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

    /// Describe managed inference without reading or uploading local context.
    pub async fn managed_ai_capability(
        &self,
        token: &str,
    ) -> Result<ManagedAiCapability, CloudError> {
        let response = self
            .http
            .get(self.backend.endpoint("/v1/ai/capability"))
            .bearer_auth(token)
            .send()
            .await
            .map_err(|error| CloudError::Unreachable {
                reason: error.to_string(),
                spooled_bytes: 0,
            })?;
        decode_managed_response(response).await
    }

    /// Submit the exact manifest the operator approved in the OSS AI surface.
    /// Source telemetry is never fetched by Cloud and BYO credentials never use
    /// this path; local BYO continues through the OSS AI gateway directly.
    pub async fn managed_ai_analysis(
        &self,
        token: &str,
        request: &ManagedAiAnalysisRequest,
    ) -> Result<ManagedAiAnalysisResponse, CloudError> {
        let response = self
            .http
            .post(self.backend.endpoint("/v1/ai/analyses"))
            .bearer_auth(token)
            .timeout(AI_REQUEST_TIMEOUT)
            .json(request)
            .send()
            .await
            .map_err(|error| CloudError::Unreachable {
                reason: error.to_string(),
                spooled_bytes: 0,
            })?;
        decode_managed_response(response).await
    }

    /// Proxy one OpenAI-compatible completion. Retries reuse the request id so
    /// Cloud and the upstream provider cannot reserve or charge twice when a
    /// response is lost after either side has committed it.
    pub async fn managed_ai_chat(
        &self,
        token: &str,
        request: &ManagedAiChatRequest,
    ) -> Result<ManagedAiChatResponse, CloudError> {
        let mut last_failure = None;
        for attempt in 0..3 {
            match self
                .http
                .post(self.backend.endpoint("/v1/ai/chat/completions"))
                .bearer_auth(token)
                .timeout(AI_REQUEST_TIMEOUT)
                .json(request)
                .send()
                .await
            {
                Ok(response) if !matches!(response.status().as_u16(), 429 | 500..=599) => {
                    return decode_managed_response(response).await;
                }
                Ok(response) => {
                    last_failure = Some(format!("managed backend returned {}", response.status()));
                }
                Err(error) => last_failure = Some(error.without_url().to_string()),
            }
            if attempt < 2 {
                tokio::time::sleep(Duration::from_millis(100 * (1 << attempt))).await;
            }
        }
        Err(CloudError::Unreachable {
            reason: last_failure.unwrap_or_else(|| "managed backend returned no response".into()),
            spooled_bytes: 0,
        })
    }

    /// Stream one completed local backup directly to Cloud-owned object
    /// storage. Every retry keeps the same client-generated backup id; target
    /// creation, PUT and completion are therefore safe when a response is lost.
    pub async fn upload_backup_file(
        &self,
        token: &str,
        instance_id: Uuid,
        backup_id: Uuid,
        source: String,
        artifact: BackupArtifact,
        path: &Path,
    ) -> Result<BackupTarget, CloudError> {
        let (bytes, checksum_sha256) = inspect_local_backup(path).await?;
        let request = BackupTargetRequest {
            backup_id,
            instance_id,
            source,
            estimated_bytes: bytes,
            checksum_sha256: checksum_sha256.clone(),
            artifact,
        };
        let target = self.backup_target(token, &request).await?;

        if !target.upload_required {
            return Ok(target);
        }

        let mut last_failure = None;
        for attempt in 0..3 {
            let file = tokio::fs::File::open(path)
                .await
                .map_err(|error| CloudError::Rejected {
                    detail: format!("could not reopen backup artifact for upload: {error}"),
                })?;
            let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file));
            let mut upload = self.http.put(&target.upload_url).body(body);
            for (name, value) in &target.headers {
                upload = upload.header(name, value);
            }
            match upload.send().await {
                Ok(response) if response.status().is_success() => {
                    last_failure = None;
                    break;
                }
                Ok(response)
                    if matches!(response.status().as_u16(), 408 | 425 | 429 | 500..=599) =>
                {
                    last_failure = Some(format!("object storage returned {}", response.status()));
                }
                Ok(response) => {
                    return Err(CloudError::Rejected {
                        detail: format!(
                            "object storage rejected the backup with {}",
                            response.status()
                        ),
                    });
                }
                Err(error) => last_failure = Some(error.without_url().to_string()),
            }
            if attempt < 2 {
                tokio::time::sleep(Duration::from_millis(100 * (1 << attempt))).await;
            }
        }
        if let Some(reason) = last_failure {
            return Err(CloudError::Unreachable {
                reason: format!("backup upload did not complete after bounded retries: {reason}"),
                spooled_bytes: bytes,
            });
        }

        self.complete_backup(
            token,
            &BackupCompleted {
                backup_id: target.backup_id,
                bytes,
                checksum_sha256,
            },
        )
        .await?;
        Ok(target)
    }

    async fn backup_target(
        &self,
        token: &str,
        request: &BackupTargetRequest,
    ) -> Result<BackupTarget, CloudError> {
        self.retry_backup_json("/v1/backups/target", token, request)
            .await
    }

    pub async fn declare_walg_snapshot(
        &self,
        token: &str,
        request: &WalGSnapshotRequest,
    ) -> Result<WalGSnapshot, CloudError> {
        self.retry_backup_json("/v1/backups/walg/snapshots", token, request)
            .await
    }

    pub async fn declare_native_snapshot(
        &self,
        token: &str,
        request: &NativeSnapshotRequest,
    ) -> Result<NativeSnapshot, CloudError> {
        self.retry_backup_json("/v1/backups/native/snapshots", token, request)
            .await
    }

    pub async fn native_object_target(
        &self,
        token: &str,
        request: &WalGObjectTargetRequest,
    ) -> Result<WalGObjectTarget, CloudError> {
        self.retry_backup_json("/v1/backups/native/objects/target", token, request)
            .await
    }

    pub async fn complete_native_object(
        &self,
        token: &str,
        completion: &WalGObjectCompleted,
    ) -> Result<(), CloudError> {
        let _: serde_json::Value = self
            .retry_backup_json("/v1/backups/native/objects/complete", token, completion)
            .await?;
        Ok(())
    }

    pub async fn complete_native_snapshot(
        &self,
        token: &str,
        completion: &WalGSnapshotCompleted,
    ) -> Result<(), CloudError> {
        let _: serde_json::Value = self
            .retry_backup_json("/v1/backups/native/snapshots/complete", token, completion)
            .await?;
        Ok(())
    }

    pub async fn walg_object_target(
        &self,
        token: &str,
        request: &WalGObjectTargetRequest,
    ) -> Result<WalGObjectTarget, CloudError> {
        self.retry_backup_json("/v1/backups/walg/objects/target", token, request)
            .await
    }

    pub async fn complete_walg_object(
        &self,
        token: &str,
        completion: &WalGObjectCompleted,
    ) -> Result<(), CloudError> {
        let _: serde_json::Value = self
            .retry_backup_json("/v1/backups/walg/objects/complete", token, completion)
            .await?;
        Ok(())
    }

    pub async fn complete_walg_snapshot(
        &self,
        token: &str,
        completion: &WalGSnapshotCompleted,
    ) -> Result<(), CloudError> {
        let _: serde_json::Value = self
            .retry_backup_json("/v1/backups/walg/snapshots/complete", token, completion)
            .await?;
        Ok(())
    }

    async fn complete_backup(
        &self,
        token: &str,
        completion: &BackupCompleted,
    ) -> Result<(), CloudError> {
        let _: serde_json::Value = self
            .retry_backup_json("/v1/backups/complete", token, completion)
            .await?;
        Ok(())
    }

    async fn retry_backup_json<T, R>(
        &self,
        path: &str,
        token: &str,
        body: &T,
    ) -> Result<R, CloudError>
    where
        T: serde::Serialize + ?Sized,
        R: serde::de::DeserializeOwned,
    {
        let mut last_failure = None;
        for attempt in 0..3 {
            match self
                .http
                .post(self.backend.endpoint(path))
                .bearer_auth(token)
                .json(body)
                .send()
                .await
            {
                Ok(response)
                    if !matches!(response.status().as_u16(), 408 | 425 | 429 | 500..=599) =>
                {
                    return decode_managed_response(response).await;
                }
                Ok(response) => {
                    last_failure = Some(format!("managed backend returned {}", response.status()));
                }
                Err(error) => last_failure = Some(error.without_url().to_string()),
            }
            if attempt < 2 {
                tokio::time::sleep(Duration::from_millis(100 * (1 << attempt))).await;
            }
        }
        Err(CloudError::Unreachable {
            reason: last_failure.unwrap_or_else(|| "managed backend returned no response".into()),
            spooled_bytes: 0,
        })
    }

    /// Queue one OSS alert for Cloud-owned fan-out. A retry with the same
    /// source id is idempotent at the backend.
    pub async fn send_notification(
        &self,
        token: &str,
        request: &ManagedNotificationRequest,
    ) -> Result<ManagedNotificationAccepted, CloudError> {
        let response = self
            .http
            .post(self.backend.endpoint("/v1/notifications"))
            .bearer_auth(token)
            .json(request)
            .send()
            .await
            .map_err(|error| CloudError::Unreachable {
                reason: error.to_string(),
                spooled_bytes: 0,
            })?;
        decode_managed_response(response).await
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

async fn inspect_local_backup(path: &Path) -> Result<(u64, String), CloudError> {
    use tokio::io::AsyncReadExt;

    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|error| CloudError::Rejected {
            detail: format!("could not open backup artifact: {error}"),
        })?;
    let mut checksum = Sha256::new();
    let mut bytes = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|error| CloudError::Rejected {
                detail: format!("could not read backup artifact: {error}"),
            })?;
        if read == 0 {
            break;
        }
        bytes = bytes
            .checked_add(read as u64)
            .ok_or_else(|| CloudError::Rejected {
                detail: "backup artifact size exceeded the supported range".into(),
            })?;
        checksum.update(&buffer[..read]);
    }
    if bytes == 0 {
        return Err(CloudError::Rejected {
            detail: "backup artifact is empty".into(),
        });
    }
    let checksum_sha256 = checksum
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok((bytes, checksum_sha256))
}

async fn decode_managed_response<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, CloudError> {
    let status = response.status();
    if status.is_success() {
        return response
            .json::<T>()
            .await
            .map_err(|error| CloudError::Rejected {
                detail: format!("managed backend returned an unreadable response: {error}"),
            });
    }
    if matches!(status.as_u16(), 401 | 403) {
        return Err(CloudError::CredentialRejected);
    }
    if matches!(status.as_u16(), 429 | 500..=599) {
        return Err(CloudError::Unreachable {
            reason: format!("managed backend returned {status}"),
            spooled_bytes: 0,
        });
    }
    let detail = response
        .json::<serde_json::Value>()
        .await
        .ok()
        .and_then(|value| value["detail"].as_str().map(str::to_string))
        .unwrap_or_else(|| format!("managed backend returned {status}"));
    Err(CloudError::Rejected { detail })
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };

    use axum::{
        body::Body,
        extract::State,
        http::{HeaderMap, StatusCode},
        routing::{post, put},
        Json, Router,
    };
    use chrono::Utc;
    use futures::StreamExt;
    use serde_json::json;

    use super::*;

    async fn managed_chat_stub(
        State(calls): State<Arc<AtomicUsize>>,
        Json(request): Json<ManagedAiChatRequest>,
    ) -> (StatusCode, Json<serde_json::Value>) {
        let attempt = calls.fetch_add(1, Ordering::SeqCst);
        if attempt == 0 {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"detail": "temporary outage"})),
            );
        }
        (
            StatusCode::OK,
            Json(json!({
                "request_id": request.request_id,
                "settled_credits": 1,
                "provider": "test-provider",
                "model": "test-model",
                "body": {"id": "completion-1", "choices": []}
            })),
        )
    }

    #[tokio::test]
    async fn managed_chat_retries_transient_failure_with_the_same_request_id() {
        let calls = Arc::new(AtomicUsize::new(0));
        let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!("skipping managed-chat retry test: sandbox denied TCP bind");
                return;
            }
            Err(error) => panic!("bind loopback stub: {error}"),
        };
        let address = listener.local_addr().expect("stub address");
        let app = Router::new()
            .route("/v1/ai/chat/completions", post(managed_chat_stub))
            .with_state(calls.clone());
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve managed AI stub");
        });

        let client = CloudClient::new(
            BackendUrl::loopback_development(&format!("http://{address}"))
                .expect("loopback backend"),
        )
        .expect("cloud client");
        let request_id = Uuid::new_v4();
        let response = client
            .managed_ai_chat(
                "instance-token",
                &ManagedAiChatRequest {
                    request_id,
                    requested_at: Utc::now(),
                    body: json!({"messages": [{"role": "user", "content": "status?"}]}),
                },
            )
            .await
            .expect("transient outage recovers");

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(response.request_id, request_id);
        assert_eq!(response.settled_credits, 1);
    }

    struct BackupStub {
        origin: String,
        target_calls: AtomicUsize,
        upload_calls: AtomicUsize,
        complete_calls: AtomicUsize,
        first_upload_bytes: AtomicUsize,
        successful_upload_bytes: AtomicUsize,
        expected_upload_bytes: usize,
        backup_id: Mutex<Option<Uuid>>,
    }

    async fn backup_target_stub(
        State(state): State<Arc<BackupStub>>,
        Json(request): Json<BackupTargetRequest>,
    ) -> (StatusCode, Json<serde_json::Value>) {
        let attempt = state.target_calls.fetch_add(1, Ordering::SeqCst);
        let mut observed = state.backup_id.lock().expect("backup id lock");
        if let Some(id) = *observed {
            assert_eq!(id, request.backup_id, "target retry changed backup id");
        } else {
            *observed = Some(request.backup_id);
        }
        if attempt == 0 {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"detail": "target response lost"})),
            );
        }
        (
            StatusCode::CREATED,
            Json(json!({
                "backup_id": request.backup_id,
                "upload_url": format!("{}/upload", state.origin),
                "object_key": format!("backups/{}", request.backup_id),
                "expires_at_millis": Utc::now().timestamp_millis() + 60_000,
                "headers": {
                    "content-length": request.estimated_bytes.to_string(),
                    "x-amz-checksum-sha256": "provider-bound"
                }
            })),
        )
    }

    async fn backup_upload_stub(
        State(state): State<Arc<BackupStub>>,
        headers: HeaderMap,
        body: Body,
    ) -> StatusCode {
        assert_eq!(headers["x-amz-checksum-sha256"], "provider-bound");
        let attempt = state.upload_calls.fetch_add(1, Ordering::SeqCst);
        let mut stream = body.into_data_stream();
        if attempt == 0 {
            let bytes = match stream.next().await {
                Some(Ok(bytes)) => bytes.len(),
                _ => 0,
            };
            state.first_upload_bytes.store(bytes, Ordering::SeqCst);
            return StatusCode::SERVICE_UNAVAILABLE;
        }

        let mut received = 0_usize;
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(_) => return StatusCode::BAD_REQUEST,
            };
            received = match received.checked_add(chunk.len()) {
                Some(total) => total,
                None => return StatusCode::PAYLOAD_TOO_LARGE,
            };
        }
        state
            .successful_upload_bytes
            .store(received, Ordering::SeqCst);
        if received == state.expected_upload_bytes {
            StatusCode::OK
        } else {
            StatusCode::BAD_REQUEST
        }
    }

    async fn backup_complete_stub(
        State(state): State<Arc<BackupStub>>,
        Json(request): Json<BackupCompleted>,
    ) -> (StatusCode, Json<serde_json::Value>) {
        assert_eq!(
            Some(request.backup_id),
            *state.backup_id.lock().expect("backup id lock")
        );
        if state.complete_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"detail": "completion response lost"})),
            );
        }
        (StatusCode::OK, Json(json!({"state": "complete"})))
    }

    #[tokio::test]
    async fn backup_upload_recovers_each_network_boundary_without_changing_identity() {
        let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!("skipping backup retry test: sandbox denied TCP bind");
                return;
            }
            Err(error) => panic!("bind backup stub: {error}"),
        };
        let address = listener.local_addr().expect("backup stub address");
        let state = Arc::new(BackupStub {
            origin: format!("http://{address}"),
            target_calls: AtomicUsize::new(0),
            upload_calls: AtomicUsize::new(0),
            complete_calls: AtomicUsize::new(0),
            first_upload_bytes: AtomicUsize::new(0),
            successful_upload_bytes: AtomicUsize::new(0),
            expected_upload_bytes: 8 * 1024 * 1024,
            backup_id: Mutex::new(None),
        });
        let app = Router::new()
            .route("/v1/backups/target", post(backup_target_stub))
            .route("/upload", put(backup_upload_stub))
            .route("/v1/backups/complete", post(backup_complete_stub))
            .with_state(state.clone());
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve backup stub");
        });
        let temp = tempfile::tempdir().expect("backup tempdir");
        let artifact_path = temp.path().join("backup.sql.gz");
        tokio::fs::write(&artifact_path, vec![0x5a; state.expected_upload_bytes])
            .await
            .expect("write backup fixture");
        let client = CloudClient::new(
            BackendUrl::loopback_development(&format!("http://{address}"))
                .expect("loopback backup backend"),
        )
        .expect("cloud client");
        let backup_id = Uuid::new_v4();

        let target = client
            .upload_backup_file(
                "instance-token",
                Uuid::new_v4(),
                backup_id,
                "postgres/main".into(),
                BackupArtifact {
                    engine: temps_cloud_protocol::BackupEngine::Postgres,
                    format: temps_cloud_protocol::BackupFormat::PgDumpPlain,
                    compression: temps_cloud_protocol::BackupCompression::Gzip,
                    postgres_major: 18,
                },
                &artifact_path,
            )
            .await
            .expect("backup survives transient target, PUT and completion failures");

        assert_eq!(Some(target.backup_id), *state.backup_id.lock().unwrap());
        assert_eq!(target.backup_id, backup_id);
        assert_eq!(state.target_calls.load(Ordering::SeqCst), 2);
        assert_eq!(state.upload_calls.load(Ordering::SeqCst), 2);
        assert_eq!(state.complete_calls.load(Ordering::SeqCst), 2);
        let interrupted_bytes = state.first_upload_bytes.load(Ordering::SeqCst);
        assert!(
            interrupted_bytes > 0 && interrupted_bytes < state.expected_upload_bytes,
            "the injected outage must interrupt a live stream, not wait for the whole file"
        );
        assert_eq!(
            state.successful_upload_bytes.load(Ordering::SeqCst),
            state.expected_upload_bytes,
            "the retry must reopen and stream the complete artifact"
        );
    }

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
