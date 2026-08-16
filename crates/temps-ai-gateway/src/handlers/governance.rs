use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, put},
    Extension, Json, Router,
};
use serde::{Deserialize, Serialize};
use temps_auth::{permission_guard, RequireAuth};
use temps_core::audit::{AuditContext, AuditOperation};
use temps_core::problemdetails::{Problem, ProblemDetails};
use temps_core::RequestMetadata;
use utoipa::{OpenApi, ToSchema};

use crate::handlers::types::AiGatewayAppState;

#[derive(OpenApi)]
#[openapi(
    paths(list_governance_configs, upsert_governance_config, delete_governance_config),
    components(schemas(GovernanceConfigResponse, UpsertGovernanceConfigRequest)),
    info(
        title = "AI Gateway Governance API",
        description = "Configure instance, project, environment, and deployment-token AI gateway limits",
        version = "1.0.0"
    ),
    tags((name = "AI Gateway Governance", description = "AI model, rate, and budget policy"))
)]
pub struct AiGatewayGovernanceApiDoc;

pub fn configure_governance_routes() -> Router<Arc<AiGatewayAppState>> {
    Router::new()
        .route("/ai/governance", get(list_governance_configs))
        .route(
            "/ai/governance/{scope}",
            put(upsert_governance_config).delete(delete_governance_config),
        )
}

#[derive(Debug, Serialize, ToSchema)]
pub struct GovernanceConfigResponse {
    pub id: i32,
    pub scope: String,
    pub allowed_models: Option<Vec<String>>,
    pub max_requests_per_minute: Option<i64>,
    pub max_cost_per_month_microcents: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
}

impl From<temps_entities::ai_gateway_config::Model> for GovernanceConfigResponse {
    fn from(model: temps_entities::ai_gateway_config::Model) -> Self {
        Self {
            id: model.id,
            scope: model.scope,
            allowed_models: model.allowed_models.and_then(|value| {
                value.as_array().map(|models| {
                    models
                        .iter()
                        .filter_map(|model| model.as_str().map(String::from))
                        .collect()
                })
            }),
            max_requests_per_minute: model.max_requests_per_minute,
            max_cost_per_month_microcents: model.max_cost_per_month_microcents,
            created_at: model.created_at.to_rfc3339(),
            updated_at: model.updated_at.to_rfc3339(),
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpsertGovernanceConfigRequest {
    /// NULL allows every model; an empty array denies every model.
    pub allowed_models: Option<Vec<String>>,
    /// NULL disables the request-rate limit for this scope.
    pub max_requests_per_minute: Option<i64>,
    /// NULL disables the operator-funded monthly budget for this scope.
    pub max_cost_per_month_microcents: Option<i64>,
}

#[derive(Debug, Serialize)]
struct GovernanceConfigAudit {
    context: AuditContext,
    action: String,
    scope: String,
}

impl AuditOperation for GovernanceConfigAudit {
    fn operation_type(&self) -> String {
        format!("AI_GATEWAY_GOVERNANCE_{}", self.action)
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

    fn serialize(&self) -> temps_core::anyhow::Result<String> {
        serde_json::to_string(self).map_err(|error| {
            temps_core::anyhow::anyhow!(
                "Failed to serialize AI gateway governance audit for scope '{}': {}",
                self.scope,
                error
            )
        })
    }
}

#[utoipa::path(
    tag = "AI Gateway Governance",
    get,
    path = "/ai/governance",
    responses(
        (status = 200, body = Vec<GovernanceConfigResponse>),
        (status = 401, body = ProblemDetails),
        (status = 403, body = ProblemDetails),
    ),
    security(("bearer_auth" = []))
)]
async fn list_governance_configs(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<AiGatewayAppState>>,
) -> Result<impl IntoResponse, Problem> {
    // Governance policies expose budget and allowlist details and therefore
    // use the same operator-only permission as mutations.
    permission_guard!(auth, AiGatewayWrite);
    let configs = app_state.governance_service.list_configs().await?;
    Ok(Json(
        configs
            .into_iter()
            .map(GovernanceConfigResponse::from)
            .collect::<Vec<_>>(),
    ))
}

#[utoipa::path(
    tag = "AI Gateway Governance",
    put,
    path = "/ai/governance/{scope}",
    params(("scope" = String, Path, description = "instance, project:<id>, environment:<id>, or token:<id>")),
    request_body = UpsertGovernanceConfigRequest,
    responses(
        (status = 200, body = GovernanceConfigResponse),
        (status = 400, body = ProblemDetails),
        (status = 401, body = ProblemDetails),
        (status = 403, body = ProblemDetails),
    ),
    security(("bearer_auth" = []))
)]
async fn upsert_governance_config(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<AiGatewayAppState>>,
    Extension(metadata): Extension<RequestMetadata>,
    Path(scope): Path<String>,
    Json(request): Json<UpsertGovernanceConfigRequest>,
) -> Result<impl IntoResponse, Problem> {
    permission_guard!(auth, AiGatewayWrite);

    let allowed_models = request
        .allowed_models
        .map(|models| serde_json::json!(models));
    let config = app_state
        .governance_service
        .upsert_config(
            &scope,
            allowed_models,
            request.max_requests_per_minute,
            request.max_cost_per_month_microcents,
        )
        .await?;

    audit_change(&app_state, &auth, &metadata, "UPSERTED", &scope).await;
    Ok(Json(GovernanceConfigResponse::from(config)))
}

#[utoipa::path(
    tag = "AI Gateway Governance",
    delete,
    path = "/ai/governance/{scope}",
    params(("scope" = String, Path, description = "instance, project:<id>, environment:<id>, or token:<id>")),
    responses(
        (status = 204),
        (status = 401, body = ProblemDetails),
        (status = 403, body = ProblemDetails),
        (status = 404, body = ProblemDetails),
    ),
    security(("bearer_auth" = []))
)]
async fn delete_governance_config(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<AiGatewayAppState>>,
    Extension(metadata): Extension<RequestMetadata>,
    Path(scope): Path<String>,
) -> Result<impl IntoResponse, Problem> {
    permission_guard!(auth, AiGatewayWrite);
    app_state.governance_service.delete_config(&scope).await?;
    audit_change(&app_state, &auth, &metadata, "DELETED", &scope).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn audit_change(
    app_state: &AiGatewayAppState,
    auth: &temps_auth::AuthContext,
    metadata: &RequestMetadata,
    action: &str,
    scope: &str,
) {
    let audit = GovernanceConfigAudit {
        context: AuditContext {
            user_id: auth.user_id(),
            ip_address: Some(metadata.ip_address.clone()),
            user_agent: metadata.user_agent.clone(),
        },
        action: action.to_string(),
        scope: scope.to_string(),
    };
    if let Err(error) = app_state.audit_service.create_audit_log(&audit).await {
        tracing::error!(
            error = %error,
            scope,
            action,
            "Failed to create AI gateway governance audit log"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_response_preserves_scope_and_limits() {
        let response = GovernanceConfigResponse::from(temps_entities::ai_gateway_config::Model {
            id: 1,
            scope: "project:7".to_string(),
            allowed_models: Some(serde_json::json!(["gpt-5-mini"])),
            max_requests_per_minute: Some(60),
            max_cost_per_month_microcents: Some(1_000_000),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            provider_type: "gateway".to_string(),
            agent_cli_provider_id: None,
            interactive_bridge_enabled: false,
            summary_provider_id: None,
            summary_model: None,
            summary_thinking_level: None,
        });
        assert_eq!(response.scope, "project:7");
        assert_eq!(
            response.allowed_models,
            Some(vec!["gpt-5-mini".to_string()])
        );
        assert_eq!(response.max_requests_per_minute, Some(60));
    }
}
