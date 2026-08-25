use std::sync::Arc;

use chrono::{Duration, Utc};

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, patch, post},
    Json, Router,
};
use serde::Deserialize;
use temps_auth::{permission_guard, project_access_guard, RequireAuth};
use temps_core::error_builder::{bad_request, internal_server_error, not_found};
use temps_core::problemdetails::Problem;
use temps_core::DateTime;
use utoipa::OpenApi;

use crate::services::{
    CreateIncidentRequest, CreateMonitorRequest, CurrentStatusResponse, IncidentBucketedResponse,
    IncidentResponse, IncidentUpdateResponse, MonitorResponse, ProjectMonitorHealth,
    StatusBucketedResponse, StatusPageError, StatusPageOverview, StatusPageService,
    UpdateIncidentStatusRequest, UptimeHistoryResponse,
};

/// Application state trait for status page routes
pub trait StatusPageAppState: Send + Sync + 'static {
    fn status_page_service(&self) -> &StatusPageService;
    fn telemetry(&self) -> &std::sync::Arc<dyn temps_core::TelemetryReporter>;
    /// Optional checker for team-based project access (human sessions only).
    fn project_access_checker(&self) -> Option<Arc<dyn temps_core::ProjectAccessChecker>>;
}

/// OpenAPI documentation for status page endpoints
#[derive(OpenApi)]
#[openapi(
    paths(
        get_status_overview,
        create_monitor,
        list_monitors,
        get_monitor,
        delete_monitor,
        get_current_monitor_status,
        get_uptime_history,
        get_bucketed_status,
        create_incident,
        list_incidents,
        get_incident,
        update_incident_status,
        get_incident_updates,
        get_bucketed_incidents,
        get_projects_monitor_health,
    ),
    components(
        schemas(
            StatusPageOverview,
            MonitorResponse,
            CreateMonitorRequest,
            CurrentStatusResponse,
            UptimeHistoryResponse,
            StatusBucketedResponse,
            IncidentResponse,
            CreateIncidentRequest,
            UpdateIncidentStatusRequest,
            IncidentUpdateResponse,
            IncidentBucketedResponse,
            ProjectMonitorHealth,
            ProjectsMonitorHealthResponse,
        )
    ),
    tags(
        (name = "Status Page", description = "Status page and monitoring endpoints")
    )
)]
pub struct StatusPageApiDoc;

#[derive(Deserialize)]
pub struct PaginationQuery {
    pub page: Option<u64>,
    pub page_size: Option<u64>,
}

#[derive(Deserialize)]
pub struct IncidentListQuery {
    pub page: Option<u64>,
    pub page_size: Option<u64>,
    pub environment_id: Option<i32>,
    pub status: Option<String>,
}

#[derive(Deserialize)]
pub struct MonitorListQuery {
    pub environment_id: Option<i32>,
}

#[derive(Deserialize)]
pub struct UptimeQuery {
    pub days: Option<i32>,
    pub start_time: Option<DateTime>, // ISO 8601 datetime -- overrides `days` when set
    pub end_time: Option<DateTime>,   // ISO 8601 datetime -- overrides `days` when set
}

#[derive(Deserialize)]
pub struct CurrentStatusQuery {
    pub start_time: Option<DateTime>, // Custom start time (ISO 8601) -- defaults to last 24h when unset
    pub end_time: Option<DateTime>, // Custom end time (ISO 8601) -- defaults to last 24h when unset
}

#[derive(Deserialize)]
pub struct BucketedQuery {
    pub interval: Option<String>,     // "5min", "hourly", or "daily"
    pub start_time: Option<DateTime>, // ISO 8601 datetime -- defaults to 24 hours ago when unset
    pub end_time: Option<DateTime>,   // ISO 8601 datetime -- defaults to now when unset
}

/// Get status page overview
#[utoipa::path(
    get,
    path = "/projects/{project_id}/status",
    params(
        ("project_id" = i32, Path, description = "Project ID"),
        ("environment_id" = Option<i32>, Query, description = "Filter by environment ID"),
    ),
    responses(
        (status = 200, description = "Successfully retrieved status overview", body = StatusPageOverview),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "Status Page",
    security(("bearer_auth" = []))
)]
pub async fn get_status_overview<T>(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<T>>,
    Path(project_id): Path<i32>,
    Query(query): Query<MonitorListQuery>,
) -> Result<impl IntoResponse, Problem>
where
    T: StatusPageAppState,
{
    permission_guard!(auth, StatusPageRead);
    project_access_guard!(auth, project_id, app_state.project_access_checker());
    app_state
        .status_page_service()
        .get_status_overview(project_id, query.environment_id)
        .await
        .map(Json)
        .map_err(map_error)
}

/// Create a new monitor
#[utoipa::path(
    post,
    path = "/projects/{project_id}/monitors",
    request_body = CreateMonitorRequest,
    params(
        ("project_id" = i32, Path, description = "Project ID"),
    ),
    responses(
        (status = 201, description = "Monitor created successfully", body = MonitorResponse),
        (status = 400, description = "Invalid request"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "Status Page",
    security(("bearer_auth" = []))
)]
pub async fn create_monitor<T>(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<T>>,
    Path(project_id): Path<i32>,
    Json(request): Json<CreateMonitorRequest>,
) -> Result<impl IntoResponse, Problem>
where
    T: StatusPageAppState,
{
    permission_guard!(auth, StatusPageCreate);
    project_access_guard!(auth, project_id, app_state.project_access_checker());
    let monitor = app_state
        .status_page_service()
        .monitor_service()
        .create_monitor(project_id, request)
        .await
        .map_err(map_error)?;

    app_state.telemetry().report(
        temps_core::TelemetryEvent::new(temps_core::TelemetryEventKind::StatusPagePublished)
            .with("monitor_type", monitor.monitor_type.clone()),
    );

    Ok((StatusCode::CREATED, Json(monitor)))
}

/// List monitors for a project
#[utoipa::path(
    get,
    path = "/projects/{project_id}/monitors",
    params(
        ("project_id" = i32, Path, description = "Project ID"),
        ("environment_id" = Option<i32>, Query, description = "Filter by environment ID"),
    ),
    responses(
        (status = 200, description = "Successfully retrieved monitors", body = Vec<MonitorResponse>),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "Status Page",
    security(("bearer_auth" = []))
)]
pub async fn list_monitors<T>(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<T>>,
    Path(project_id): Path<i32>,
    Query(query): Query<MonitorListQuery>,
) -> Result<impl IntoResponse, Problem>
where
    T: StatusPageAppState,
{
    permission_guard!(auth, StatusPageRead);
    project_access_guard!(auth, project_id, app_state.project_access_checker());
    app_state
        .status_page_service()
        .monitor_service()
        .list_monitors(project_id, query.environment_id)
        .await
        .map(Json)
        .map_err(map_error)
}

/// Get a monitor by ID
#[utoipa::path(
    get,
    path = "/monitors/{monitor_id}",
    params(
        ("monitor_id" = i32, Path, description = "Monitor ID"),
    ),
    responses(
        (status = 200, description = "Successfully retrieved monitor", body = MonitorResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Monitor not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "Status Page",
    security(("bearer_auth" = []))
)]
pub async fn get_monitor<T>(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<T>>,
    Path(monitor_id): Path<i32>,
) -> Result<impl IntoResponse, Problem>
where
    T: StatusPageAppState,
{
    permission_guard!(auth, StatusPageRead);
    let project_id = app_state
        .status_page_service()
        .monitor_service()
        .get_monitor_project_id(monitor_id)
        .await
        .map_err(map_error)?;
    project_access_guard!(auth, project_id, app_state.project_access_checker());
    app_state
        .status_page_service()
        .monitor_service()
        .get_monitor(monitor_id)
        .await
        .map(Json)
        .map_err(map_error)
}

/// Delete a monitor
#[utoipa::path(
    delete,
    path = "/monitors/{monitor_id}",
    params(
        ("monitor_id" = i32, Path, description = "Monitor ID"),
    ),
    responses(
        (status = 204, description = "Monitor deleted successfully"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Monitor not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "Status Page",
    security(("bearer_auth" = []))
)]
pub async fn delete_monitor<T>(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<T>>,
    Path(monitor_id): Path<i32>,
) -> Result<impl IntoResponse, Problem>
where
    T: StatusPageAppState,
{
    permission_guard!(auth, StatusPageDelete);
    let project_id = app_state
        .status_page_service()
        .monitor_service()
        .get_monitor_project_id(monitor_id)
        .await
        .map_err(map_error)?;
    project_access_guard!(auth, project_id, app_state.project_access_checker());
    app_state
        .status_page_service()
        .monitor_service()
        .delete_monitor(monitor_id)
        .await
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(map_error)
}

/// Get current status and uptime metrics for a monitor
#[utoipa::path(
    get,
    path = "/monitors/{monitor_id}/current-status",
    params(
        ("monitor_id" = i32, Path, description = "Monitor ID"),
        ("start_time" = Option<String>, Query, description = "Custom start time (ISO 8601) - overrides timeframe"),
        ("end_time" = Option<String>, Query, description = "Custom end time (ISO 8601)"),
    ),
    responses(
        (status = 200, description = "Successfully retrieved current status", body = CurrentStatusResponse),
        (status = 400, description = "Invalid time parameters"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Monitor not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "Status Page",
    security(("bearer_auth" = []))
)]
pub async fn get_current_monitor_status<T>(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<T>>,
    Path(monitor_id): Path<i32>,
    Query(query): Query<CurrentStatusQuery>,
) -> Result<impl IntoResponse, Problem>
where
    T: StatusPageAppState,
{
    permission_guard!(auth, StatusPageRead);
    let project_id = app_state
        .status_page_service()
        .monitor_service()
        .get_monitor_project_id(monitor_id)
        .await
        .map_err(map_error)?;
    project_access_guard!(auth, project_id, app_state.project_access_checker());
    // Custom timeframe only when BOTH bounds are given; otherwise the documented
    // default (last 24h) applies. These were previously non-optional `DateTime`
    // fields, so every call had to pass both or 400 -- making the 24h-default
    // path (and this doc comment's own premise) unreachable.
    let result = match (query.start_time, query.end_time) {
        (Some(start_time), Some(end_time)) => {
            app_state
                .status_page_service()
                .monitor_service()
                .get_current_status_with_timeframes(monitor_id, *start_time, *end_time)
                .await
        }
        _ => {
            app_state
                .status_page_service()
                .monitor_service()
                .get_current_status(monitor_id)
                .await
        }
    };
    result.map(Json).map_err(map_error)
}

/// Get uptime history for a monitor
#[utoipa::path(
    get,
    path = "/monitors/{monitor_id}/uptime",
    params(
        ("monitor_id" = i32, Path, description = "Monitor ID"),
        ("days" = Option<i32>, Query, description = "Number of days of history (default: 60) - ignored if start_time/end_time provided"),
        ("start_time" = Option<String>, Query, description = "Start time (ISO 8601) - overrides days parameter"),
        ("end_time" = Option<String>, Query, description = "End time (ISO 8601) - defaults to now"),
    ),
    responses(
        (status = 200, description = "Successfully retrieved uptime history", body = UptimeHistoryResponse),
        (status = 400, description = "Invalid time parameters"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Monitor not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "Status Page",
    security(("bearer_auth" = []))
)]
pub async fn get_uptime_history<T>(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<T>>,
    Path(monitor_id): Path<i32>,
    Query(query): Query<UptimeQuery>,
) -> Result<impl IntoResponse, Problem>
where
    T: StatusPageAppState,
{
    permission_guard!(auth, StatusPageRead);
    let project_id = app_state
        .status_page_service()
        .monitor_service()
        .get_monitor_project_id(monitor_id)
        .await
        .map_err(map_error)?;
    project_access_guard!(auth, project_id, app_state.project_access_checker());
    // Custom range only when BOTH bounds are given; otherwise fall back to the
    // `days`-based default (see get_uptime_history's own doc: 60 days). These
    // were previously non-optional `DateTime` fields, so `days` could never
    // actually take effect -- every call had to pass an explicit range.
    let result = match (query.start_time, query.end_time) {
        (Some(start_time), Some(end_time)) => {
            app_state
                .status_page_service()
                .monitor_service()
                .get_uptime_history_range(monitor_id, *start_time, *end_time)
                .await
        }
        _ => {
            app_state
                .status_page_service()
                .monitor_service()
                .get_uptime_history(monitor_id, query.days)
                .await
        }
    };
    result.map(Json).map_err(map_error)
}

/// Get bucketed status data for a monitor using TimescaleDB
#[utoipa::path(
    get,
    path = "/monitors/{monitor_id}/bucketed",
    params(
        ("monitor_id" = i32, Path, description = "Monitor ID"),
        ("interval" = Option<String>, Query, description = "Bucket interval: '5min', 'hourly', or 'daily' (default: hourly)"),
        ("start_time" = Option<String>, Query, description = "Start time (ISO 8601) (default: 24 hours ago)"),
        ("end_time" = Option<String>, Query, description = "End time (ISO 8601) (default: now)"),
    ),
    responses(
        (status = 200, description = "Successfully retrieved bucketed status data", body = StatusBucketedResponse),
        (status = 400, description = "Invalid parameters"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Monitor not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "Status Page",
    security(("bearer_auth" = []))
)]
pub async fn get_bucketed_status<T>(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<T>>,
    Path(monitor_id): Path<i32>,
    Query(query): Query<BucketedQuery>,
) -> Result<impl IntoResponse, Problem>
where
    T: StatusPageAppState,
{
    permission_guard!(auth, StatusPageRead);
    let project_id = app_state
        .status_page_service()
        .monitor_service()
        .get_monitor_project_id(monitor_id)
        .await
        .map_err(map_error)?;
    project_access_guard!(auth, project_id, app_state.project_access_checker());
    let interval = query.interval.as_deref().unwrap_or("hourly");
    // These were previously non-optional `DateTime` fields, so the documented
    // "(default: 24 hours ago)" / "(default: now)" behavior was unreachable --
    // every call had to pass both explicitly or 400.
    let end_time = query.end_time.map(|d| *d).unwrap_or_else(Utc::now);
    let start_time = query
        .start_time
        .map(|d| *d)
        .unwrap_or_else(|| end_time - Duration::hours(24));
    app_state
        .status_page_service()
        .monitor_service()
        .get_bucketed_status(monitor_id, interval, start_time, end_time)
        .await
        .map(Json)
        .map_err(map_error)
}

/// Create a new incident
#[utoipa::path(
    post,
    path = "/projects/{project_id}/incidents",
    request_body = CreateIncidentRequest,
    params(
        ("project_id" = i32, Path, description = "Project ID"),
    ),
    responses(
        (status = 201, description = "Incident created successfully", body = IncidentResponse),
        (status = 400, description = "Invalid request"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "Status Page",
    security(("bearer_auth" = []))
)]
pub async fn create_incident<T>(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<T>>,
    Path(project_id): Path<i32>,
    Json(request): Json<CreateIncidentRequest>,
) -> Result<impl IntoResponse, Problem>
where
    T: StatusPageAppState,
{
    permission_guard!(auth, StatusPageCreate);
    project_access_guard!(auth, project_id, app_state.project_access_checker());
    app_state
        .status_page_service()
        .incident_service()
        .create_incident(project_id, request)
        .await
        .map(|incident| (StatusCode::CREATED, Json(incident)))
        .map_err(map_error)
}

/// List incidents for a project
#[utoipa::path(
    get,
    path = "/projects/{project_id}/incidents",
    params(
        ("project_id" = i32, Path, description = "Project ID"),
        ("environment_id" = Option<i32>, Query, description = "Filter by environment ID"),
        ("status" = Option<String>, Query, description = "Filter by status"),
        ("page" = Option<u64>, Query, description = "Page number"),
        ("page_size" = Option<u64>, Query, description = "Items per page"),
    ),
    responses(
        (status = 200, description = "Successfully retrieved incidents"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "Status Page",
    security(("bearer_auth" = []))
)]
pub async fn list_incidents<T>(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<T>>,
    Path(project_id): Path<i32>,
    Query(query): Query<IncidentListQuery>,
) -> Result<impl IntoResponse, Problem>
where
    T: StatusPageAppState,
{
    permission_guard!(auth, StatusPageRead);
    project_access_guard!(auth, project_id, app_state.project_access_checker());
    let (incidents, total) = app_state
        .status_page_service()
        .incident_service()
        .list_incidents(
            project_id,
            query.environment_id,
            query.status,
            query.page,
            query.page_size,
        )
        .await
        .map_err(map_error)?;

    Ok(Json(serde_json::json!({
        "incidents": incidents,
        "total": total,
    })))
}

/// Get an incident by ID
#[utoipa::path(
    get,
    path = "/incidents/{incident_id}",
    params(
        ("incident_id" = i32, Path, description = "Incident ID"),
    ),
    responses(
        (status = 200, description = "Successfully retrieved incident", body = IncidentResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Incident not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "Status Page",
    security(("bearer_auth" = []))
)]
pub async fn get_incident<T>(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<T>>,
    Path(incident_id): Path<i32>,
) -> Result<impl IntoResponse, Problem>
where
    T: StatusPageAppState,
{
    permission_guard!(auth, StatusPageRead);
    let project_id = app_state
        .status_page_service()
        .incident_service()
        .get_incident_project_id(incident_id)
        .await
        .map_err(map_error)?;
    project_access_guard!(auth, project_id, app_state.project_access_checker());
    app_state
        .status_page_service()
        .incident_service()
        .get_incident(incident_id)
        .await
        .map(Json)
        .map_err(map_error)
}

/// Update incident status
#[utoipa::path(
    patch,
    path = "/incidents/{incident_id}/status",
    request_body = UpdateIncidentStatusRequest,
    params(
        ("incident_id" = i32, Path, description = "Incident ID"),
    ),
    responses(
        (status = 200, description = "Incident status updated successfully", body = IncidentResponse),
        (status = 400, description = "Invalid request"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Incident not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "Status Page",
    security(("bearer_auth" = []))
)]
pub async fn update_incident_status<T>(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<T>>,
    Path(incident_id): Path<i32>,
    Json(request): Json<UpdateIncidentStatusRequest>,
) -> Result<impl IntoResponse, Problem>
where
    T: StatusPageAppState,
{
    permission_guard!(auth, StatusPageWrite);
    let project_id = app_state
        .status_page_service()
        .incident_service()
        .get_incident_project_id(incident_id)
        .await
        .map_err(map_error)?;
    project_access_guard!(auth, project_id, app_state.project_access_checker());
    app_state
        .status_page_service()
        .incident_service()
        .update_incident_status(incident_id, request)
        .await
        .map(Json)
        .map_err(map_error)
}

/// Get incident updates
#[utoipa::path(
    get,
    path = "/incidents/{incident_id}/updates",
    params(
        ("incident_id" = i32, Path, description = "Incident ID"),
    ),
    responses(
        (status = 200, description = "Successfully retrieved incident updates", body = Vec<IncidentUpdateResponse>),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Incident not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "Status Page",
    security(("bearer_auth" = []))
)]
pub async fn get_incident_updates<T>(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<T>>,
    Path(incident_id): Path<i32>,
) -> Result<impl IntoResponse, Problem>
where
    T: StatusPageAppState,
{
    permission_guard!(auth, StatusPageRead);
    let project_id = app_state
        .status_page_service()
        .incident_service()
        .get_incident_project_id(incident_id)
        .await
        .map_err(map_error)?;
    project_access_guard!(auth, project_id, app_state.project_access_checker());
    app_state
        .status_page_service()
        .incident_service()
        .get_incident_updates(incident_id)
        .await
        .map(Json)
        .map_err(map_error)
}

/// Get bucketed incident data for a project
#[utoipa::path(
    get,
    path = "/projects/{project_id}/incidents/bucketed",
    params(
        ("project_id" = i32, Path, description = "Project ID"),
        ("environment_id" = Option<i32>, Query, description = "Filter by environment ID"),
        ("interval" = Option<String>, Query, description = "Bucket interval: '5min', 'hourly', or 'daily' (default: hourly)"),
        ("start_time" = Option<String>, Query, description = "Start time (ISO 8601) (default: 7 days ago)"),
        ("end_time" = Option<String>, Query, description = "End time (ISO 8601) (default: now)"),
    ),
    responses(
        (status = 200, description = "Successfully retrieved bucketed incident data", body = IncidentBucketedResponse),
        (status = 400, description = "Invalid parameters"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "Status Page",
    security(("bearer_auth" = []))
)]
pub async fn get_bucketed_incidents<T>(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<T>>,
    Path(project_id): Path<i32>,
    Query(query): Query<IncidentListQuery>,
    Query(bucket_query): Query<BucketedQuery>,
) -> Result<impl IntoResponse, Problem>
where
    T: StatusPageAppState,
{
    permission_guard!(auth, StatusPageRead);
    project_access_guard!(auth, project_id, app_state.project_access_checker());
    let interval = bucket_query.interval.as_deref().unwrap_or("hourly");
    // Previously non-optional `DateTime` fields made the documented
    // "(default: 7 days ago)" / "(default: now)" behavior unreachable -- every
    // call had to pass both explicitly or 400.
    let end_time = bucket_query.end_time.map(|d| *d).unwrap_or_else(Utc::now);
    let start_time = bucket_query
        .start_time
        .map(|d| *d)
        .unwrap_or_else(|| end_time - Duration::days(7));

    app_state
        .status_page_service()
        .incident_service()
        .get_bucketed_incidents(
            project_id,
            query.environment_id,
            interval,
            start_time,
            end_time,
        )
        .await
        .map(Json)
        .map_err(map_error)
}

/// Query parameters for batch project health
#[derive(Deserialize, utoipa::IntoParams)]
pub struct ProjectsHealthQuery {
    /// Comma-separated list of project IDs
    pub project_ids: String,
}

/// Batch response for projects health
#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct ProjectsMonitorHealthResponse {
    pub projects: std::collections::HashMap<String, ProjectMonitorHealth>,
}

/// Get monitor-based health summaries for multiple projects in a single query
#[utoipa::path(
    get,
    path = "/monitors-health/projects",
    params(ProjectsHealthQuery),
    responses(
        (status = 200, description = "Health summaries per project", body = ProjectsMonitorHealthResponse),
        (status = 400, description = "Invalid parameters"),
        (status = 401, description = "Unauthorized"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "Status Page",
    security(("bearer_auth" = []))
)]
pub async fn get_projects_monitor_health<T>(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<T>>,
    Query(query): Query<ProjectsHealthQuery>,
) -> Result<impl IntoResponse, Problem>
where
    T: StatusPageAppState,
{
    permission_guard!(auth, StatusPageRead);

    let project_ids: Vec<i32> = query
        .project_ids
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();

    if project_ids.is_empty() {
        return Err(bad_request()
            .detail("project_ids must contain at least one valid ID")
            .build());
    }

    if project_ids.len() > 100 {
        return Err(bad_request()
            .detail("Maximum 100 project IDs allowed")
            .build());
    }

    let summaries = app_state
        .status_page_service()
        .monitor_service()
        .get_projects_monitor_health(&project_ids)
        .await
        .map_err(map_error)?;

    let projects: std::collections::HashMap<String, ProjectMonitorHealth> = summaries
        .into_iter()
        .map(|s| (s.project_id.to_string(), s))
        .collect();

    Ok(Json(ProjectsMonitorHealthResponse { projects }))
}

/// Create router for status page endpoints
pub fn create_router<T>() -> Router<Arc<T>>
where
    T: StatusPageAppState,
{
    Router::new()
        .route("/projects/{project_id}/status", get(get_status_overview))
        .route("/projects/{project_id}/monitors", post(create_monitor))
        .route("/projects/{project_id}/monitors", get(list_monitors))
        .route(
            "/monitors-health/projects",
            get(get_projects_monitor_health),
        )
        .route("/monitors/{monitor_id}", get(get_monitor))
        .route("/monitors/{monitor_id}", delete(delete_monitor))
        .route(
            "/monitors/{monitor_id}/current-status",
            get(get_current_monitor_status),
        )
        .route("/monitors/{monitor_id}/uptime", get(get_uptime_history))
        .route("/monitors/{monitor_id}/bucketed", get(get_bucketed_status))
        .route("/projects/{project_id}/incidents", post(create_incident))
        .route("/projects/{project_id}/incidents", get(list_incidents))
        .route(
            "/projects/{project_id}/incidents/bucketed",
            get(get_bucketed_incidents),
        )
        .route("/incidents/{incident_id}", get(get_incident))
        .route(
            "/incidents/{incident_id}/status",
            patch(update_incident_status),
        )
        .route(
            "/incidents/{incident_id}/updates",
            get(get_incident_updates),
        )
}

fn map_error(error: StatusPageError) -> Problem {
    match error {
        StatusPageError::NotFound => not_found().detail("Resource not found").build(),
        StatusPageError::Validation(msg) => bad_request().detail(&msg).build(),
        StatusPageError::InvalidRequest(msg) => bad_request().detail(&msg).build(),
        StatusPageError::Database(err) => {
            tracing::error!("Database error: {}", err);
            internal_server_error()
                .detail("Database error while processing status page request")
                .build()
        }
        StatusPageError::Internal(msg) => {
            tracing::error!("Internal error: {}", msg);
            internal_server_error().detail(&msg).build()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use sea_orm::{ActiveModelTrait, Set};
    use temps_auth::context::AuthContext;
    use temps_auth::permissions::{Permission, Role};
    use temps_config::ConfigService;
    use temps_core::{NoopTelemetryReporter, ProjectAccessChecker, TelemetryReporter};
    use temps_database::test_utils::TestDatabase;
    use temps_entities::{environments, projects, upstream_config::UpstreamList, users};

    /// Mock team-based project access checker: allows only the projects
    /// listed in `allowed`, mirroring the plugin `TeamProjectAccessChecker`
    /// registers when EE Teams is installed.
    #[derive(Clone)]
    struct MockProjectAccessChecker {
        allowed: Vec<i32>,
    }

    #[async_trait]
    impl ProjectAccessChecker for MockProjectAccessChecker {
        async fn user_can_access_project(
            &self,
            _user_id: i32,
            project_id: i32,
        ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
            Ok(self.allowed.contains(&project_id))
        }
    }

    struct TestAppState {
        status_page_service: StatusPageService,
        telemetry: Arc<dyn TelemetryReporter>,
        project_access_checker: Option<Arc<dyn ProjectAccessChecker>>,
    }

    impl StatusPageAppState for TestAppState {
        fn status_page_service(&self) -> &StatusPageService {
            &self.status_page_service
        }

        fn telemetry(&self) -> &Arc<dyn TelemetryReporter> {
            &self.telemetry
        }

        fn project_access_checker(&self) -> Option<Arc<dyn ProjectAccessChecker>> {
            self.project_access_checker.clone()
        }
    }

    fn test_user() -> users::Model {
        let now = chrono::Utc::now();
        users::Model {
            id: 1,
            name: "Test User".to_string(),
            email: "test@example.com".to_string(),
            password_hash: None,
            email_verified: true,
            email_verification_token: None,
            email_verification_expires: None,
            password_reset_token: None,
            password_reset_expires: None,
            must_change_password: false,
            deleted_at: None,
            mfa_secret: None,
            mfa_enabled: false,
            mfa_recovery_codes: None,
            oidc_subject: None,
            oidc_provider_id: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// A non-admin caller holding exactly `StatusPageDelete` -- lets the
    /// delete-monitor guard test exercise `project_access_guard!` in
    /// isolation, since `Role::User` doesn't hold `StatusPageDelete` (would
    /// be rejected earlier by `permission_guard!`) and `Role::Admin` would
    /// bypass `project_access_guard!` entirely.
    fn status_page_delete_api_key_auth() -> AuthContext {
        AuthContext::new_api_key(
            test_user(),
            None,
            Some(vec![Permission::StatusPageDelete]),
            "status-page-delete-key".to_string(),
            1,
        )
    }

    fn create_mock_config_service(db: &Arc<sea_orm::DatabaseConnection>) -> Arc<ConfigService> {
        use temps_config::ServerConfig;
        let config = ServerConfig::new(
            "127.0.0.1:3000".to_string(),
            "postgres://test:test@localhost/test".to_string(),
            None,
            None,
        )
        .expect("Failed to create test config");
        Arc::new(ConfigService::new(Arc::new(config), db.clone()))
    }

    async fn create_test_project(db: &Arc<sea_orm::DatabaseConnection>) -> projects::Model {
        let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let slug = format!("test-project-{}", nanos);
        let project = projects::ActiveModel {
            name: Set("Test Project".to_string()),
            slug: Set(slug.clone()),
            directory: Set(slug),
            main_branch: Set("main".to_string()),
            preset: Set(temps_entities::preset::Preset::Nixpacks),
            repo_name: Set("test-repo".to_string()),
            repo_owner: Set("test-owner".to_string()),
            ..Default::default()
        };
        project.insert(db.as_ref()).await.unwrap()
    }

    async fn create_test_environment(
        db: &Arc<sea_orm::DatabaseConnection>,
        project_id: i32,
    ) -> environments::Model {
        let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let subdomain = format!("test-env-{}", nanos);
        let slug = format!("test-env-{}", nanos);
        let env = environments::ActiveModel {
            project_id: Set(project_id),
            name: Set(slug.clone()),
            slug: Set(slug),
            subdomain: Set(subdomain.clone()),
            host: Set(format!("{}.local", subdomain)),
            upstreams: Set(UpstreamList::default()),
            ..Default::default()
        };
        env.insert(db.as_ref()).await.unwrap()
    }

    /// Live project + monitor + incident, wired to an `AppState` backed by
    /// the checker `make_checker` builds from the real project id. Keeps the
    /// `TestDatabase` alive for the fixture's lifetime -- dropping it early
    /// would tear down the schema mid-test.
    struct Fixture {
        _db: TestDatabase,
        app_state: Arc<TestAppState>,
        monitor_id: i32,
        incident_id: i32,
    }

    async fn build_fixture(
        make_checker: impl FnOnce(i32) -> Option<Arc<dyn ProjectAccessChecker>>,
    ) -> Fixture {
        let test_db = TestDatabase::with_migrations().await.unwrap();
        let db = test_db.connection_arc();
        let config_service = create_mock_config_service(&db);
        let status_page_service = StatusPageService::new(db.clone(), config_service);

        let project = create_test_project(&db).await;
        let environment = create_test_environment(&db, project.id).await;

        let monitor = status_page_service
            .monitor_service()
            .create_monitor(
                project.id,
                CreateMonitorRequest {
                    name: "Test Monitor".to_string(),
                    monitor_type: "web".to_string(),
                    environment_id: environment.id,
                    check_interval_seconds: Some(60),
                    check_path: None,
                },
            )
            .await
            .unwrap();

        let incident = status_page_service
            .incident_service()
            .create_incident(
                project.id,
                CreateIncidentRequest {
                    title: "Test Incident".to_string(),
                    description: None,
                    severity: "minor".to_string(),
                    environment_id: Some(environment.id),
                    monitor_id: Some(monitor.id),
                },
            )
            .await
            .unwrap();

        let app_state = Arc::new(TestAppState {
            status_page_service,
            telemetry: Arc::new(NoopTelemetryReporter),
            project_access_checker: make_checker(project.id),
        });

        Fixture {
            _db: test_db,
            app_state,
            monitor_id: monitor.id,
            incident_id: incident.id,
        }
    }

    /// A checker that is registered but denies every project -- simulates a
    /// user authenticated on the instance who is not a member of the team
    /// that owns this monitor/incident's project.
    fn denies_everything(_project_id: i32) -> Option<Arc<dyn ProjectAccessChecker>> {
        Some(Arc::new(MockProjectAccessChecker { allowed: vec![] }))
    }

    /// A checker that grants access to exactly this fixture's project --
    /// simulates a user who *is* a member of the owning team.
    fn allows_this_project(project_id: i32) -> Option<Arc<dyn ProjectAccessChecker>> {
        Some(Arc::new(MockProjectAccessChecker {
            allowed: vec![project_id],
        }))
    }

    /// Regression: before the fix, `get_monitor` never called
    /// `project_access_guard!`, so any authenticated user with
    /// `StatusPageRead` could read another team's monitor by id.
    #[tokio::test]
    async fn get_monitor_denies_user_without_project_access() {
        let fx = build_fixture(denies_everything).await;

        let rejection = get_monitor(
            RequireAuth(AuthContext::new_session(test_user(), Role::User)),
            State(fx.app_state.clone()),
            Path(fx.monitor_id),
        )
        .await
        .err()
        .map(|p| p.status_code);

        assert_eq!(
            rejection,
            Some(StatusCode::FORBIDDEN),
            "a user without team access to the monitor's project must be denied"
        );
    }

    /// Sanity check for the happy path: a user whose team the checker grants
    /// access to must still be able to read the monitor.
    #[tokio::test]
    async fn get_monitor_allows_user_with_project_access() {
        let fx = build_fixture(allows_this_project).await;

        let rejection = get_monitor(
            RequireAuth(AuthContext::new_session(test_user(), Role::User)),
            State(fx.app_state.clone()),
            Path(fx.monitor_id),
        )
        .await
        .err()
        .map(|p| p.status_code);

        assert_ne!(
            rejection,
            Some(StatusCode::FORBIDDEN),
            "a user with team access to the monitor's project must not be denied"
        );
    }

    /// Regression: `delete_monitor` never called `project_access_guard!`.
    /// Uses an API-key caller scoped to exactly `StatusPageDelete` so the
    /// project-access check is exercised in isolation from both the
    /// instance-wide permission gate and the admin bypass.
    #[tokio::test]
    async fn delete_monitor_denies_caller_without_project_access() {
        let fx = build_fixture(denies_everything).await;

        let rejection = delete_monitor(
            RequireAuth(status_page_delete_api_key_auth()),
            State(fx.app_state.clone()),
            Path(fx.monitor_id),
        )
        .await
        .err()
        .map(|p| p.status_code);

        assert_eq!(
            rejection,
            Some(StatusCode::FORBIDDEN),
            "a caller without team access to the monitor's project must be denied"
        );
    }

    /// Regression: `get_current_monitor_status` never called
    /// `project_access_guard!`.
    #[tokio::test]
    async fn get_current_monitor_status_denies_user_without_project_access() {
        let fx = build_fixture(denies_everything).await;

        let rejection = get_current_monitor_status(
            RequireAuth(AuthContext::new_session(test_user(), Role::User)),
            State(fx.app_state.clone()),
            Path(fx.monitor_id),
            Query(CurrentStatusQuery {
                start_time: None,
                end_time: None,
            }),
        )
        .await
        .err()
        .map(|p| p.status_code);

        assert_eq!(rejection, Some(StatusCode::FORBIDDEN));
    }

    /// Regression: `get_uptime_history` never called `project_access_guard!`.
    #[tokio::test]
    async fn get_uptime_history_denies_user_without_project_access() {
        let fx = build_fixture(denies_everything).await;

        let rejection = get_uptime_history(
            RequireAuth(AuthContext::new_session(test_user(), Role::User)),
            State(fx.app_state.clone()),
            Path(fx.monitor_id),
            Query(UptimeQuery {
                days: None,
                start_time: None,
                end_time: None,
            }),
        )
        .await
        .err()
        .map(|p| p.status_code);

        assert_eq!(rejection, Some(StatusCode::FORBIDDEN));
    }

    /// Regression: `get_bucketed_status` never called `project_access_guard!`.
    #[tokio::test]
    async fn get_bucketed_status_denies_user_without_project_access() {
        let fx = build_fixture(denies_everything).await;

        let rejection = get_bucketed_status(
            RequireAuth(AuthContext::new_session(test_user(), Role::User)),
            State(fx.app_state.clone()),
            Path(fx.monitor_id),
            Query(BucketedQuery {
                interval: None,
                start_time: None,
                end_time: None,
            }),
        )
        .await
        .err()
        .map(|p| p.status_code);

        assert_eq!(rejection, Some(StatusCode::FORBIDDEN));
    }

    /// Regression: before the fix, `get_incident` never called
    /// `project_access_guard!`, so any authenticated user with
    /// `StatusPageRead` could read another team's incident by id.
    #[tokio::test]
    async fn get_incident_denies_user_without_project_access() {
        let fx = build_fixture(denies_everything).await;

        let rejection = get_incident(
            RequireAuth(AuthContext::new_session(test_user(), Role::User)),
            State(fx.app_state.clone()),
            Path(fx.incident_id),
        )
        .await
        .err()
        .map(|p| p.status_code);

        assert_eq!(
            rejection,
            Some(StatusCode::FORBIDDEN),
            "a user without team access to the incident's project must be denied"
        );
    }

    /// Regression: before the fix, `update_incident_status` never called
    /// `project_access_guard!`, so any authenticated user with
    /// `StatusPageWrite` could write a fabricated status onto another
    /// team's incident.
    #[tokio::test]
    async fn update_incident_status_denies_user_without_project_access() {
        let fx = build_fixture(denies_everything).await;

        let rejection = update_incident_status(
            RequireAuth(AuthContext::new_session(test_user(), Role::User)),
            State(fx.app_state.clone()),
            Path(fx.incident_id),
            Json(UpdateIncidentStatusRequest {
                status: "resolved".to_string(),
                message: "forged update".to_string(),
            }),
        )
        .await
        .err()
        .map(|p| p.status_code);

        assert_eq!(
            rejection,
            Some(StatusCode::FORBIDDEN),
            "a user without team access to the incident's project must not be able to write to it"
        );
    }

    /// Regression: `get_incident_updates` never called `project_access_guard!`.
    #[tokio::test]
    async fn get_incident_updates_denies_user_without_project_access() {
        let fx = build_fixture(denies_everything).await;

        let rejection = get_incident_updates(
            RequireAuth(AuthContext::new_session(test_user(), Role::User)),
            State(fx.app_state.clone()),
            Path(fx.incident_id),
        )
        .await
        .err()
        .map(|p| p.status_code);

        assert_eq!(rejection, Some(StatusCode::FORBIDDEN));
    }
}
