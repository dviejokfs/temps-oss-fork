use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc};

use temps_cloud_client::CloudLink;
use temps_config::ConfigService;
use temps_core::plugin::{
    PluginContext, PluginError, PluginRoutes, ServiceRegistrationContext, TempsPlugin,
};
use utoipa::{openapi::OpenApi, OpenApi as _};

use crate::{cloud_routes, CloudApiDoc, CloudService};

pub struct CloudPlugin {
    data_dir: PathBuf,
    agent_version: String,
    allow_loopback_development: bool,
}

impl CloudPlugin {
    pub fn new(data_dir: PathBuf, agent_version: impl Into<String>) -> Self {
        let allow_loopback_development = std::env::var("TEMPS_CLOUD_ALLOW_LOOPBACK")
            .is_ok_and(|value| matches!(value.as_str(), "1" | "true" | "TRUE"));
        Self {
            data_dir,
            agent_version: agent_version.into(),
            allow_loopback_development,
        }
    }
}

impl TempsPlugin for CloudPlugin {
    fn name(&self) -> &'static str {
        "cloud"
    }

    fn register_services<'a>(
        &'a self,
        context: &'a ServiceRegistrationContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>> {
        Box::pin(async move {
            let config = context.require_service::<ConfigService>();
            let link = if self.allow_loopback_development {
                Arc::new(CloudLink::load_for_loopback_development(
                    self.data_dir.clone(),
                    self.agent_version.clone(),
                ))
            } else {
                Arc::new(CloudLink::load(
                    self.data_dir.clone(),
                    self.agent_version.clone(),
                ))
            };
            let service = Arc::new(CloudService::new(
                link.clone(),
                config,
                self.allow_loopback_development,
            ));
            context.register_service(link);
            context.register_service(service);
            Ok(())
        })
    }

    fn initialize_plugin_services<'a>(
        &'a self,
        context: &'a PluginContext,
    ) -> Pin<Box<dyn Future<Output = Result<(), PluginError>> + Send + 'a>> {
        Box::pin(async move {
            let service = context.require_service::<CloudService>();
            service
                .initialize()
                .await
                .map_err(|error| PluginError::InitializationFailed(error.to_string()))?;
            service.start_backup_mirror(
                context.require_service::<sea_orm::DatabaseConnection>(),
                context.require_service::<temps_core::EncryptionService>(),
            );
            Ok(())
        })
    }

    fn configure_routes(&self, context: &PluginContext) -> Option<PluginRoutes> {
        Some(PluginRoutes::new(cloud_routes(
            context.require_service::<CloudService>(),
            context.require_service::<dyn temps_core::AuditLogger>(),
        )))
    }

    fn openapi_schema(&self) -> Option<OpenApi> {
        Some(CloudApiDoc::openapi())
    }
}
