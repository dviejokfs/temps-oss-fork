//! HTTP handlers for OTel span attribute facet management.
//!
//! Facets allow admins to mark any OTel attribute key as "faceted", which pre-populates
//! a dedicated slot column in ClickHouse and enables bloom-filter-accelerated filtering
//! for that key — replacing the default full-JSON-parse `JSONExtractString` predicate.
//!
//! ## Endpoints
//!
//! - `GET  /otel/facets`          — list all registered facets (OtelRead)
//! - `POST /otel/facets`          — create a facet for an attribute key (OtelWrite)
//! - `DELETE /otel/facets/{key}`  — remove a facet by attribute key (OtelWrite)
//!
//! ## CLI parity
//!
//! TODO: CLI parity for these endpoints is deferred. The equivalent commands
//! (`temps otel facets list`, `temps otel facets create`, `temps otel facets delete`)
//! should be added to `apps/temps-cli/src/commands/` following the `otel-forward`
//! pattern for plugin-only routes (hand-written local request/response types + the
//! shared `client` object). See CLAUDE.md "Regenerating the OpenAPI clients" §
//! "Never add a plugin-only route or schema to `apps/temps-cli/openapi.json`".

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use tracing::error;
use utoipa::ToSchema;

use crate::handlers::audit::{FacetCreatedAudit, FacetDeletedAudit};
use crate::services::facet_service::FacetError;
use crate::services::FacetInfo;
use crate::OtelAppState;
use temps_auth::{permission_guard, RequireAuth};
use temps_core::problemdetails::{self, Problem};
use temps_core::{AuditContext, ProblemDetails, RequestMetadata};

// ── Error conversion ─────────────────────────────────────────────────────────

impl From<FacetError> for Problem {
    fn from(error: FacetError) -> Self {
        match error {
            FacetError::AlreadyFaceted { .. } => problemdetails::new(StatusCode::CONFLICT)
                .with_title("Facet Already Registered")
                .with_detail(error.to_string()),

            FacetError::CapacityExceeded { .. } => problemdetails::new(StatusCode::CONFLICT)
                .with_title("Facet Capacity Exceeded")
                .with_detail(error.to_string()),

            FacetError::NotFound { .. } => problemdetails::new(StatusCode::NOT_FOUND)
                .with_title("Facet Not Found")
                .with_detail(error.to_string()),

            FacetError::Validation { .. } => problemdetails::new(StatusCode::BAD_REQUEST)
                .with_title("Validation Error")
                .with_detail(error.to_string()),

            FacetError::Database(_) => problemdetails::new(StatusCode::INTERNAL_SERVER_ERROR)
                .with_title("Internal Server Error")
                .with_detail(error.to_string()),

            FacetError::Storage { .. } => problemdetails::new(StatusCode::INTERNAL_SERVER_ERROR)
                .with_title("Storage Error")
                .with_detail(error.to_string()),
        }
    }
}

// ── Request / response DTOs ───────────────────────────────────────────────────

/// Request body for registering a new facet.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateFacetRequest {
    /// The OTel attribute key to facet (e.g. `enduser.id`, `galachain.contract`).
    /// Must be non-empty, ≤200 characters, and not already registered.
    pub attribute_key: String,
}

/// Response body for facet list.
#[derive(Debug, Serialize, ToSchema)]
pub struct FacetsResponse {
    pub data: Vec<FacetInfo>,
}

// ── Handlers ─────────────────────────────────────────────────────────────────

/// List all registered OTel span attribute facets.
///
/// Returns all facets registered on this platform (newest first). Since the
/// `spans` ClickHouse table is platform-global, facets are also platform-global.
#[utoipa::path(
    tag = "OTel Facets",
    get,
    path = "/otel/facets",
    responses(
        (status = 200, description = "Registered facets", body = FacetsResponse),
        (status = 401, description = "Unauthorized", body = ProblemDetails),
        (status = 403, description = "Insufficient permissions", body = ProblemDetails),
        (status = 500, description = "Internal server error", body = ProblemDetails),
    ),
    security(("bearer_auth" = []))
)]
pub async fn list_facets(
    RequireAuth(auth): RequireAuth,
    State(state): State<OtelAppState>,
) -> Result<impl IntoResponse, Problem> {
    permission_guard!(auth, OtelRead);

    let data = state.facet_service.list_facets().await?;
    Ok(Json(FacetsResponse { data }))
}

/// Register an OTel attribute key as a facet.
///
/// Assigns the key to the lowest available slot column (1..=20) in the
/// ClickHouse `spans` table, inserts the mapping into Postgres, and runs a
/// backfill mutation so existing spans that carry this attribute are populated
/// in the slot column. At 500M+ row scale this mutation would be expensive and
/// should be tracked asynchronously; for the current demo dataset it is near-instant.
#[utoipa::path(
    tag = "OTel Facets",
    post,
    path = "/otel/facets",
    request_body = CreateFacetRequest,
    responses(
        (status = 201, description = "Facet registered", body = FacetInfo),
        (status = 400, description = "Validation error", body = ProblemDetails),
        (status = 401, description = "Unauthorized", body = ProblemDetails),
        (status = 403, description = "Insufficient permissions", body = ProblemDetails),
        (status = 409, description = "Already registered or capacity exceeded", body = ProblemDetails),
        (status = 500, description = "Internal server error", body = ProblemDetails),
    ),
    security(("bearer_auth" = []))
)]
pub async fn create_facet(
    RequireAuth(auth): RequireAuth,
    State(state): State<OtelAppState>,
    Extension(metadata): Extension<RequestMetadata>,
    Json(request): Json<CreateFacetRequest>,
) -> Result<impl IntoResponse, Problem> {
    permission_guard!(auth, OtelWrite);

    let info = state
        .facet_service
        .create_facet(request.attribute_key.clone(), Some(auth.user_id()))
        .await?;

    let audit = FacetCreatedAudit {
        context: AuditContext {
            user_id: auth.user_id(),
            ip_address: Some(metadata.ip_address.clone()),
            user_agent: metadata.user_agent.clone(),
        },
        attribute_key: info.attribute_key.clone(),
        slot: info.slot,
    };
    if let Err(e) = state.audit_service.create_audit_log(&audit).await {
        error!("Failed to create audit log for facet creation: {}", e);
    }

    Ok((StatusCode::CREATED, Json(info)))
}

/// Remove a registered OTel span attribute facet.
///
/// Clears the corresponding ClickHouse slot column for all existing spans
/// (to prevent stale data leaking into a future facet that reuses the slot),
/// then removes the Postgres mapping. The slot becomes available for reuse.
#[utoipa::path(
    tag = "OTel Facets",
    delete,
    path = "/otel/facets/{key}",
    params(
        ("key" = String, Path, description = "The OTel attribute key (URL-encoded)"),
    ),
    responses(
        (status = 204, description = "Facet deleted"),
        (status = 401, description = "Unauthorized", body = ProblemDetails),
        (status = 403, description = "Insufficient permissions", body = ProblemDetails),
        (status = 404, description = "Facet not found", body = ProblemDetails),
        (status = 500, description = "Internal server error", body = ProblemDetails),
    ),
    security(("bearer_auth" = []))
)]
pub async fn delete_facet(
    RequireAuth(auth): RequireAuth,
    State(state): State<OtelAppState>,
    Extension(metadata): Extension<RequestMetadata>,
    Path(key): Path<String>,
) -> Result<impl IntoResponse, Problem> {
    permission_guard!(auth, OtelWrite);

    state.facet_service.delete_facet(&key).await?;

    let audit = FacetDeletedAudit {
        context: AuditContext {
            user_id: auth.user_id(),
            ip_address: Some(metadata.ip_address.clone()),
            user_agent: metadata.user_agent.clone(),
        },
        attribute_key: key.clone(),
    };
    if let Err(e) = state.audit_service.create_audit_log(&audit).await {
        error!("Failed to create audit log for facet deletion: {}", e);
    }

    Ok(StatusCode::NO_CONTENT)
}
