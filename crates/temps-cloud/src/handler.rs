use std::sync::Arc;

use axum::{
    extract::{Extension, State},
    http::StatusCode,
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use temps_auth::{permission_guard, RequireAuth};
use temps_core::{
    error_builder::ErrorBuilder, problemdetails::Problem, AuditContext, AuditLogger,
    AuditOperation, RequestMetadata,
};
use utoipa::{OpenApi, ToSchema};

use crate::{CloudCapability, CloudService, CloudServiceError, CloudStatus};

#[derive(Clone)]
pub struct CloudState {
    service: Arc<CloudService>,
    audit: Arc<dyn AuditLogger>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct EnrollCloudRequest {
    #[schema(min_length = 1, example = "ABCD-EFGH")]
    pub enrollment_code: String,
}

#[derive(Debug, Serialize)]
struct CloudLinkAudit {
    context: AuditContext,
    action: &'static str,
}

impl AuditOperation for CloudLinkAudit {
    fn operation_type(&self) -> String {
        self.action.to_string()
    }
    fn user_id(&self) -> Option<i32> {
        Some(self.context.user_id)
    }
    fn ip_address(&self) -> Option<String> {
        self.context.ip_address.clone()
    }
    fn user_agent(&self) -> &str {
        &self.context.user_agent
    }
    fn serialize(&self) -> anyhow::Result<String> {
        serde_json::to_string(self).map_err(Into::into)
    }
}

fn problem(error: CloudServiceError) -> Problem {
    let status = match &error {
        CloudServiceError::Client(temps_cloud_client::CloudError::EnrollmentRefused { .. }) => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        CloudServiceError::Client(temps_cloud_client::CloudError::Unreachable { .. }) => {
            StatusCode::SERVICE_UNAVAILABLE
        }
        CloudServiceError::Client(temps_cloud_client::CloudError::CredentialRejected) => {
            StatusCode::UNAUTHORIZED
        }
        CloudServiceError::Client(temps_cloud_client::CloudError::NotEnrolled) => {
            StatusCode::CONFLICT
        }
        CloudServiceError::Client(
            temps_cloud_client::CloudError::Rejected { .. }
            | temps_cloud_client::CloudError::InvalidAcknowledgement { .. },
        ) => StatusCode::BAD_GATEWAY,
        CloudServiceError::Configuration(_)
        | CloudServiceError::InvalidBackend { .. }
        | CloudServiceError::State(_)
        | CloudServiceError::Client(
            temps_cloud_client::CloudError::InvalidBackendUrl { .. }
            | temps_cloud_client::CloudError::ClientConfiguration { .. },
        ) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    ErrorBuilder::new(status)
        .type_("https://temps.sh/probs/cloud-link")
        .title("Managed control plane error")
        .detail(error.to_string())
        .build()
}

#[utoipa::path(get, path = "/cloud/capability", tag = "Cloud", responses((status = 200, body = CloudCapability)), security(("bearer_auth" = [])))]
async fn get_cloud_capability(
    RequireAuth(auth): RequireAuth,
    State(state): State<CloudState>,
) -> Result<Json<CloudCapability>, Problem> {
    permission_guard!(auth, SettingsRead);
    Ok(Json(state.service.capability().await))
}

#[utoipa::path(get, path = "/cloud/status", tag = "Cloud", responses((status = 200, body = CloudStatus)), security(("bearer_auth" = [])))]
async fn get_cloud_status(
    RequireAuth(auth): RequireAuth,
    State(state): State<CloudState>,
) -> Result<Json<CloudStatus>, Problem> {
    permission_guard!(auth, SettingsRead);
    state.service.status().await.map(Json).map_err(problem)
}

#[utoipa::path(post, path = "/cloud/enroll", tag = "Cloud", request_body = EnrollCloudRequest, responses((status = 200, body = CloudStatus)), security(("bearer_auth" = [])))]
async fn enroll_cloud(
    RequireAuth(auth): RequireAuth,
    State(state): State<CloudState>,
    Extension(metadata): Extension<RequestMetadata>,
    Json(request): Json<EnrollCloudRequest>,
) -> Result<Json<CloudStatus>, Problem> {
    permission_guard!(auth, SettingsWrite);
    if request.enrollment_code.trim().is_empty() {
        return Err(ErrorBuilder::new(StatusCode::BAD_REQUEST)
            .detail("Enrollment code cannot be empty")
            .build());
    }
    let result = state
        .service
        .enroll(&request.enrollment_code)
        .await
        .map_err(problem)?;
    audit(&state, &auth, &metadata, "CLOUD_LINK_CONNECTED").await;
    Ok(Json(result))
}

#[utoipa::path(delete, path = "/cloud", tag = "Cloud", responses((status = 200, body = CloudStatus)), security(("bearer_auth" = [])))]
async fn disconnect_cloud(
    RequireAuth(auth): RequireAuth,
    State(state): State<CloudState>,
    Extension(metadata): Extension<RequestMetadata>,
) -> Result<Json<CloudStatus>, Problem> {
    permission_guard!(auth, SettingsWrite);
    let result = state.service.disconnect().await.map_err(problem)?;
    audit(&state, &auth, &metadata, "CLOUD_LINK_DISCONNECTED").await;
    Ok(Json(result))
}

async fn audit(
    state: &CloudState,
    auth: &temps_auth::AuthContext,
    metadata: &RequestMetadata,
    action: &'static str,
) {
    let event = CloudLinkAudit {
        context: AuditContext {
            user_id: auth.user_id(),
            ip_address: Some(metadata.ip_address.clone()),
            user_agent: metadata.user_agent.clone(),
        },
        action,
    };
    if let Err(error) = state.audit.create_audit_log(&event).await {
        tracing::error!(%error, action, "failed to record managed control-plane audit event");
    }
}

pub fn cloud_routes(service: Arc<CloudService>, audit: Arc<dyn AuditLogger>) -> Router {
    Router::new()
        .route("/cloud/capability", get(get_cloud_capability))
        .route("/cloud/status", get(get_cloud_status))
        .route("/cloud/enroll", post(enroll_cloud))
        .route("/cloud", delete(disconnect_cloud))
        .with_state(CloudState { service, audit })
}

#[derive(OpenApi)]
#[openapi(
    paths(get_cloud_capability, get_cloud_status, enroll_cloud, disconnect_cloud),
    components(schemas(CloudCapability, CloudStatus, EnrollCloudRequest))
)]
pub struct CloudApiDoc;
