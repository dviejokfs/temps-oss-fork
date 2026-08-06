use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::audit::PermissionDeniedAudit;

const DEFAULT_QUEUE_CAPACITY: usize = 1_024;
const DEFAULT_MAX_AGGREGATIONS: usize = 1_024;
const DEFAULT_WINDOW: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum AuthSourceKind {
    Anonymous,
    Session,
    CliToken,
    ApiKey,
    DeploymentToken,
}

impl AuthSourceKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Anonymous => "anonymous",
            Self::Session => "session",
            Self::CliToken => "cli_token",
            Self::ApiKey => "api_key",
            Self::DeploymentToken => "deployment_token",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SafePrincipal {
    pub(crate) user_id: Option<i32>,
    pub(crate) source: AuthSourceKind,
    pub(crate) credential_id: Option<i32>,
}

impl SafePrincipal {
    pub(crate) const fn anonymous() -> Self {
        Self {
            user_id: None,
            source: AuthSourceKind::Anonymous,
            credential_id: None,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PermissionDenialEvent {
    pub(crate) principal: SafePrincipal,
    pub(crate) method: String,
    pub(crate) route: String,
    pub(crate) denial_kind: String,
    pub(crate) required_permission: Option<String>,
    pub(crate) ip_address: Option<String>,
    pub(crate) user_agent: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct AggregationKey {
    user_id: Option<i32>,
    source: AuthSourceKind,
    credential_id: Option<i32>,
    method: String,
    route: String,
    denial_kind: String,
    required_permission: Option<String>,
}

impl From<&PermissionDenialEvent> for AggregationKey {
    fn from(event: &PermissionDenialEvent) -> Self {
        Self {
            user_id: event.principal.user_id,
            source: event.principal.source,
            credential_id: event.principal.credential_id,
            method: event.method.clone(),
            route: event.route.clone(),
            denial_kind: event.denial_kind.clone(),
            required_permission: event.required_permission.clone(),
        }
    }
}

impl PermissionDenialEvent {
    fn into_audit(self) -> PermissionDeniedAudit {
        PermissionDeniedAudit {
            user_id: self.principal.user_id,
            auth_source: self.principal.source.as_str().to_string(),
            credential_id: self.principal.credential_id,
            method: self.method,
            route: self.route,
            denial_kind: self.denial_kind,
            required_permission: self.required_permission,
            attempt_count: 1,
            ip_address: self.ip_address,
            user_agent: self.user_agent,
        }
    }
}

struct RecorderCounters {
    queue_drops: AtomicU64,
    aggregation_overflows: AtomicU64,
    write_failures: AtomicU64,
}

impl RecorderCounters {
    fn new() -> Self {
        Self {
            queue_drops: AtomicU64::new(0),
            aggregation_overflows: AtomicU64::new(0),
            write_failures: AtomicU64::new(0),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct RecorderConfig {
    queue_capacity: usize,
    max_aggregations: usize,
    window: Duration,
}

/// Non-blocking, explicitly bounded entry point for permission-denial audit
/// events. Request handling performs only `try_send`; aggregation and audit
/// logger I/O happen in the spawned worker.
pub struct PermissionDenialRecorder {
    sender: mpsc::Sender<PermissionDenialEvent>,
    counters: Arc<RecorderCounters>,
}

impl PermissionDenialRecorder {
    pub fn new(logger: Arc<dyn temps_core::AuditLogger>) -> Arc<Self> {
        Self::with_config(
            logger,
            RecorderConfig {
                queue_capacity: DEFAULT_QUEUE_CAPACITY,
                max_aggregations: DEFAULT_MAX_AGGREGATIONS,
                window: DEFAULT_WINDOW,
            },
        )
    }

    fn with_config(logger: Arc<dyn temps_core::AuditLogger>, config: RecorderConfig) -> Arc<Self> {
        let (sender, receiver) = mpsc::channel(config.queue_capacity);
        let counters = Arc::new(RecorderCounters::new());
        let worker = PermissionDenialWorker {
            receiver,
            logger,
            counters: counters.clone(),
            max_aggregations: config.max_aggregations,
            window: config.window,
            pending: HashMap::with_capacity(config.max_aggregations),
        };
        tokio::spawn(worker.run());

        Arc::new(Self { sender, counters })
    }

    pub(crate) fn record(&self, event: PermissionDenialEvent) {
        if let Err(error) = self.sender.try_send(event) {
            let event = error.into_inner();
            let total = self.counters.queue_drops.fetch_add(1, Ordering::Relaxed) + 1;
            if should_log_counter(total) {
                tracing::warn!(
                    total_queue_drops = total,
                    user_id = event.principal.user_id,
                    auth_source = event.principal.source.as_str(),
                    credential_id = event.principal.credential_id,
                    method = event.method,
                    route = event.route,
                    denial_kind = event.denial_kind,
                    required_permission = event.required_permission,
                    "permission-denial audit queue saturated or closed; event dropped"
                );
            }
        }
    }
}

fn should_log_counter(total: u64) -> bool {
    total.is_power_of_two()
}

struct PermissionDenialWorker {
    receiver: mpsc::Receiver<PermissionDenialEvent>,
    logger: Arc<dyn temps_core::AuditLogger>,
    counters: Arc<RecorderCounters>,
    max_aggregations: usize,
    window: Duration,
    pending: HashMap<AggregationKey, PermissionDeniedAudit>,
}

impl PermissionDenialWorker {
    async fn run(mut self) {
        let start = tokio::time::Instant::now() + self.window;
        let mut ticker = tokio::time::interval_at(start, self.window);

        loop {
            tokio::select! {
                event = self.receiver.recv() => {
                    match event {
                        Some(event) => self.aggregate(event),
                        None => {
                            self.flush().await;
                            break;
                        }
                    }
                }
                _ = ticker.tick() => self.flush().await,
            }
        }
    }

    fn aggregate(&mut self, event: PermissionDenialEvent) {
        let key = AggregationKey::from(&event);
        if let Some(audit) = self.pending.get_mut(&key) {
            audit.attempt_count = audit.attempt_count.saturating_add(1);
            return;
        }

        if self.pending.len() >= self.max_aggregations {
            let total = self
                .counters
                .aggregation_overflows
                .fetch_add(1, Ordering::Relaxed)
                + 1;
            if should_log_counter(total) {
                tracing::warn!(
                    total_aggregation_overflows = total,
                    max_aggregations = self.max_aggregations,
                    user_id = event.principal.user_id,
                    auth_source = event.principal.source.as_str(),
                    credential_id = event.principal.credential_id,
                    method = event.method,
                    route = event.route,
                    denial_kind = event.denial_kind,
                    required_permission = event.required_permission,
                    "permission-denial aggregation map full; new key dropped"
                );
            }
            return;
        }

        self.pending.insert(key, event.into_audit());
    }

    async fn flush(&mut self) {
        // At most `max_aggregations` entries are moved into this temporary
        // vector. The queue and map remain bounded while logger I/O is in flight.
        let audits: Vec<_> = self.pending.drain().map(|(_, audit)| audit).collect();
        for audit in audits {
            if let Err(error) = self.logger.create_audit_log(&audit).await {
                let total = self.counters.write_failures.fetch_add(1, Ordering::Relaxed) + 1;
                if should_log_counter(total) {
                    tracing::warn!(
                        total_write_failures = total,
                        user_id = audit.user_id,
                        auth_source = audit.auth_source,
                        credential_id = audit.credential_id,
                        method = audit.method,
                        route = audit.route,
                        denial_kind = audit.denial_kind,
                        required_permission = audit.required_permission,
                        error = %error,
                        "failed to persist permission-denial audit; denial response was unaffected"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use anyhow::Result;
    use temps_core::{AuditLogger, AuditOperation};

    use super::*;

    #[derive(Default)]
    struct RecordingLogger {
        records: Mutex<Vec<serde_json::Value>>,
    }

    #[async_trait::async_trait]
    impl AuditLogger for RecordingLogger {
        async fn create_audit_log(&self, operation: &dyn AuditOperation) -> Result<()> {
            let serialized = operation.serialize()?;
            let value = serde_json::from_str(&serialized)?;
            self.records
                .lock()
                .map_err(|_| anyhow::anyhow!("test logger lock poisoned"))?
                .push(value);
            Ok(())
        }
    }

    struct FailingLogger;

    #[async_trait::async_trait]
    impl AuditLogger for FailingLogger {
        async fn create_audit_log(&self, _operation: &dyn AuditOperation) -> Result<()> {
            Err(anyhow::anyhow!("intentional logger failure"))
        }
    }

    fn event(route: &str) -> PermissionDenialEvent {
        PermissionDenialEvent {
            principal: SafePrincipal {
                user_id: Some(42),
                source: AuthSourceKind::ApiKey,
                credential_id: Some(9),
            },
            method: "PATCH".to_string(),
            route: route.to_string(),
            denial_kind: "insufficient_permission".to_string(),
            required_permission: Some("projects:write".to_string()),
            ip_address: Some("203.0.113.7".to_string()),
            user_agent: "test-agent".to_string(),
        }
    }

    fn test_config(queue_capacity: usize, max_aggregations: usize) -> RecorderConfig {
        RecorderConfig {
            queue_capacity,
            max_aggregations,
            window: Duration::from_millis(20),
        }
    }

    #[tokio::test]
    async fn repeated_safe_key_is_aggregated_and_ip_is_not_part_of_key() {
        let logger = Arc::new(RecordingLogger::default());
        let recorder = PermissionDenialRecorder::with_config(logger.clone(), test_config(8, 8));
        recorder.record(event("/projects/{project_id}"));
        let mut second = event("/projects/{project_id}");
        second.ip_address = Some("198.51.100.99".to_string());
        recorder.record(second);

        tokio::time::sleep(Duration::from_millis(60)).await;
        let records = logger.records.lock().expect("test logger lock");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["attempt_count"], 2);
        assert_eq!(records[0]["route"], "/projects/{project_id}");
        assert_eq!(records[0]["auth_source"], "api_key");
        assert_eq!(records[0]["credential_id"], 9);
        assert!(records[0].get("key_name").is_none());
        assert!(records[0].get("token_name").is_none());
    }

    #[tokio::test]
    async fn aggregation_cardinality_is_bounded() {
        let logger = Arc::new(RecordingLogger::default());
        let recorder = PermissionDenialRecorder::with_config(logger.clone(), test_config(8, 1));
        recorder.record(event("/first"));
        recorder.record(event("/second"));

        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(
            recorder
                .counters
                .aggregation_overflows
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(logger.records.lock().expect("test logger lock").len(), 1);
    }

    #[tokio::test]
    async fn queue_overflow_is_counted_without_blocking() {
        let (sender, _receiver) = mpsc::channel(1);
        let counters = Arc::new(RecorderCounters::new());
        let recorder = PermissionDenialRecorder {
            sender,
            counters: counters.clone(),
        };

        recorder.record(event("/first"));
        recorder.record(event("/second"));
        assert_eq!(counters.queue_drops.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn logger_failure_is_counted_and_worker_continues() {
        let recorder =
            PermissionDenialRecorder::with_config(Arc::new(FailingLogger), test_config(8, 8));
        recorder.record(event("/first"));
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(recorder.counters.write_failures.load(Ordering::Relaxed), 1);

        recorder.record(event("/second"));
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(recorder.counters.write_failures.load(Ordering::Relaxed), 2);
    }
}
