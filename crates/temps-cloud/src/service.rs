use std::sync::{Arc, Mutex};

use serde::Serialize;
use temps_cloud_client::{BackendUrl, CloudError, CloudLink};
use temps_cloud_protocol::{ManagedNotificationAccepted, ManagedNotificationRequest};
use temps_config::{ConfigService, ConfigServiceError};
use thiserror::Error;
use tokio::sync::watch;
use utoipa::ToSchema;
use uuid::Uuid;

const SETUP_PATH: &str = "/settings/cloud";

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
}

pub struct CloudService {
    link: Arc<CloudLink>,
    config: Arc<ConfigService>,
    cancel: watch::Sender<bool>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    backup_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    allow_loopback_development: bool,
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

    pub async fn initialize(&self) -> Result<(), CloudServiceError> {
        let settings = self.config.get_settings().await?;
        let backend = parse_backend(&settings.cloud.backend_url, self.allow_loopback_development)
            .map_err(|error| CloudServiceError::InvalidBackend {
            reason: error.to_string(),
        })?;
        self.link
            .configure(backend)
            .map_err(CloudServiceError::State)?;

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
        Ok(())
    }

    pub async fn capability(&self) -> CloudCapability {
        match self.config.get_settings().await {
            Ok(settings) => {
                match parse_backend(&settings.cloud.backend_url, self.allow_loopback_development) {
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
        let settings = self.config.get_settings().await?;
        let status = self.link.status();
        let health = self.link.health();
        Ok(CloudStatus {
            status: status_name(&status).to_string(),
            status_message: status.message(),
            health: health_name(&health).to_string(),
            health_message: health.message(),
            instance_id: self.link.instance_id(),
            account_email: self.link.account_email(),
            spooled_spans: self.link.spooled(),
            backend_url: settings.cloud.backend_url,
        })
    }

    pub async fn enroll(&self, code: &str) -> Result<CloudStatus, CloudServiceError> {
        let settings = self.config.get_settings().await?;
        let backend = parse_backend(&settings.cloud.backend_url, self.allow_loopback_development)
            .map_err(|error| CloudServiceError::InvalidBackend {
            reason: error.to_string(),
        })?;
        self.link
            .configure(backend)
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
            if let Err(error) = task.await {
                tracing::warn!(%error, "managed telemetry mirror task did not shut down cleanly");
            }
        }
        let backup_task = self
            .backup_task
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(task) = backup_task {
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
}
