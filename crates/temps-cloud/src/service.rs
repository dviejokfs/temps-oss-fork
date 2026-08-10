use std::{
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use serde::Serialize;
use temps_cloud_client::{BackendUrl, CloudError, CloudFeatureSwitches, CloudLink};
use temps_cloud_protocol::{ManagedNotificationAccepted, ManagedNotificationRequest};
use temps_config::{ConfigService, ConfigServiceError};
use thiserror::Error;
use tokio::sync::watch;
use utoipa::ToSchema;
use uuid::Uuid;

const SETUP_PATH: &str = "/settings/cloud";
const SHUTDOWN_TASK_TIMEOUT: Duration = Duration::from_secs(6);

#[derive(Debug, Error)]
pub enum CloudServiceError {
    #[error("Could not read managed-control-plane settings: {0}")]
    Configuration(#[from] ConfigServiceError),
    #[error("Managed-control-plane URL is invalid: {reason}")]
    InvalidBackend { reason: String },
    #[error("Managed-control-plane operation failed: {0}")]
    Client(CloudError),
    #[error("Could not persist the managed-control-plane link: {0}")]
    State(temps_cloud_client::state::StateError),
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CloudCapability {
    pub configured: bool,
    pub reason: Option<String>,
    pub setup_path: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CloudAiCapability {
    pub configured: bool,
    pub reason: Option<String>,
    pub setup_path: String,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CloudStatus {
    pub status: String,
    pub status_message: String,
    pub health: String,
    pub health_message: String,
    #[schema(value_type = Option<String>)]
    pub instance_id: Option<Uuid>,
    pub account_email: Option<String>,
    pub spooled_spans: usize,
    pub backend_url: String,
    pub telemetry_enabled: bool,
    pub backups_enabled: bool,
    pub notifications_enabled: bool,
}

pub struct CloudService {
    link: Arc<CloudLink>,
    config: Arc<ConfigService>,
    cancel: watch::Sender<bool>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    backup_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    allow_loopback_development: bool,
    configuration_issue: RwLock<Option<String>>,
}

impl CloudService {
    pub fn new(
        link: Arc<CloudLink>,
        config: Arc<ConfigService>,
        allow_loopback_development: bool,
    ) -> Self {
        let (cancel, _) = watch::channel(false);
        Self {
            link,
            config,
            cancel,
            task: Mutex::new(None),
            backup_task: Mutex::new(None),
            allow_loopback_development,
            configuration_issue: RwLock::new(None),
        }
    }

    fn set_configuration_issue(&self, issue: Option<String>) {
        *self
            .configuration_issue
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = issue;
    }

    fn configuration_issue(&self) -> Option<String> {
        self.configuration_issue
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn start_flusher(&self) {
        let mut task = self
            .task
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if task.is_none() {
            let link = self.link.clone();
            let cancel = self.cancel.subscribe();
            *task = Some(tokio::spawn(async move {
                temps_cloud_client::flusher::run(link, cancel).await;
            }));
        }
    }

    pub fn start_backup_mirror(
        &self,
        db: Arc<sea_orm::DatabaseConnection>,
        encryption: Arc<temps_core::EncryptionService>,
    ) {
        let mut task = self
            .backup_task
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if task.is_none() {
            tracing::info!("Cloud service launching backup mirror task");
            let link = self.link.clone();
            let cancel = self.cancel.subscribe();
            *task = Some(tokio::spawn(async move {
                crate::backup_mirror::run(link, db, encryption, cancel).await;
            }));
        } else {
            tracing::debug!("Cloud backup mirror task is already registered");
        }
    }

    pub fn link(&self) -> Arc<CloudLink> {
        self.link.clone()
    }

    /// Apply explicit persisted operator consent. Enrollment does not call
    /// this method and therefore cannot enable exports by itself.
    pub fn set_feature_switches(
        &self,
        switches: CloudFeatureSwitches,
    ) -> Result<(), CloudServiceError> {
        self.link
            .set_feature_switches(switches)
            .map_err(CloudServiceError::State)
    }

    pub fn feature_switches(&self) -> CloudFeatureSwitches {
        self.link.feature_switches()
    }

    pub async fn initialize(&self) -> Result<(), CloudServiceError> {
        let settings = match self.config.get_settings().await {
            Ok(settings) => settings,
            Err(error) => {
                tracing::error!(%error, "Cloud settings unavailable; Cloud integration disabled");
                if let Err(state_error) = self
                    .link
                    .set_feature_switches(CloudFeatureSwitches::default())
                {
                    tracing::error!(%state_error, "could not purge Cloud telemetry while disabling integration");
                }
                self.link
                    .block_outbound("Cloud settings are unavailable; fix local settings and retry");
                self.set_configuration_issue(Some(
                    "Cloud settings could not be loaded. Check the server logs, then retry."
                        .to_string(),
                ));
                self.start_flusher();
                return Ok(());
            }
        };
        if let Err(state_error) = self.link.set_feature_switches(CloudFeatureSwitches {
            telemetry: settings.cloud.telemetry_enabled,
            backups: settings.cloud.backups_enabled,
            notifications: settings.cloud.notifications_enabled,
        }) {
            tracing::error!(%state_error, "Cloud consent state could not be applied; outbound operations blocked");
            self.link.block_outbound(
                "telemetry consent state could not be persisted; repair the Cloud link state",
            );
            self.set_configuration_issue(Some(
                "Cloud telemetry consent could not be persisted. Check the server logs before reconnecting."
                    .to_string(),
            ));
            self.start_flusher();
            return Ok(());
        }
        let backend = match parse_backend(
            &settings.cloud.backend_url,
            self.allow_loopback_development || self.link.allows_loopback_development(),
        ) {
            Ok(backend) => backend,
            Err(error) => {
                tracing::error!(%error, "Cloud backend configuration invalid; Cloud integration disabled");
                if let Err(state_error) = self
                    .link
                    .set_feature_switches(CloudFeatureSwitches::default())
                {
                    tracing::error!(%state_error, "could not purge Cloud telemetry while disabling invalid integration");
                }
                self.link
                    .block_outbound("the configured backend URL is invalid; update Cloud settings");
                self.set_configuration_issue(Some(format!(
                    "Cloud backend configuration is invalid: {error}. Update it in Cloud settings."
                )));
                self.start_flusher();
                return Ok(());
            }
        };
        if let Err(error) = self.link.configure(backend) {
            if matches!(
                error,
                temps_cloud_client::state::StateError::UnreadableStateBlocksMutation { .. }
            ) {
                tracing::error!(%error, "Cloud service started with unreadable link state");
            } else {
                tracing::error!(%error, "Cloud link configuration could not be applied; Cloud integration disabled");
                if let Err(state_error) = self
                    .link
                    .set_feature_switches(CloudFeatureSwitches::default())
                {
                    tracing::error!(%state_error, "could not purge Cloud telemetry after configuration failure");
                }
                self.link.block_outbound(
                    "the configured Cloud link could not be applied; check server logs",
                );
                self.set_configuration_issue(Some(
                    "Cloud link configuration could not be applied. Check the server logs and Cloud settings."
                        .to_string(),
                ));
            }
        } else {
            self.set_configuration_issue(None);
        }
        self.start_flusher();
        Ok(())
    }

    pub async fn capability(&self) -> CloudCapability {
        match self.config.get_settings().await {
            Ok(settings) => {
                match parse_backend(
                    &settings.cloud.backend_url,
                    self.allow_loopback_development || self.link.allows_loopback_development(),
                ) {
                    Ok(_) => CloudCapability {
                        configured: true,
                        reason: None,
                        setup_path: SETUP_PATH.to_string(),
                    },
                    Err(error) => CloudCapability {
                        configured: false,
                        reason: Some(error.to_string()),
                        setup_path: SETUP_PATH.to_string(),
                    },
                }
            }
            Err(error) => CloudCapability {
                configured: false,
                reason: Some(format!("Could not load settings: {error}")),
                setup_path: SETUP_PATH.to_string(),
            },
        }
    }

    pub async fn status(&self) -> Result<CloudStatus, CloudServiceError> {
        let (backend_url, settings_issue) = match self.config.get_settings().await {
            Ok(settings) => (settings.cloud.backend_url, None),
            Err(error) => {
                tracing::error!(%error, "Could not refresh Cloud settings for status");
                (
                    String::new(),
                    Some(
                        "Cloud settings could not be loaded. Check the server logs, then retry."
                            .to_string(),
                    ),
                )
            }
        };
        let link_status = self.link.status();
        let issue = settings_issue.or_else(|| self.configuration_issue());
        let (status, status_message) = if matches!(
            link_status,
            temps_cloud_client::LinkStatus::StateUnreadable { .. }
        ) {
            (status_name(&link_status).to_string(), link_status.message())
        } else if let Some(issue) = issue {
            ("configuration_invalid".to_string(), issue)
        } else {
            (status_name(&link_status).to_string(), link_status.message())
        };
        let health = self.link.health();
        let switches = self.link.feature_switches();
        Ok(CloudStatus {
            status,
            status_message,
            health: health_name(&health).to_string(),
            health_message: health.message(),
            instance_id: self.link.instance_id(),
            account_email: self.link.account_email(),
            spooled_spans: self.link.spooled(),
            backend_url,
            telemetry_enabled: switches.telemetry,
            backups_enabled: switches.backups,
            notifications_enabled: switches.notifications,
        })
    }

    pub async fn ai_capability(&self) -> Result<CloudAiCapability, CloudServiceError> {
        match self.link.managed_ai_capability().await {
            Ok(capability) => Ok(CloudAiCapability {
                configured: capability.configured,
                reason: capability.reason,
                setup_path: capability.setup_path,
                model: capability.managed_model,
            }),
            Err(CloudError::NotEnrolled) => Ok(CloudAiCapability {
                configured: false,
                reason: Some("Link this instance to use managed AI.".to_string()),
                setup_path: SETUP_PATH.to_string(),
                model: None,
            }),
            Err(error) => Err(CloudServiceError::Client(error)),
        }
    }

    pub async fn update_feature_switches(
        &self,
        switches: CloudFeatureSwitches,
    ) -> Result<CloudStatus, CloudServiceError> {
        self.config
            .update_cloud_features(switches.telemetry, switches.backups, switches.notifications)
            .await?;
        if let Err(error) = self.link.set_feature_switches(switches) {
            self.link.block_outbound(
                "telemetry consent state could not be persisted; repair the Cloud link state",
            );
            return Err(CloudServiceError::State(error));
        }
        self.status().await
    }

    pub async fn enroll(&self, code: &str) -> Result<CloudStatus, CloudServiceError> {
        let settings = self.config.get_settings().await?;
        let backend = parse_backend(
            &settings.cloud.backend_url,
            self.allow_loopback_development || self.link.allows_loopback_development(),
        )
        .map_err(|error| CloudServiceError::InvalidBackend {
            reason: error.to_string(),
        })?;
        self.link
            .configure(backend)
            .map_err(CloudServiceError::State)?;
        self.set_configuration_issue(None);
        self.link
            .set_feature_switches(CloudFeatureSwitches {
                telemetry: settings.cloud.telemetry_enabled,
                backups: settings.cloud.backups_enabled,
                notifications: settings.cloud.notifications_enabled,
            })
            .map_err(CloudServiceError::State)?;
        self.link
            .enroll(code)
            .await
            .map_err(CloudServiceError::Client)?;
        self.status().await
    }

    pub async fn disconnect(&self) -> Result<CloudStatus, CloudServiceError> {
        match self.link.revoke().await {
            Ok(()) | Err(CloudError::CredentialRejected) => {}
            Err(error) => return Err(CloudServiceError::Client(error)),
        }
        self.link.disconnect().map_err(CloudServiceError::State)?;
        self.status().await
    }

    pub async fn send_notification(
        &self,
        request: &ManagedNotificationRequest,
    ) -> Result<ManagedNotificationAccepted, CloudServiceError> {
        self.link
            .send_notification(request)
            .await
            .map_err(CloudServiceError::Client)
    }

    pub async fn shutdown(&self) {
        let _ = self.cancel.send(true);
        let task = self
            .task
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(task) = task {
            await_task_shutdown(task, "managed telemetry mirror", SHUTDOWN_TASK_TIMEOUT).await;
        }
        let backup_task = self
            .backup_task
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(task) = backup_task {
            await_task_shutdown(task, "Cloud backup mirror", SHUTDOWN_TASK_TIMEOUT).await;
        }
    }
}

async fn await_task_shutdown(
    mut task: tokio::task::JoinHandle<()>,
    task_name: &'static str,
    timeout: Duration,
) {
    match tokio::time::timeout(timeout, &mut task).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(%error, task_name, "Cloud task did not shut down cleanly"),
        Err(_) => {
            tracing::warn!(
                task_name,
                timeout_ms = timeout.as_millis(),
                "Cloud task exceeded shutdown deadline; cancelling in-flight network work"
            );
            task.abort();
            let _ = task.await;
        }
    }
}

fn parse_backend(value: &str, allow_loopback_development: bool) -> Result<BackendUrl, CloudError> {
    if allow_loopback_development {
        BackendUrl::loopback_development(value)
    } else {
        BackendUrl::production(value)
    }
}

fn status_name(status: &temps_cloud_client::LinkStatus) -> &'static str {
    match status {
        temps_cloud_client::LinkStatus::StateUnreadable { .. } => "state_unreadable",
        temps_cloud_client::LinkStatus::NotConfigured => "not_configured",
        temps_cloud_client::LinkStatus::AwaitingEnrollment { .. } => "awaiting_enrollment",
        temps_cloud_client::LinkStatus::Linked { .. } => "linked",
        temps_cloud_client::LinkStatus::CredentialRejected { .. } => "credential_rejected",
    }
}

fn health_name(health: &temps_cloud_client::MirrorHealth) -> &'static str {
    match health {
        temps_cloud_client::MirrorHealth::Healthy => "healthy",
        temps_cloud_client::MirrorHealth::Buffering { .. } => "buffering",
        temps_cloud_client::MirrorHealth::Dropping { .. } => "dropping",
        temps_cloud_client::MirrorHealth::Degraded { .. } => "degraded",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_cloud_configuration_rejects_plain_http() {
        assert!(parse_backend("http://cloud.example.com", false).is_err());
        assert!(parse_backend("http://cloud.example.com", true).is_err());
    }

    #[test]
    fn loopback_http_requires_the_explicit_development_gate() {
        assert!(parse_backend("http://127.0.0.1:19200", false).is_err());
        assert!(parse_backend("http://127.0.0.1:19200", true).is_ok());
    }

    #[test]
    fn status_names_are_stable_api_values() {
        assert_eq!(
            status_name(&temps_cloud_client::LinkStatus::StateUnreadable {
                state_path: "/data/cloud-link/state.json".to_string(),
            }),
            "state_unreadable"
        );
        assert_eq!(
            status_name(&temps_cloud_client::LinkStatus::Linked {
                base_url: "https://cloud.test".to_string(),
            }),
            "linked"
        );
        assert_eq!(
            health_name(&temps_cloud_client::MirrorHealth::Buffering {
                spooled: 1,
                reason: "offline".to_string(),
            }),
            "buffering"
        );
    }

    #[tokio::test]
    async fn shutdown_aborts_in_flight_work_after_the_deadline() {
        let task = tokio::spawn(std::future::pending::<()>());
        tokio::time::timeout(
            Duration::from_secs(1),
            await_task_shutdown(task, "test mirror", Duration::from_millis(10)),
        )
        .await
        .unwrap();
    }
}
