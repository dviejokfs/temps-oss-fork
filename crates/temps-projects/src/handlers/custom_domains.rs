use super::types::{
    AppState, CustomDomainRequest, CustomDomainResponse, CustomDomainWithInfo, DomainEnvironment,
    DomainInfo, ListCustomDomainsResponse, ReassignCustomDomainRequest, UpdateCustomDomainRequest,
};
use super::{
    audit::AuditContext, audit::CustomDomainReassignedAudit,
    audit::CustomDomainReassignmentRequestedAudit,
};
use crate::services::custom_domains::CustomDomainError;
use axum::{
    extract::{Extension, Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use sea_orm::EntityTrait;
use std::sync::Arc;
use temps_auth::{
    deny_deployment_token, permission_guard, project_access_guard, project_permission_guard,
    RequireAuth,
};
use temps_core::problemdetails;
use temps_core::problemdetails::Problem;
use temps_core::RequestMetadata;
use temps_entities::{domains, environments, project_custom_domains};
use tracing::{error, info};
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(
    paths(
        create_custom_domain,
        get_custom_domain,
        list_custom_domains_for_project,
        update_custom_domain,
        get_custom_domain_by_hostname,
        reassign_custom_domain,
        delete_custom_domain,
        link_custom_domain_to_certificate,
    ),
    components(
        schemas(
            CustomDomainRequest,
            CustomDomainResponse,
            UpdateCustomDomainRequest,
            ReassignCustomDomainRequest,
            ListCustomDomainsResponse,
        )
    ),
    tags((name = "Custom Domains", description = "Custom domain management for projects"))
)]
pub struct CustomDomainsApiDoc;

/// Create a custom domain for a project
#[utoipa::path(
    post,
    path = "/{project_id}/custom-domains",
    request_body = CustomDomainRequest,
    responses(
        (status = 201, description = "Custom domain created successfully", body = CustomDomainResponse),
        (status = 400, description = "Invalid input"),
        (status = 401, description = "Unauthorized"),
        (status = 409, description = "Domain already exists"),
        (status = 500, description = "Internal server error")
    ),
    params(
        ("project_id" = i32, Path, description = "Project ID")
    ),
    tag = "Custom Domains",
    security(
        ("bearer_auth" = [])
    )
)]
pub async fn create_custom_domain(
    RequireAuth(auth): RequireAuth,
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<i32>,
    Json(request): Json<CustomDomainRequest>,
) -> Result<impl IntoResponse, Problem> {
    project_permission_guard!(
        auth,
        ProjectsWrite,
        project_id,
        state.project_access_checker
    );

    info!(
        "Creating custom domain: {} for project: {}",
        request.domain, project_id
    );

    let custom_domain = state
        .custom_domain_service
        .create_custom_domain(
            project_id,
            request.environment_id,
            request.domain.clone(),
            request.redirect_to,
            request.status_code,
            request.branch,
            request.service_name,
        )
        .await?;

    // Fetch additional info for response
    let domain_with_info = get_domain_with_info(&state, custom_domain).await?;

    state
        .telemetry
        .report(temps_core::telemetry::TelemetryEvent::new(
            temps_core::telemetry::TelemetryEventKind::CustomDomainAdded,
        ));

    Ok((
        StatusCode::CREATED,
        Json(CustomDomainResponse::from(domain_with_info)),
    ))
}

/// Get a custom domain by ID
#[utoipa::path(
    get,
    path = "/{project_id}/custom-domains/{domain_id}",
    responses(
        (status = 200, description = "Custom domain retrieved successfully", body = CustomDomainResponse),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Custom domain not found"),
        (status = 500, description = "Internal server error")
    ),
    params(
        ("project_id" = i32, Path, description = "Project ID"),
        ("domain_id" = i32, Path, description = "Custom domain ID")
    ),
    tag = "Custom Domains",
    security(
        ("bearer_auth" = [])
    )
)]
pub async fn get_custom_domain(
    RequireAuth(auth): RequireAuth,
    State(state): State<Arc<AppState>>,
    Path((project_id, domain_id)): Path<(i32, i32)>,
) -> Result<impl IntoResponse, Problem> {
    permission_guard!(auth, ProjectsRead);
    project_access_guard!(auth, project_id, state.project_access_checker);

    info!(
        "Getting custom domain: {} for project: {}",
        domain_id, project_id
    );

    let custom_domain = state
        .custom_domain_service
        .get_custom_domain(domain_id)
        .await?
        .ok_or_else(|| {
            problemdetails::new(StatusCode::NOT_FOUND)
                .with_title("Custom domain not found")
                .with_detail(format!("Custom domain with ID {} not found", domain_id))
        })?;

    // Verify it belongs to the specified project
    if custom_domain.project_id != project_id {
        return Err(problemdetails::new(StatusCode::NOT_FOUND)
            .with_title("Custom domain not found")
            .with_detail("Domain does not belong to the specified project"));
    }

    let domain_with_info = get_domain_with_info(&state, custom_domain).await?;

    Ok((
        StatusCode::OK,
        Json(CustomDomainResponse::from(domain_with_info)),
    ))
}

/// List all custom domains for a project
#[utoipa::path(
    get,
    path = "/{project_id}/custom-domains",
    responses(
        (status = 200, description = "Custom domains retrieved successfully", body = ListCustomDomainsResponse),
        (status = 401, description = "Unauthorized"),
        (status = 500, description = "Internal server error")
    ),
    params(
        ("project_id" = i32, Path, description = "Project ID")
    ),
    tag = "Custom Domains",
    security(
        ("bearer_auth" = [])
    )
)]
pub async fn list_custom_domains_for_project(
    RequireAuth(auth): RequireAuth,
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<i32>,
) -> Result<impl IntoResponse, Problem> {
    permission_guard!(auth, ProjectsRead);
    project_access_guard!(auth, project_id, state.project_access_checker);

    info!("Listing custom domains for project: {}", project_id);

    let custom_domains = state
        .custom_domain_service
        .list_custom_domains_for_project(project_id)
        .await?;

    let total = custom_domains.len();

    let mut domain_responses = Vec::new();
    for domain in custom_domains {
        let domain_with_info = get_domain_with_info(&state, domain).await?;
        domain_responses.push(CustomDomainResponse::from(domain_with_info));
    }

    Ok((
        StatusCode::OK,
        Json(ListCustomDomainsResponse {
            domains: domain_responses,
            total,
        }),
    ))
}

/// Find the project assignment for a certificate hostname.
#[utoipa::path(
    get,
    path = "/custom-domains/by-host/{hostname}",
    operation_id = "get_visible_custom_domain_by_hostname",
    responses(
        (status = 200, description = "Custom domain assignment retrieved", body = CustomDomainResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Project access denied"),
        (status = 404, description = "Domain is not assigned to a project")
    ),
    params(("hostname" = String, Path, description = "Domain hostname")),
    tag = "Custom Domains",
    security(("bearer_auth" = []))
)]
pub async fn get_custom_domain_by_hostname(
    RequireAuth(auth): RequireAuth,
    State(state): State<Arc<AppState>>,
    Path(hostname): Path<String>,
) -> Result<impl IntoResponse, Problem> {
    permission_guard!(auth, ProjectsRead);
    deny_deployment_token!(auth);
    let hidden_project_ids = super::handlers::resolve_hidden_projects(&state, &auth).await?;

    let custom_domain = state
        .custom_domain_service
        .get_visible_custom_domain_by_hostname(&hostname, &hidden_project_ids)
        .await?
        .ok_or_else(|| {
            problemdetails::new(StatusCode::NOT_FOUND)
                .with_title("Domain is not assigned")
                .with_detail(format!(
                    "Domain {hostname} is not currently assigned to a project environment"
                ))
        })?;

    let domain_with_info = get_domain_with_info(&state, custom_domain).await?;
    Ok((
        StatusCode::OK,
        Json(CustomDomainResponse::from(domain_with_info)),
    ))
}

/// Move a domain between projects without deleting its route or certificate.
#[utoipa::path(
    put,
    path = "/{source_project_id}/custom-domains/{domain_id}/assignment",
    operation_id = "reassign_project_custom_domain",
    request_body = ReassignCustomDomainRequest,
    responses(
        (status = 200, description = "Domain assignment updated atomically", body = CustomDomainResponse),
        (status = 400, description = "Target environment does not belong to the target project"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Write access required for both projects"),
        (status = 404, description = "Custom domain not found"),
        (status = 409, description = "Domain assignment changed; refresh and retry")
    ),
    params(
        ("source_project_id" = i32, Path, description = "Current project ID"),
        ("domain_id" = i32, Path, description = "Custom domain ID")
    ),
    tag = "Custom Domains",
    security(("bearer_auth" = []))
)]
pub async fn reassign_custom_domain(
    RequireAuth(auth): RequireAuth,
    State(state): State<Arc<AppState>>,
    Extension(metadata): Extension<RequestMetadata>,
    Path((source_project_id, domain_id)): Path<(i32, i32)>,
    Json(request): Json<ReassignCustomDomainRequest>,
) -> Result<impl IntoResponse, Problem> {
    project_permission_guard!(
        auth,
        ProjectsWrite,
        source_project_id,
        state.project_access_checker
    );
    project_permission_guard!(
        auth,
        ProjectsWrite,
        request.target_project_id,
        state.project_access_checker
    );

    // Persist a forensic intent before changing ownership. Audit backends may
    // fail independently from the project database, so mutation must not begin
    // until this record is durable. The operation deliberately says
    // REQUESTED: it remains accurate if row locking or validation later fails.
    let audit = CustomDomainReassignmentRequestedAudit {
        context: AuditContext {
            user_id: auth.user_id(),
            ip_address: Some(metadata.ip_address.to_string()),
            user_agent: metadata.user_agent,
        },
        custom_domain_id: domain_id,
        source_project_id,
        target_project_id: request.target_project_id,
        target_environment_id: request.target_environment_id,
    };
    state
        .audit_service
        .create_audit_log(&audit)
        .await
        .map_err(|audit_error| {
            error!(
                domain_id,
                source_project_id,
                target_project_id = request.target_project_id,
                error = %audit_error,
                "Failed to persist required custom-domain reassignment audit intent"
            );
            CustomDomainError::AuditIntentFailed {
                domain_id,
                source_project_id,
                target_project_id: request.target_project_id,
                reason: audit_error.to_string(),
            }
        })?;

    let updated_domain = state
        .custom_domain_service
        .reassign_custom_domain(
            domain_id,
            source_project_id,
            request.target_project_id,
            request.target_environment_id,
        )
        .await?;

    let completed_audit = CustomDomainReassignedAudit {
        context: audit.context,
        custom_domain_id: updated_domain.id,
        domain: updated_domain.domain.clone(),
        source_project_id,
        target_project_id: request.target_project_id,
        target_environment_id: request.target_environment_id,
    };
    if let Err(audit_error) = state.audit_service.create_audit_log(&completed_audit).await {
        error!(
            domain_id,
            source_project_id,
            target_project_id = request.target_project_id,
            error = %audit_error,
            "Failed to persist custom-domain reassignment completion audit"
        );
    }

    let domain_with_info = get_domain_with_info(&state, updated_domain).await?;
    Ok((
        StatusCode::OK,
        Json(CustomDomainResponse::from(domain_with_info)),
    ))
}

/// Update a custom domain
#[utoipa::path(
    put,
    path = "/{project_id}/custom-domains/{domain_id}",
    request_body = UpdateCustomDomainRequest,
    responses(
        (status = 200, description = "Custom domain updated successfully", body = CustomDomainResponse),
        (status = 400, description = "Invalid input"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Custom domain not found"),
        (status = 500, description = "Internal server error")
    ),
    params(
        ("project_id" = i32, Path, description = "Project ID"),
        ("domain_id" = i32, Path, description = "Custom domain ID")
    ),
    tag = "Custom Domains",
    security(
        ("bearer_auth" = [])
    )
)]
pub async fn update_custom_domain(
    RequireAuth(auth): RequireAuth,
    State(state): State<Arc<AppState>>,
    Path((project_id, domain_id)): Path<(i32, i32)>,
    Json(request): Json<UpdateCustomDomainRequest>,
) -> Result<impl IntoResponse, Problem> {
    project_permission_guard!(
        auth,
        ProjectsWrite,
        project_id,
        state.project_access_checker
    );

    info!(
        "Updating custom domain: {} for project: {}",
        domain_id, project_id
    );

    // Verify domain belongs to project
    let existing_domain = state
        .custom_domain_service
        .get_custom_domain(domain_id)
        .await?
        .ok_or_else(|| {
            problemdetails::new(StatusCode::NOT_FOUND)
                .with_title("Custom domain not found")
                .with_detail(format!("Custom domain with ID {} not found", domain_id))
        })?;

    if existing_domain.project_id != project_id {
        return Err(problemdetails::new(StatusCode::NOT_FOUND)
            .with_title("Custom domain not found")
            .with_detail("Domain does not belong to the specified project"));
    }

    // If only domain and/or environment_id are sent (no redirect fields), clear redirect settings
    let should_clear_redirect = request.redirect_to.is_none()
        && request.status_code.is_none()
        && request.branch.is_none()
        && (request.domain.is_some() || request.environment_id.is_some());

    let updated_domain = state
        .custom_domain_service
        .update_custom_domain(
            domain_id,
            request.domain,
            request.environment_id,
            request.redirect_to,
            request.status_code,
            request.branch,
            None,
            None,
            None,
            request.service_name,
        )
        .await?;

    // If we need to clear redirect settings, do a second update
    let updated_domain = if should_clear_redirect {
        state
            .custom_domain_service
            .update_custom_domain(
                domain_id,
                None,
                None,
                Some("".to_string()), // Empty string to clear
                Some(0),              // 0 to clear status code
                Some("".to_string()), // Empty string to clear branch
                None,
                None,
                None,
                None,
            )
            .await?
    } else {
        updated_domain
    };

    let domain_with_info = get_domain_with_info(&state, updated_domain).await?;

    Ok((
        StatusCode::OK,
        Json(CustomDomainResponse::from(domain_with_info)),
    ))
}

/// Delete a custom domain
#[utoipa::path(
    delete,
    path = "/{project_id}/custom-domains/{domain_id}",
    responses(
        (status = 204, description = "Custom domain deleted successfully"),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Custom domain not found"),
        (status = 500, description = "Internal server error")
    ),
    params(
        ("project_id" = i32, Path, description = "Project ID"),
        ("domain_id" = i32, Path, description = "Custom domain ID")
    ),
    tag = "Custom Domains",
    security(
        ("bearer_auth" = [])
    )
)]
pub async fn delete_custom_domain(
    RequireAuth(auth): RequireAuth,
    State(state): State<Arc<AppState>>,
    Path((project_id, domain_id)): Path<(i32, i32)>,
) -> Result<impl IntoResponse, Problem> {
    project_permission_guard!(
        auth,
        ProjectsDelete,
        project_id,
        state.project_access_checker
    );

    info!(
        "Deleting custom domain: {} for project: {}",
        domain_id, project_id
    );

    // Verify domain belongs to project
    let existing_domain = state
        .custom_domain_service
        .get_custom_domain(domain_id)
        .await?
        .ok_or_else(|| {
            problemdetails::new(StatusCode::NOT_FOUND)
                .with_title("Custom domain not found")
                .with_detail(format!("Custom domain with ID {} not found", domain_id))
        })?;

    if existing_domain.project_id != project_id {
        return Err(problemdetails::new(StatusCode::NOT_FOUND)
            .with_title("Custom domain not found")
            .with_detail("Domain does not belong to the specified project"));
    }

    state
        .custom_domain_service
        .delete_custom_domain(domain_id)
        .await?;

    Ok(StatusCode::NO_CONTENT)
}

/// Link a custom domain to a certificate
#[utoipa::path(
    post,
    path = "/{project_id}/custom-domains/{domain_id}/link-certificate/{certificate_id}",
    responses(
        (status = 200, description = "Custom domain linked to certificate successfully", body = CustomDomainResponse),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Custom domain or certificate not found"),
        (status = 500, description = "Internal server error")
    ),
    params(
        ("project_id" = i32, Path, description = "Project ID"),
        ("domain_id" = i32, Path, description = "Custom domain ID"),
        ("certificate_id" = i32, Path, description = "Certificate ID")
    ),
    tag = "Custom Domains",
    security(
        ("bearer_auth" = [])
    )
)]
pub async fn link_custom_domain_to_certificate(
    RequireAuth(auth): RequireAuth,
    State(state): State<Arc<AppState>>,
    Path((project_id, domain_id, certificate_id)): Path<(i32, i32, i32)>,
) -> Result<impl IntoResponse, Problem> {
    project_permission_guard!(
        auth,
        ProjectsWrite,
        project_id,
        state.project_access_checker
    );

    info!(
        "Linking custom domain: {} to certificate: {} for project: {}",
        domain_id, certificate_id, project_id
    );

    // Verify domain belongs to project
    let existing_domain = state
        .custom_domain_service
        .get_custom_domain(domain_id)
        .await?
        .ok_or_else(|| {
            problemdetails::new(StatusCode::NOT_FOUND)
                .with_title("Custom domain not found")
                .with_detail(format!("Custom domain with ID {} not found", domain_id))
        })?;

    if existing_domain.project_id != project_id {
        return Err(problemdetails::new(StatusCode::NOT_FOUND)
            .with_title("Custom domain not found")
            .with_detail("Domain does not belong to the specified project"));
    }

    let updated_domain = state
        .custom_domain_service
        .link_certificate(domain_id, certificate_id)
        .await?;

    let domain_with_info = get_domain_with_info(&state, updated_domain).await?;

    Ok((
        StatusCode::OK,
        Json(CustomDomainResponse::from(domain_with_info)),
    ))
}

// Helper function to get domain with additional info
async fn get_domain_with_info(
    state: &Arc<AppState>,
    custom_domain: project_custom_domains::Model,
) -> Result<CustomDomainWithInfo, Problem> {
    let db = state.project_service.db.as_ref();

    // Get domain info if certificate_id is present
    let domain_info = if let Some(cert_id) = custom_domain.certificate_id {
        domains::Entity::find_by_id(cert_id)
            .one(db)
            .await
            .map_err(|e| {
                error!("Failed to get domain info: {}", e);
                problemdetails::new(StatusCode::INTERNAL_SERVER_ERROR)
                    .with_title("Failed to get domain info")
                    .with_detail(e.to_string())
            })?
            .map(|d| DomainInfo {
                id: d.id,
                domain: d.domain,
                expiration_time: d.expiration_time,
                last_renewed: d.last_renewed,
            })
    } else {
        None
    };

    // Get environment info
    let environment = environments::Entity::find_by_id(custom_domain.environment_id)
        .one(db)
        .await
        .map_err(|e| {
            error!("Failed to get environment info: {}", e);
            problemdetails::new(StatusCode::INTERNAL_SERVER_ERROR)
                .with_title("Failed to get environment info")
                .with_detail(e.to_string())
        })?
        .map(|e| DomainEnvironment {
            id: e.id,
            name: e.name,
            slug: e.slug,
        });

    Ok(CustomDomainWithInfo {
        custom_domain,
        domain_info,
        environment,
    })
}

pub fn configure_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/projects/custom-domains/by-host/{hostname}",
            get(get_custom_domain_by_hostname),
        )
        .route(
            "/projects/{project_id}/custom-domains",
            post(create_custom_domain).get(list_custom_domains_for_project),
        )
        .route(
            "/projects/{source_project_id}/custom-domains/{domain_id}/assignment",
            axum::routing::put(reassign_custom_domain),
        )
        .route(
            "/projects/{project_id}/custom-domains/{domain_id}",
            get(get_custom_domain)
                .put(update_custom_domain)
                .delete(delete_custom_domain),
        )
        .route(
            "/projects/{project_id}/custom-domains/{domain_id}/link-certificate/{certificate_id}",
            post(link_custom_domain_to_certificate),
        )
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_reassign_custom_domain_handler_checks_source_and_target_project_permissions() {
        let source = include_str!("custom_domains.rs");
        let start = source
            .find("pub async fn reassign_custom_domain")
            .expect("reassign_custom_domain handler must exist");
        let remainder = &source[start + 1..];
        let end = remainder.find("pub async fn").unwrap_or(remainder.len());
        let handler = &source[start..start + 1 + end];

        let source_guard = handler
            .find("source_project_id,\n        state.project_access_checker")
            .expect("handler must authorize write access to the source project");
        let target_guard = handler
            .find("request.target_project_id,\n        state.project_access_checker")
            .expect("handler must authorize write access to the target project");
        let mutation = handler
            .find(".reassign_custom_domain(")
            .expect("handler must call the reassignment service");

        assert!(source_guard < mutation);
        assert!(target_guard < mutation);
    }

    #[test]
    fn test_get_custom_domain_by_hostname_handler_filters_inaccessible_projects_before_lookup() {
        let source = include_str!("custom_domains.rs");
        let start = source
            .find("pub async fn get_custom_domain_by_hostname")
            .expect("hostname handler must exist");
        let remainder = &source[start + 1..];
        let end = remainder.find("pub async fn").unwrap_or(remainder.len());
        let handler = &source[start..start + 1 + end];

        let deny_token = handler
            .find("deny_deployment_token!(auth)")
            .expect("hostname lookup must reject a deployment token without a visible-project set");
        let hidden_projects = handler
            .find("resolve_hidden_projects(&state, &auth)")
            .expect("hostname lookup must resolve projects hidden from the caller");
        let scoped_lookup = handler
            .find(".get_visible_custom_domain_by_hostname(&hostname, &hidden_project_ids)")
            .expect("hostname lookup must exclude hidden projects in its database query");

        assert!(deny_token < scoped_lookup);
        assert!(hidden_projects < scoped_lookup);
    }
}
