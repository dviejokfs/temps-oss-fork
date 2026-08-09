//! The object a running instance holds.
//!
//! Owns the link state, the spool and the HTTP client, and exposes the two
//! operations the rest of the instance needs: [`CloudLink::record`], which must
//! never block or fail, and [`CloudLink::flush`], which a background task calls
//! on an interval.
//!
//! # Why `record` cannot fail
//!
//! It is called from wherever the instance already produces telemetry. If it
//! could return an error, every call site would need a decision about what to
//! do — and one of them would eventually decide to propagate it, which would
//! make an outage in *our* backend into an incident in the operator's
//! application. So it takes `&self`, returns `()`, and the worst it can do is
//! silently... no: the worst it can do is *count a drop the operator can see*.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, RwLock};
use tokio::sync::mpsc;

use temps_cloud_protocol::{
    BackupArtifact, BackupTarget, ManagedAiAnalysisRequest, ManagedAiAnalysisResponse,
    ManagedAiCapability, ManagedAiChatRequest, ManagedAiChatResponse, ManagedNotificationAccepted,
    ManagedNotificationRequest, NativeSnapshot, NativeSnapshotRequest, SpanRecord,
    WalGObjectCompleted, WalGObjectTarget, WalGObjectTargetRequest, WalGSnapshot,
    WalGSnapshotCompleted, WalGSnapshotRequest,
};
use uuid::Uuid;

use crate::spool::Spool;
use crate::state::EnrollmentState;
use crate::status::{LinkStatus, MirrorHealth};
use crate::{BackendUrl, CloudClient, CloudError};

/// Spans per shipment. Small enough that one failure loses little progress.
const BATCH_SIZE: usize = 500;
/// Number of producer batches accepted before the mirror starts shedding load.
/// The local telemetry store remains authoritative and is never affected.
const INCOMING_BATCH_CAPACITY: usize = 8;

/// What a flush attempt did. Returned so a caller can log or schedule backoff.
#[derive(Debug, Clone, PartialEq)]
pub enum FlushOutcome {
    /// Nothing buffered.
    Idle,
    /// Not linked, so there is nothing to mirror to.
    NotLinked,
    Shipped {
        spans: usize,
    },
    /// Kept for a later attempt.
    Retained {
        spans: usize,
        reason: String,
    },
    /// Shipment needs operator action, but the batch remains retained.
    Blocked {
        spans: usize,
        reason: String,
    },
}

#[derive(Clone)]
struct PendingSubmission {
    submission_id: Uuid,
    spans: Vec<SpanRecord>,
}

struct IncomingBatch {
    generation: u64,
    spans: Vec<SpanRecord>,
}

pub struct CloudLink {
    state: RwLock<Option<EnrollmentState>>,
    incoming_tx: mpsc::Sender<IncomingBatch>,
    incoming_rx: Mutex<mpsc::Receiver<IncomingBatch>>,
    incoming_spans: AtomicUsize,
    incoming_dropped: AtomicU64,
    linked: AtomicBool,
    spool: Mutex<Spool>,
    /// The active submission stays here until a matching full acknowledgement
    /// arrives, preserving its id across retries.
    pending: Mutex<Option<PendingSubmission>>,
    health: RwLock<MirrorHealth>,
    state_path: PathBuf,
    agent_version: String,
    /// Set when the backend refuses our token. Distinct from mirror health:
    /// this one needs the operator, not time.
    credential_rejected: AtomicBool,
    generation: AtomicU64,
    flush_lock: tokio::sync::Mutex<()>,
    allow_loopback_development: bool,
}

impl CloudLink {
    /// Load from disk. An unlinked or absent state is a normal outcome, not an
    /// error — most instances never connect anything.
    pub fn load(data_dir: PathBuf, agent_version: impl Into<String>) -> Self {
        Self::load_inner(data_dir, agent_version, false)
    }

    /// Local-test constructor. Production callers must use [`CloudLink::load`].
    pub fn load_for_loopback_development(
        data_dir: PathBuf,
        agent_version: impl Into<String>,
    ) -> Self {
        Self::load_inner(data_dir, agent_version, true)
    }

    fn load_inner(
        data_dir: PathBuf,
        agent_version: impl Into<String>,
        allow_loopback_development: bool,
    ) -> Self {
        // Credentials live in their own directory; state hardening must never
        // chmod an operator's shared TEMPS_DATA_DIR.
        let state_path = data_dir.join("cloud-link").join("state.json");
        let state = EnrollmentState::load(&state_path).unwrap_or_else(|e| {
            // Corruption is reported, not silently reset: overwriting would
            // destroy a token the operator may still be able to recover.
            tracing::error!(error = %e, "link state unreadable; treating as unlinked");
            None
        });
        let linked = state.as_ref().is_some_and(EnrollmentState::is_linked);
        let (incoming_tx, incoming_rx) = mpsc::channel(INCOMING_BATCH_CAPACITY);

        Self {
            state: RwLock::new(state),
            incoming_tx,
            incoming_rx: Mutex::new(incoming_rx),
            incoming_spans: AtomicUsize::new(0),
            incoming_dropped: AtomicU64::new(0),
            linked: AtomicBool::new(linked),
            spool: Mutex::new(Spool::with_default_capacity()),
            pending: Mutex::new(None),
            health: RwLock::new(MirrorHealth::Healthy),
            state_path,
            agent_version: agent_version.into(),
            credential_rejected: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            flush_lock: tokio::sync::Mutex::new(()),
            allow_loopback_development,
        }
    }

    fn parse_backend(&self, value: &str) -> Result<BackendUrl, CloudError> {
        if self.allow_loopback_development {
            BackendUrl::loopback_development(value)
        } else {
            BackendUrl::production(value)
        }
    }

    pub fn status(&self) -> LinkStatus {
        match &*self.state.read().unwrap_or_else(|p| p.into_inner()) {
            None => LinkStatus::NotConfigured,
            Some(s) if s.is_linked() => {
                let base_url = s.base_url.clone();
                // A token that still exists but is no longer accepted is its
                // own state: the operator must re-enroll, and no amount of
                // waiting will fix it. Reporting it as plain `Linked` would
                // leave them watching a spool that never drains.
                if self.credential_rejected.load(Ordering::SeqCst) {
                    LinkStatus::CredentialRejected { base_url }
                } else {
                    LinkStatus::Linked { base_url }
                }
            }
            Some(s) => LinkStatus::AwaitingEnrollment {
                base_url: s.base_url.clone(),
            },
        }
    }

    pub fn health(&self) -> MirrorHealth {
        let spool_dropped = self
            .spool
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .dropped();
        let dropped = spool_dropped.saturating_add(self.incoming_dropped.load(Ordering::Relaxed));
        if dropped > 0 {
            return MirrorHealth::Dropping {
                spooled: self.spooled(),
                dropped,
            };
        }
        self.health
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub fn instance_id(&self) -> Option<Uuid> {
        self.state
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .map(|s| s.instance_id)
    }

    pub fn tenant_id(&self) -> Option<Uuid> {
        self.state
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .and_then(|state| state.tenant_id)
    }

    pub fn account_email(&self) -> Option<String> {
        self.state
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .and_then(|state| state.account_email.clone())
    }

    /// Lock-free fast-path hint for telemetry producers. Enrollment can race
    /// with an offer; generation tagging prevents a raced batch crossing links.
    pub fn is_linked(&self) -> bool {
        self.linked.load(Ordering::Acquire)
    }

    /// Point this instance at a backend without linking it yet.
    pub fn configure(&self, backend: BackendUrl) -> Result<(), crate::state::StateError> {
        let mut guard = self.state.write().unwrap_or_else(|p| p.into_inner());
        let next_url = backend.as_str().to_string();
        if let Some(existing) = guard.as_ref() {
            if existing.is_linked() && existing.base_url != next_url {
                return Err(crate::state::StateError::BackendChangeRequiresDisconnect {
                    current: existing.base_url.clone(),
                    requested: next_url,
                });
            }
        }
        let mut changed_origin = false;
        let next = match guard.as_ref() {
            Some(existing) => {
                let mut existing = existing.clone();
                if existing.base_url != next_url {
                    changed_origin = true;
                    // Credentials are origin-bound. Keeping a token while
                    // changing its destination would exfiltrate it on flush.
                    // Buffered telemetry is origin-bound for the same reason.
                    existing.unlink();
                }
                existing.base_url = next_url;
                existing
            }
            None => EnrollmentState::new(next_url),
        };
        next.save(&self.state_path)?;
        if changed_origin {
            self.linked.store(false, Ordering::Release);
            self.spool
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take(usize::MAX);
            self.pending
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take();
            self.credential_rejected.store(false, Ordering::SeqCst);
            *self.health.write().unwrap_or_else(|p| p.into_inner()) = MirrorHealth::Healthy;
        }
        *guard = Some(next);
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.linked.store(
            guard.as_ref().is_some_and(EnrollmentState::is_linked),
            Ordering::Release,
        );
        Ok(())
    }

    /// Redeem an operator-pasted code and persist the resulting credential.
    pub async fn enroll(&self, code: &str) -> Result<(), CloudError> {
        let (base_url, instance_id, generation) = {
            let guard = self.state.read().unwrap_or_else(|p| p.into_inner());
            let s = guard.as_ref().ok_or(CloudError::NotEnrolled)?;
            (
                s.base_url.clone(),
                s.instance_id,
                self.generation.load(Ordering::SeqCst),
            )
        };

        let backend = self.parse_backend(&base_url)?;
        let res = CloudClient::new(backend)?
            .enroll(code, instance_id, &self.agent_version)
            .await?;

        let mut guard = self.state.write().unwrap_or_else(|p| p.into_inner());
        let current = guard
            .as_ref()
            .ok_or_else(|| CloudError::EnrollmentRefused {
                detail: "link state changed while enrollment was in progress; try again".into(),
            })?;
        if self.generation.load(Ordering::SeqCst) != generation
            || current.base_url != base_url
            || current.instance_id != instance_id
        {
            return Err(CloudError::EnrollmentRefused {
                detail: "link state changed while enrollment was in progress; try again".into(),
            });
        }
        let mut next = current.clone();
        next.token = Some(res.instance_token);
        next.tenant_id = Some(res.tenant_id);
        next.account_email = res.account_email;
        // Clone → save → swap: a failed disk write cannot leave a credential
        // alive only in memory.
        next.save(&self.state_path)
            .map_err(|e| CloudError::EnrollmentRefused {
                detail: format!("enrolled, but the credential could not be saved: {e}"),
            })?;
        *guard = Some(next);
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.linked.store(true, Ordering::Release);
        self.credential_rejected.store(false, Ordering::SeqCst);
        *self.health.write().unwrap_or_else(|p| p.into_inner()) = MirrorHealth::Healthy;
        Ok(())
    }

    /// Revoke the active credential at its issuing backend.
    ///
    /// This deliberately leaves local state untouched. The caller may only
    /// remove the local credential after this succeeds, or after the backend
    /// confirms that the credential is already invalid.
    pub async fn revoke(&self) -> Result<(), CloudError> {
        let (base_url, token) = {
            let guard = self.state.read().unwrap_or_else(|p| p.into_inner());
            let state = guard.as_ref().ok_or(CloudError::NotEnrolled)?;
            let token = state.token.clone().ok_or(CloudError::NotEnrolled)?;
            (state.base_url.clone(), token)
        };
        let backend = self.parse_backend(&base_url)?;
        CloudClient::new(backend)?.revoke(&token).await
    }

    pub async fn managed_ai_capability(&self) -> Result<ManagedAiCapability, CloudError> {
        let (base_url, token) = self.linked_credential()?;
        let backend = self.parse_backend(&base_url)?;
        CloudClient::new(backend)?
            .managed_ai_capability(&token)
            .await
    }

    pub async fn managed_ai_analysis(
        &self,
        request: &ManagedAiAnalysisRequest,
    ) -> Result<ManagedAiAnalysisResponse, CloudError> {
        let (base_url, token) = self.linked_credential()?;
        let backend = self.parse_backend(&base_url)?;
        CloudClient::new(backend)?
            .managed_ai_analysis(&token, request)
            .await
    }

    pub async fn managed_ai_chat(
        &self,
        request: &ManagedAiChatRequest,
    ) -> Result<ManagedAiChatResponse, CloudError> {
        let (base_url, token) = self.linked_credential()?;
        let backend = self.parse_backend(&base_url)?;
        CloudClient::new(backend)?
            .managed_ai_chat(&token, request)
            .await
    }

    /// Upload a completed local artifact without ever routing its bytes through
    /// the Cloud API process. The instance credential is read only for the two
    /// small lifecycle calls around the direct object-storage PUT.
    pub async fn upload_backup_file(
        &self,
        backup_id: Uuid,
        source: String,
        artifact: BackupArtifact,
        path: &Path,
    ) -> Result<BackupTarget, CloudError> {
        let (base_url, token, instance_id) = self.linked_backup_credential()?;
        let backend = self.parse_backend(&base_url)?;
        CloudClient::new(backend)?
            .upload_backup_file(&token, instance_id, backup_id, source, artifact, path)
            .await
    }

    fn walg_client(&self) -> Result<(CloudClient, String, Uuid), CloudError> {
        let (base_url, token, instance_id) = self.linked_backup_credential()?;
        let backend = self.parse_backend(&base_url)?;
        Ok((CloudClient::new(backend)?, token, instance_id))
    }

    pub async fn declare_walg_snapshot(
        &self,
        request: &WalGSnapshotRequest,
    ) -> Result<WalGSnapshot, CloudError> {
        let (client, token, instance_id) = self.walg_client()?;
        if request.instance_id != instance_id {
            return Err(CloudError::Rejected {
                detail: "WAL-G snapshot instance_id does not match this Cloud link".into(),
            });
        }
        client.declare_walg_snapshot(&token, request).await
    }

    pub async fn declare_native_snapshot(
        &self,
        request: &NativeSnapshotRequest,
    ) -> Result<NativeSnapshot, CloudError> {
        let (client, token, instance_id) = self.walg_client()?;
        if request.instance_id != instance_id {
            return Err(CloudError::Rejected {
                detail: "native snapshot instance_id does not match this Cloud link".into(),
            });
        }
        client.declare_native_snapshot(&token, request).await
    }

    pub async fn native_object_target(
        &self,
        request: &WalGObjectTargetRequest,
    ) -> Result<WalGObjectTarget, CloudError> {
        let (client, token, instance_id) = self.walg_client()?;
        if request.instance_id != instance_id {
            return Err(CloudError::Rejected {
                detail: "native object instance_id does not match this Cloud link".into(),
            });
        }
        client.native_object_target(&token, request).await
    }

    pub async fn complete_native_object(
        &self,
        completion: &WalGObjectCompleted,
    ) -> Result<(), CloudError> {
        let (client, token, _) = self.walg_client()?;
        client.complete_native_object(&token, completion).await
    }

    pub async fn complete_native_snapshot(
        &self,
        completion: &WalGSnapshotCompleted,
    ) -> Result<(), CloudError> {
        let (client, token, _) = self.walg_client()?;
        client.complete_native_snapshot(&token, completion).await
    }

    pub async fn walg_object_target(
        &self,
        request: &WalGObjectTargetRequest,
    ) -> Result<WalGObjectTarget, CloudError> {
        let (client, token, instance_id) = self.walg_client()?;
        if request.instance_id != instance_id {
            return Err(CloudError::Rejected {
                detail: "WAL-G object instance_id does not match this Cloud link".into(),
            });
        }
        client.walg_object_target(&token, request).await
    }

    /// Stream a repository object to the exact short-lived destination Cloud
    /// issued. Destination validation and redirect prevention live in
    /// [`CloudClient`] and are therefore identical for WAL-G, native snapshots
    /// and local backup files.
    pub async fn upload_backup_object(
        &self,
        target: &WalGObjectTarget,
        body: reqwest::Body,
        spooled_bytes: u64,
    ) -> Result<reqwest::Response, CloudError> {
        let (client, _, _) = self.walg_client()?;
        client
            .upload_backup_object(target, body, spooled_bytes)
            .await
    }

    pub async fn complete_walg_object(
        &self,
        completion: &WalGObjectCompleted,
    ) -> Result<(), CloudError> {
        let (client, token, _) = self.walg_client()?;
        client.complete_walg_object(&token, completion).await
    }

    pub async fn complete_walg_snapshot(
        &self,
        completion: &WalGSnapshotCompleted,
    ) -> Result<(), CloudError> {
        let (client, token, _) = self.walg_client()?;
        client.complete_walg_snapshot(&token, completion).await
    }

    pub async fn send_notification(
        &self,
        request: &ManagedNotificationRequest,
    ) -> Result<ManagedNotificationAccepted, CloudError> {
        let (base_url, token) = self.linked_credential()?;
        let backend = self.parse_backend(&base_url)?;
        CloudClient::new(backend)?
            .send_notification(&token, request)
            .await
    }

    fn linked_credential(&self) -> Result<(String, String), CloudError> {
        let guard = self.state.read().unwrap_or_else(|p| p.into_inner());
        let state = guard.as_ref().ok_or(CloudError::NotEnrolled)?;
        let token = state.token.clone().ok_or(CloudError::NotEnrolled)?;
        Ok((state.base_url.clone(), token))
    }

    fn linked_backup_credential(&self) -> Result<(String, String, Uuid), CloudError> {
        let guard = self.state.read().unwrap_or_else(|p| p.into_inner());
        let state = guard.as_ref().ok_or(CloudError::NotEnrolled)?;
        let token = state.token.clone().ok_or(CloudError::NotEnrolled)?;
        Ok((state.base_url.clone(), token, state.instance_id))
    }

    /// Forget the credential. Keeps the instance identity so re-linking later
    /// reattaches to the same record.
    pub fn disconnect(&self) -> Result<(), crate::state::StateError> {
        self.linked.store(false, Ordering::Release);
        let mut guard = self.state.write().unwrap_or_else(|p| p.into_inner());
        if let Some(s) = guard.as_mut() {
            let mut next = s.clone();
            next.unlink();
            next.save(&self.state_path)?;
            *s = next;
        }
        self.spool
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take(usize::MAX);
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        self.credential_rejected.store(false, Ordering::SeqCst);
        self.generation.fetch_add(1, Ordering::SeqCst);
        *self.health.write().unwrap_or_else(|p| p.into_inner()) = MirrorHealth::Healthy;
        Ok(())
    }

    /// Offer spans to the mirror. Never blocks on IO, never fails.
    ///
    /// When the instance is not linked this is a no-op: buffering for a backend
    /// that does not exist would burn memory to no purpose. Telemetry is still
    /// stored locally by the instance itself — that path is untouched.
    pub fn record(&self, spans: Vec<SpanRecord>) {
        if spans.is_empty() || !self.linked.load(Ordering::Acquire) {
            return;
        }
        let count = spans.len();
        self.incoming_spans.fetch_add(count, Ordering::Relaxed);
        let batch = IncomingBatch {
            generation: self.generation.load(Ordering::Acquire),
            spans,
        };
        if let Err(error) = self.incoming_tx.try_send(batch) {
            let dropped = error.into_inner().spans.len();
            self.incoming_spans.fetch_sub(dropped, Ordering::Relaxed);
            self.incoming_dropped
                .fetch_add(dropped as u64, Ordering::Relaxed);
        }
    }

    pub fn spooled(&self) -> usize {
        // Status reads are off the ingest path and may opportunistically move
        // accepted producer batches into the bounded spool for an exact count.
        self.drain_incoming();
        let queued = self.spool.lock().unwrap_or_else(|p| p.into_inner()).len();
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .map_or(0, |batch| batch.spans.len());
        self.incoming_spans.load(Ordering::Relaxed) + queued + pending
    }

    fn drain_incoming(&self) {
        let current_generation = self.generation.load(Ordering::Acquire);
        let mut receiver = self.incoming_rx.lock().unwrap_or_else(|p| p.into_inner());
        let mut spool = self.spool.lock().unwrap_or_else(|p| p.into_inner());
        while let Ok(batch) = receiver.try_recv() {
            self.incoming_spans
                .fetch_sub(batch.spans.len(), Ordering::Relaxed);
            if batch.generation == current_generation && self.linked.load(Ordering::Acquire) {
                spool.push(batch.spans);
            }
        }
    }

    /// Ship one batch. Called on an interval by a background task.
    pub async fn flush(&self) -> FlushOutcome {
        let _flush = self.flush_lock.lock().await;
        self.drain_incoming();
        let (base_url, token, generation) = {
            let guard = self.state.read().unwrap_or_else(|p| p.into_inner());
            match guard.as_ref() {
                Some(s) if s.is_linked() => (
                    s.base_url.clone(),
                    s.token.clone().unwrap_or_default(),
                    self.generation.load(Ordering::SeqCst),
                ),
                _ => return FlushOutcome::NotLinked,
            }
        };

        let pending = {
            let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
            if pending.is_none() {
                let spans = self
                    .spool
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .take(BATCH_SIZE);
                if !spans.is_empty() {
                    *pending = Some(PendingSubmission {
                        submission_id: Uuid::new_v4(),
                        spans,
                    });
                }
            }
            pending.clone()
        };
        let Some(pending) = pending else {
            *self.health.write().unwrap_or_else(|p| p.into_inner()) = MirrorHealth::Healthy;
            return FlushOutcome::Idle;
        };
        let count = pending.spans.len();
        let backend = match self.parse_backend(&base_url) {
            Ok(backend) => backend,
            Err(e) => {
                return FlushOutcome::Blocked {
                    spans: count,
                    reason: e.to_string(),
                }
            }
        };

        let client = match CloudClient::new(backend) {
            Ok(client) => client,
            Err(e) => {
                return FlushOutcome::Blocked {
                    spans: count,
                    reason: e.to_string(),
                }
            }
        };

        let result = client
            .ship(&token, pending.submission_id, pending.spans.clone())
            .await;
        if self.generation.load(Ordering::SeqCst) != generation {
            return FlushOutcome::Blocked {
                spans: count,
                reason: "link state changed while this shipment was in progress".into(),
            };
        }

        match result {
            Ok(ack) => {
                let mut current = self.pending.lock().unwrap_or_else(|p| p.into_inner());
                if current
                    .as_ref()
                    .is_some_and(|value| value.submission_id == pending.submission_id)
                {
                    current.take();
                }
                self.credential_rejected.store(false, Ordering::SeqCst);
                *self.health.write().unwrap_or_else(|p| p.into_inner()) = match ack.warning {
                    Some(detail) => MirrorHealth::Degraded { detail },
                    None => MirrorHealth::Healthy,
                };
                FlushOutcome::Shipped { spans: count }
            }

            Err(e) if e.is_retryable() => {
                if matches!(e, CloudError::CredentialRejected) {
                    self.credential_rejected.store(true, Ordering::SeqCst);
                }
                let spool = self.spool.lock().unwrap_or_else(|p| p.into_inner());
                let spooled = spool.len() + count;
                let dropped = spool
                    .dropped()
                    .saturating_add(self.incoming_dropped.load(Ordering::Relaxed));

                *self.health.write().unwrap_or_else(|p| p.into_inner()) = if dropped > 0 {
                    MirrorHealth::Dropping { spooled, dropped }
                } else {
                    MirrorHealth::Buffering {
                        spooled,
                        reason: e.to_string(),
                    }
                };
                FlushOutcome::Retained {
                    spans: count,
                    reason: e.to_string(),
                }
            }

            Err(e) => {
                // Never infer that a 4xx or version-skew response makes customer
                // telemetry disposable. Keep the bounded pending batch and make
                // the operator-visible state explicit.
                *self.health.write().unwrap_or_else(|p| p.into_inner()) = MirrorHealth::Buffering {
                    spooled: self.spooled(),
                    reason: e.to_string(),
                };
                FlushOutcome::Blocked {
                    spans: count,
                    reason: e.to_string(),
                }
            }
        }
    }
}
