use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use temps_auth::{permission_guard, RequireAuth};
use temps_core::problemdetails::Problem;
use temps_query::{ContainerInfo, ContainerPath, EntityInfo, QueryOptions};
use utoipa::ToSchema;

use super::audit::AiDataAccessChangedAudit;
use super::types::AppState;

// ============================================================================
// Request/Response Types
// ============================================================================

#[derive(Debug, Serialize, ToSchema)]
pub struct ContainerResponse {
    /// Container name
    #[schema(example = "mydb")]
    pub name: String,
    /// Container type (database, schema, keyspace, bucket, etc.)
    #[schema(example = "database")]
    pub container_type: String,
    /// Can this container hold other containers?
    #[schema(example = true)]
    pub can_contain_containers: bool,
    /// Can this container hold entities (tables, collections, etc.)?
    #[schema(example = false)]
    pub can_contain_entities: bool,
    /// Type of child containers (if can_contain_containers is true)
    #[schema(example = "schema")]
    pub child_container_type: Option<String>,
    /// Label for entity type (if can_contain_entities is true)
    #[schema(example = "table")]
    pub entity_type_label: Option<String>,
    /// Hint for UI on expected entity count (small = sidebar, large = pagination)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "large")]
    pub entity_count_hint: Option<String>,
    /// Additional metadata
    pub metadata: serde_json::Value,
}

impl From<ContainerInfo> for ContainerResponse {
    fn from(info: ContainerInfo) -> Self {
        Self {
            name: info.name,
            container_type: info.container_type.to_string(),
            can_contain_containers: info.capabilities.can_contain_containers,
            can_contain_entities: info.capabilities.can_contain_entities,
            child_container_type: info
                .capabilities
                .child_container_type
                .map(|t| t.to_string()),
            entity_type_label: info.capabilities.entity_type_label,
            entity_count_hint: info.capabilities.entity_count_hint.map(|hint| match hint {
                temps_query::EntityCountHint::Small => "small".to_string(),
                temps_query::EntityCountHint::Large => "large".to_string(),
            }),
            metadata: serde_json::to_value(info.metadata).unwrap_or_default(),
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct EntityResponse {
    /// Entity name (table/collection)
    #[schema(example = "users")]
    pub name: String,
    /// Entity type (table, view, collection, etc.)
    #[schema(example = "table")]
    pub entity_type: String,
    /// Approximate row count
    #[schema(example = 1234)]
    pub row_count: Option<usize>,
    /// Size in bytes (for files/objects)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 1048576)]
    pub size_bytes: Option<u64>,
}

impl From<EntityInfo> for EntityResponse {
    fn from(info: EntityInfo) -> Self {
        Self {
            name: info.name,
            entity_type: info.entity_type,
            row_count: info.row_count,
            size_bytes: info.size_bytes,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PaginatedEntitiesResponse {
    /// List of entities
    pub entities: Vec<EntityResponse>,
    /// Total number of entities (if available)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
    /// Number of entities returned
    pub count: usize,
    /// Limit used for this request
    pub limit: usize,
    /// Whether there are more entities available
    pub has_more: bool,
    /// Continuation token for next page (S3, etc.)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_token: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ListEntitiesQuery {
    /// Maximum number of entities to return
    #[schema(example = 100)]
    #[serde(default = "default_entity_limit")]
    pub limit: usize,
    /// Continuation token for pagination (backend-specific)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

fn default_entity_limit() -> usize {
    100
}

#[derive(Debug, Serialize, ToSchema)]
pub struct FieldResponse {
    /// Field name
    #[schema(example = "id")]
    pub name: String,
    /// Field type (Int32, String, Timestamp, etc.)
    #[schema(example = "Int64")]
    pub field_type: String,
    /// Whether the field is nullable
    #[schema(example = false)]
    pub nullable: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct EntityInfoResponse {
    /// Full container path
    #[schema(example = json!(["mydb", "public"]))]
    pub container_path: Vec<String>,
    /// Entity name
    #[schema(example = "users")]
    pub entity: String,
    /// Entity type
    #[schema(example = "table")]
    pub entity_type: String,
    /// Field definitions
    pub fields: Vec<FieldResponse>,
    /// JSON Schema for sort options (if supported)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_schema: Option<serde_json::Value>,
    /// Approximate row count (for tables/collections)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 1234)]
    pub row_count: Option<usize>,
    /// Size in bytes (for objects/files)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 1048576)]
    pub size_bytes: Option<u64>,
    /// Additional metadata (content_type, last_modified, etag, etc.)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct QueryDataRequest {
    /// JSON filters (backend-specific format)
    #[serde(default)]
    pub filters: Option<serde_json::Value>,
    /// Maximum number of rows to return
    #[schema(example = 100)]
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Number of rows to skip
    #[schema(example = 0)]
    #[serde(default)]
    pub offset: usize,
    /// Sort by field name
    #[serde(default)]
    pub sort_by: Option<String>,
    /// Sort order (asc/desc)
    #[serde(default)]
    pub sort_order: Option<String>,
}

fn default_limit() -> usize {
    100
}

/// Query-string form of [`QueryDataRequest`] for the read-only `GET` rows
/// endpoint.
///
/// The `POST` variant exists because filters are arbitrary backend-specific
/// JSON. Reading rows is nonetheless a *read*, and the AI agent's tool index
/// is GET-only by construction, so the same capability has to be reachable
/// without a body. `filter` therefore carries the JSON as a string.
#[derive(Debug, Deserialize, ToSchema, Default)]
pub struct ReadRowsQuery {
    /// Backend-specific filter, JSON-encoded. Fetch the expected shape from
    /// the `filter_schema` field of the explorer-support endpoint — e.g.
    /// `{"where":"created_at > now() - interval '7 days'"}` for SQL sources.
    #[serde(default)]
    pub filter: Option<String>,
    /// Maximum number of rows to return
    #[schema(example = 100)]
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Number of rows to skip
    #[schema(example = 0)]
    #[serde(default)]
    pub offset: usize,
    /// Sort by field name
    #[serde(default)]
    pub sort_by: Option<String>,
    /// Sort order (asc/desc)
    #[serde(default)]
    pub sort_order: Option<String>,
}

/// Rows returned to the AI agent in a single call.
///
/// The tool caller already truncates oversized bodies, but truncation produces
/// a silently-partial result the model can't reason about. Clamping the row
/// count instead keeps every response well-formed and lets the agent page
/// explicitly via `offset`.
const AI_MAX_ROWS: usize = 100;

/// Hard ceiling for any caller, agent or human.
///
/// Rows are materialised into a `Vec<DataRow>` before serialising, so an
/// unbounded `limit` is a memory bomb on the 3 vCPU / 4 GB reference box —
/// `?limit=10000000` would try to pull an entire table into the control
/// plane. Matches the ceiling `list_entities` already applies.
const MAX_ROWS: usize = 1000;

/// Resolve the effective row limit for a request.
///
/// Split out from the handler so the clamp is unit-testable: it is the only
/// thing standing between a stray query string and an OOM.
fn effective_row_limit(requested: usize, is_ai_call: bool) -> usize {
    let ceiling = if is_ai_call { AI_MAX_ROWS } else { MAX_ROWS };
    requested.min(ceiling)
}

/// Whether an agent-originated request may read this service's rows.
///
/// Pure so the gate can be tested without standing up an `AppState`; this is
/// the branch that decides whether table contents reach a third-party LLM
/// provider, so it must not be exercised only through integration paths.
fn ai_may_read_rows(is_ai_call: bool, service_ai_data_access: bool) -> bool {
    !is_ai_call || service_ai_data_access
}

/// Parse the `filter` query-string parameter into the backend-specific JSON
/// the data source expects.
///
/// Returns a 400 (rather than silently ignoring an unparsable filter) so a
/// caller that mistypes a filter gets no rows *and an explanation*, instead of
/// a full unfiltered table that looks like a successful answer.
fn parse_row_filter(
    filter: Option<&str>,
    filter_schema: Option<&serde_json::Value>,
) -> Result<Option<serde_json::Value>, Problem> {
    let Some(raw) = filter.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };

    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(value) => Ok(Some(value)),
        Err(e) => {
            let mut problem = temps_core::problemdetails::new(StatusCode::BAD_REQUEST)
                .with_title("Invalid Filter")
                .with_detail(format!(
                    "The `filter` parameter must be JSON matching this service's filter schema, \
                     but it failed to parse: {e}. Received: {raw}"
                ));
            if let Some(schema) = filter_schema {
                problem = problem.with_value("expected_filter_schema", schema.clone());
            }
            Err(problem)
        }
    }
}

/// Read rows from an entity (read-only `GET`; see [`query_data`] for the
/// `POST` form used by the console).
///
/// Gated for AI callers by the service's `ai_data_access` opt-in — see
/// [`temps_core::ai_tool_call::AiToolCall`].
#[utoipa::path(
    get,
    path = "/external-services/{service_id}/query/containers/{path}/entities/{entity}/data",
    tag = "External Services - Query",
    params(
        ("service_id" = i32, Path, description = "External service id"),
        ("path" = String, Path, description = "Container path, slash-separated (e.g. `mydb/public`)"),
        ("entity" = String, Path, description = "Table, collection, key or object name"),
        ("filter" = Option<String>, Query, description = "JSON-encoded backend-specific filter"),
        ("limit" = Option<usize>, Query, description = "Maximum rows to return"),
        ("offset" = Option<usize>, Query, description = "Rows to skip"),
        ("sort_by" = Option<String>, Query, description = "Field to sort by"),
        ("sort_order" = Option<String>, Query, description = "asc or desc"),
    ),
    responses(
        (status = 200, description = "Query results", body = QueryDataResponse),
        (status = 400, description = "Invalid query or filter"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions, or AI data access not enabled for this service"),
        (status = 404, description = "Service, container, or entity not found"),
        (status = 500, description = "Internal server error")
    ),
    security(("bearer_auth" = []))
)]
pub async fn read_entity_rows(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<AppState>>,
    Path((service_id, path_str, entity)): Path<(i32, String, String)>,
    ai_call: Option<axum::Extension<temps_core::ai_tool_call::AiToolCall>>,
    axum::extract::Query(query): axum::extract::Query<ReadRowsQuery>,
) -> Result<impl IntoResponse, Problem> {
    permission_guard!(auth, ExternalServicesRead);

    let is_ai_call = ai_call.is_some();

    // Per-service opt-in for agent row access. Schema browsing stays open —
    // only the rows themselves are gated, because those are what would be
    // shipped to a third-party LLM provider.
    //
    // NOTE ON PROJECT SCOPING: `ApiCallScope::project_ids` constrains only
    // operations that take a `project_id` parameter (see `build_request_parts`
    // in temps-ai-api-tools). This route is keyed by `service_id`, so the
    // conversation's project boundary does NOT narrow it — an agent chatting
    // about project A can reach a service linked only to project B, bounded by
    // the caller's own `ExternalServicesRead`. That is accepted deliberately:
    // services are a platform-level resource that can be shared across
    // projects, so there is no single owning project to scope to, and the
    // `ai_data_access` opt-in below is the intended boundary — an operator
    // enables row access per service, whatever project the chat is about.
    if is_ai_call {
        let service = app_state
            .external_service_manager
            .get_service(service_id)
            .await
            .map_err(|e| {
                temps_core::problemdetails::new(StatusCode::NOT_FOUND)
                    .with_title("Service Not Found")
                    .with_detail(e.to_string())
            })?;

        if !ai_may_read_rows(is_ai_call, service.ai_data_access) {
            return Err(temps_core::problemdetails::new(StatusCode::FORBIDDEN)
                .with_title("AI Data Access Not Enabled")
                .with_detail(format!(
                    "Reading row data from service '{}' (id {}) is not enabled for the AI \
                     assistant. Table and column names are still readable. An operator can turn \
                     row access on under Settings → AI data access on the service page; it is off \
                     by default because rows can contain password hashes, tokens and personal \
                     data that would be sent to the configured AI provider.",
                    service.name, service_id
                ))
                .with_value("service_id", service_id)
                .with_value("service_name", service.name.clone())
                .with_value("setup_path", format!("/storage/{service_id}")));
        }
    }

    let filter_schema = app_state
        .query_service
        .get_filter_schema(service_id)
        .await
        .ok();
    let filters = parse_row_filter(query.filter.as_deref(), filter_schema.as_ref())?;

    let segments: Vec<String> = path_str.split('/').map(String::from).collect();
    let path = ContainerPath::new(segments);

    let limit = effective_row_limit(query.limit, is_ai_call);

    let options = QueryOptions {
        limit: Some(limit),
        offset: Some(query.offset),
        cursor: None,
        sort_by: query.sort_by,
        sort_order: query.sort_order,
        timeout_ms: None,
        include_nulls: true,
    };

    let result = app_state
        .query_service
        .query_data(service_id, &path, &entity, filters, options)
        .await
        .map_err(|e| {
            let (status, title) = match &e {
                temps_query::DataError::QueryFailed(_) => (StatusCode::BAD_REQUEST, "Query Failed"),
                temps_query::DataError::InvalidQuery(_) => {
                    (StatusCode::BAD_REQUEST, "Invalid Query")
                }
                temps_query::DataError::NotFound(_) => (StatusCode::NOT_FOUND, "Not Found"),
                _ => (StatusCode::INTERNAL_SERVER_ERROR, "Query Error"),
            };

            temps_core::problemdetails::new(status)
                .with_title(title)
                .with_detail(e.to_string())
        })?;

    let response = QueryDataResponse {
        fields: result
            .schema
            .fields
            .iter()
            .map(|f| FieldResponse {
                name: f.name.clone(),
                field_type: format!("{:?}", f.field_type),
                nullable: f.nullable,
            })
            .collect(),
        rows: result
            .rows
            .into_iter()
            .map(|row| serde_json::to_value(row).unwrap_or_default())
            .collect(),
        total_count: result.stats.total_rows.unwrap_or(result.stats.row_count) as u64,
        returned_count: result.stats.row_count,
        execution_time_ms: result.stats.execution_ms,
    };

    Ok(Json(response))
}

/// Describes a level in the data source hierarchy
#[derive(Debug, Serialize, ToSchema, Clone)]
pub struct HierarchyLevel {
    /// Level number (0 = root)
    #[schema(example = 0)]
    pub level: u32,
    /// Human-readable name for this level
    #[schema(example = "root")]
    pub name: String,
    /// Type of container at this level
    #[schema(example = "database")]
    pub container_type: String,
    /// Can list containers at this level?
    #[schema(example = true)]
    pub can_list_containers: bool,
    /// Can list entities at this level?
    #[schema(example = false)]
    pub can_list_entities: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ExplorerSupportResponse {
    /// Whether the service supports query explorer functionality
    #[schema(example = true)]
    pub supported: bool,
    /// Service type
    #[schema(example = "postgres")]
    pub service_type: String,
    /// Capabilities supported by this service
    #[schema(example = json!(["sql"]))]
    pub capabilities: Vec<String>,
    /// Hierarchy levels (describes the navigation structure)
    pub hierarchy: Vec<HierarchyLevel>,
    /// JSON Schema for filter format with embedded UI hints (if supported)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter_schema: Option<serde_json::Value>,
    /// Reason why explorer is not supported (if applicable)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct QueryDataResponse {
    /// Field definitions
    pub fields: Vec<FieldResponse>,
    /// Data rows (array of JSON objects)
    pub rows: Vec<serde_json::Value>,
    /// Total number of rows matching the query (before limit/offset)
    #[schema(example = 1234)]
    pub total_count: u64,
    /// Number of rows returned in this response
    #[schema(example = 100)]
    pub returned_count: usize,
    /// Query execution time in milliseconds
    #[schema(example = 45)]
    pub execution_time_ms: u64,
}

// ============================================================================
// Handler Functions
// ============================================================================

/// Check if a service supports query explorer functionality
#[utoipa::path(
    get,
    path = "/external-services/{service_id}/query/explorer-support",
    tag = "External Services - Query",
    responses(
        (status = 200, description = "Explorer support information", body = ExplorerSupportResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Service not found"),
        (status = 500, description = "Internal server error")
    ),
    security(("bearer_auth" = []))
)]
pub async fn check_explorer_support(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<AppState>>,
    Path(service_id): Path<i32>,
) -> Result<impl IntoResponse, Problem> {
    permission_guard!(auth, ExternalServicesRead);

    // Get service configuration
    let service = app_state
        .external_service_manager
        .get_service_config(service_id)
        .await
        .map_err(|e| {
            temps_core::problemdetails::new(StatusCode::NOT_FOUND)
                .with_title("Service Not Found")
                .with_detail(format!("Service with ID {} not found: {}", service_id, e))
        })?;

    // Check if service type supports querying
    let (supported, capabilities, filter_schema, hierarchy, reason) = match service.service_type {
        crate::externalsvc::ServiceType::Mariadb => {
            let filter_schema = app_state
                .query_service
                .get_filter_schema(service_id)
                .await
                .ok();

            let hierarchy = vec![
                HierarchyLevel {
                    level: 0,
                    name: "root".to_string(),
                    container_type: "database".to_string(),
                    can_list_containers: true,
                    can_list_entities: false,
                },
                HierarchyLevel {
                    level: 1,
                    name: "database".to_string(),
                    container_type: "table".to_string(),
                    can_list_containers: false,
                    can_list_entities: true,
                },
            ];

            (
                true,
                vec!["sql".to_string()],
                filter_schema,
                hierarchy,
                None,
            )
        }
        crate::externalsvc::ServiceType::Postgres => {
            // Get filter schema from QueryService
            let filter_schema = app_state
                .query_service
                .get_filter_schema(service_id)
                .await
                .ok();

            // PostgreSQL has 3 levels: root -> database -> schema -> tables
            let hierarchy = vec![
                HierarchyLevel {
                    level: 0,
                    name: "root".to_string(),
                    container_type: "database".to_string(),
                    can_list_containers: true,
                    can_list_entities: false,
                },
                HierarchyLevel {
                    level: 1,
                    name: "database".to_string(),
                    container_type: "schema".to_string(),
                    can_list_containers: true,
                    can_list_entities: false,
                },
                HierarchyLevel {
                    level: 2,
                    name: "schema".to_string(),
                    container_type: "table".to_string(),
                    can_list_containers: false,
                    can_list_entities: true,
                },
            ];

            (
                true,
                vec!["sql".to_string()],
                filter_schema,
                hierarchy,
                None,
            )
        }
        crate::externalsvc::ServiceType::S3 => {
            // S3 has 2 levels: root -> buckets -> objects
            let hierarchy = vec![
                HierarchyLevel {
                    level: 0,
                    name: "root".to_string(),
                    container_type: "bucket".to_string(),
                    can_list_containers: true,
                    can_list_entities: false,
                },
                HierarchyLevel {
                    level: 1,
                    name: "bucket".to_string(),
                    container_type: "object".to_string(),
                    can_list_containers: false,
                    can_list_entities: true,
                },
            ];

            (
                true,
                vec!["object-store".to_string()],
                None,
                hierarchy,
                None,
            )
        }
        crate::externalsvc::ServiceType::Mongodb => {
            // MongoDB: root -> databases -> collections -> documents
            let hierarchy = vec![
                HierarchyLevel {
                    level: 0,
                    name: "root".to_string(),
                    container_type: "database".to_string(),
                    can_list_containers: true,
                    can_list_entities: false,
                },
                HierarchyLevel {
                    level: 1,
                    name: "database".to_string(),
                    container_type: "collection".to_string(),
                    can_list_containers: false,
                    can_list_entities: true,
                },
            ];

            (true, vec!["document".to_string()], None, hierarchy, None)
        }
        crate::externalsvc::ServiceType::Redis => {
            // Redis: flat key-value store (1 level)
            let hierarchy = vec![HierarchyLevel {
                level: 0,
                name: "root".to_string(),
                container_type: "key".to_string(),
                can_list_containers: false,
                can_list_entities: true,
            }];

            (true, vec!["key-value".to_string()], None, hierarchy, None)
        }
        // Temps KV: Same as Redis (flat key-value store)
        crate::externalsvc::ServiceType::Kv => {
            let hierarchy = vec![HierarchyLevel {
                level: 0,
                name: "root".to_string(),
                container_type: "key".to_string(),
                can_list_containers: false,
                can_list_entities: true,
            }];

            (true, vec!["key-value".to_string()], None, hierarchy, None)
        }
        // Temps Blob: Same as S3 (object store)
        crate::externalsvc::ServiceType::Blob => {
            let hierarchy = vec![
                HierarchyLevel {
                    level: 0,
                    name: "root".to_string(),
                    container_type: "bucket".to_string(),
                    can_list_containers: true,
                    can_list_entities: false,
                },
                HierarchyLevel {
                    level: 1,
                    name: "bucket".to_string(),
                    container_type: "object".to_string(),
                    can_list_containers: false,
                    can_list_entities: true,
                },
            ];

            (
                true,
                vec!["object-store".to_string()],
                None,
                hierarchy,
                None,
            )
        }
        // RustFS standalone: Same as S3/Blob (object store)
        crate::externalsvc::ServiceType::Rustfs => {
            let hierarchy = vec![
                HierarchyLevel {
                    level: 0,
                    name: "root".to_string(),
                    container_type: "bucket".to_string(),
                    can_list_containers: true,
                    can_list_entities: false,
                },
                HierarchyLevel {
                    level: 1,
                    name: "bucket".to_string(),
                    container_type: "object".to_string(),
                    can_list_containers: false,
                    can_list_entities: true,
                },
            ];

            (
                true,
                vec!["object-store".to_string()],
                None,
                hierarchy,
                None,
            )
        }
        // MinIO (deprecated): Same as S3/Blob (object store)
        #[allow(deprecated)]
        crate::externalsvc::ServiceType::Minio => {
            let hierarchy = vec![
                HierarchyLevel {
                    level: 0,
                    name: "root".to_string(),
                    container_type: "bucket".to_string(),
                    can_list_containers: true,
                    can_list_entities: false,
                },
                HierarchyLevel {
                    level: 1,
                    name: "bucket".to_string(),
                    container_type: "object".to_string(),
                    can_list_containers: false,
                    can_list_entities: true,
                },
            ];

            (
                true,
                vec!["object-store".to_string()],
                None,
                hierarchy,
                None,
            )
        }
    };

    let response = ExplorerSupportResponse {
        supported,
        service_type: format!("{:?}", service.service_type).to_lowercase(),
        capabilities,
        hierarchy,
        filter_schema,
        reason,
    };

    Ok(Json(response))
}

/// List containers at the root level (databases, keyspaces, etc.)
#[utoipa::path(
    get,
    path = "/external-services/{service_id}/query/containers",
    tag = "External Services - Query",
    responses(
        (status = 200, description = "List of root containers", body = Vec<ContainerResponse>),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Service not found"),
        (status = 500, description = "Internal server error")
    ),
    security(("bearer_auth" = []))
)]
pub async fn list_root_containers(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<AppState>>,
    Path(service_id): Path<i32>,
) -> Result<impl IntoResponse, Problem> {
    permission_guard!(auth, ExternalServicesRead);

    let path = ContainerPath::root();
    let containers = app_state
        .query_service
        .list_containers(service_id, &path)
        .await
        .map_err(|e| {
            temps_core::problemdetails::new(StatusCode::INTERNAL_SERVER_ERROR)
                .with_title("Query Error")
                .with_detail(format!("Failed to list containers: {}", e))
        })?;

    let response: Vec<ContainerResponse> = containers.into_iter().map(Into::into).collect();
    Ok(Json(response))
}

/// List containers at a specific path
/// Path segments are separated by forward slashes
/// Example: /external-services/1/query/containers/mydb lists schemas in database "mydb"
#[utoipa::path(
    get,
    path = "/external-services/{service_id}/query/containers/{path}",
    tag = "External Services - Query",
    responses(
        (status = 200, description = "List of containers", body = Vec<ContainerResponse>),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Service or container not found"),
        (status = 500, description = "Internal server error")
    ),
    security(("bearer_auth" = []))
)]
pub async fn list_containers_at_path(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<AppState>>,
    Path((service_id, path_str)): Path<(i32, String)>,
) -> Result<impl IntoResponse, Problem> {
    permission_guard!(auth, ExternalServicesRead);

    // Parse path from URL (segments separated by /)
    let segments: Vec<String> = path_str.split('/').map(String::from).collect();
    let path = ContainerPath::new(segments);

    let containers = app_state
        .query_service
        .list_containers(service_id, &path)
        .await
        .map_err(|e| {
            temps_core::problemdetails::new(StatusCode::INTERNAL_SERVER_ERROR)
                .with_title("Query Error")
                .with_detail(format!("Failed to list containers: {}", e))
        })?;

    let response: Vec<ContainerResponse> = containers.into_iter().map(Into::into).collect();
    Ok(Json(response))
}

/// Get information about a specific container
#[utoipa::path(
    get,
    path = "/external-services/{service_id}/query/containers/{path}/info",
    tag = "External Services - Query",
    responses(
        (status = 200, description = "Container information", body = ContainerResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Service or container not found"),
        (status = 500, description = "Internal server error")
    ),
    security(("bearer_auth" = []))
)]
pub async fn get_container_info(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<AppState>>,
    Path((service_id, path_str)): Path<(i32, String)>,
) -> Result<impl IntoResponse, Problem> {
    permission_guard!(auth, ExternalServicesRead);

    let segments: Vec<String> = path_str.split('/').map(String::from).collect();
    let path = ContainerPath::new(segments);

    let container = app_state
        .query_service
        .get_container_info(service_id, &path)
        .await
        .map_err(|e| {
            temps_core::problemdetails::new(StatusCode::INTERNAL_SERVER_ERROR)
                .with_title("Query Error")
                .with_detail(format!("Failed to get container info: {}", e))
        })?;

    Ok(Json(ContainerResponse::from(container)))
}

/// List entities (tables, collections, etc.) in a container
/// Example: /external-services/1/query/containers/mydb/public/entities lists tables in the public schema
#[utoipa::path(
    get,
    path = "/external-services/{service_id}/query/containers/{path}/entities",
    tag = "External Services - Query",
    params(
        ("limit" = Option<usize>, Query, description = "Maximum number of entities to return (default: 100, max: 1000)"),
        ("token" = Option<String>, Query, description = "Continuation token for pagination"),
    ),
    responses(
        (status = 200, description = "Paginated list of entities", body = PaginatedEntitiesResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Service or container not found"),
        (status = 500, description = "Internal server error")
    ),
    security(("bearer_auth" = []))
)]
pub async fn list_entities(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<AppState>>,
    Path((service_id, path_str)): Path<(i32, String)>,
    axum::extract::Query(query): axum::extract::Query<ListEntitiesQuery>,
) -> Result<impl IntoResponse, Problem> {
    permission_guard!(auth, ExternalServicesRead);

    let segments: Vec<String> = path_str.split('/').map(String::from).collect();
    let path = ContainerPath::new(segments);

    // Cap limit at 1000
    let limit = std::cmp::min(query.limit, 1000);

    let (entities, next_token) = app_state
        .query_service
        .list_entities_paginated(service_id, &path, limit, query.token)
        .await
        .map_err(|e| {
            temps_core::problemdetails::new(StatusCode::INTERNAL_SERVER_ERROR)
                .with_title("Query Error")
                .with_detail(format!("Failed to list entities: {}", e))
        })?;

    let count = entities.len();
    let has_more = next_token.is_some();
    let entity_responses: Vec<EntityResponse> = entities.into_iter().map(Into::into).collect();

    let response = PaginatedEntitiesResponse {
        entities: entity_responses,
        total: None, // Not all backends can provide total count efficiently
        count,
        limit,
        has_more,
        next_token,
    };

    Ok(Json(response))
}

/// Get detailed information about an entity (table schema)
#[utoipa::path(
    get,
    path = "/external-services/{service_id}/query/containers/{path}/entities/{entity}",
    tag = "External Services - Query",
    responses(
        (status = 200, description = "Entity details", body = EntityInfoResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Service, container, or entity not found"),
        (status = 500, description = "Internal server error")
    ),
    security(("bearer_auth" = []))
)]
pub async fn get_entity_info(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<AppState>>,
    Path((service_id, path_str, entity)): Path<(i32, String, String)>,
) -> Result<impl IntoResponse, Problem> {
    permission_guard!(auth, ExternalServicesRead);

    let segments: Vec<String> = path_str.split('/').map(String::from).collect();
    let path = ContainerPath::new(segments.clone());

    let entity_info = app_state
        .query_service
        .get_entity_info(service_id, &path, &entity)
        .await
        .map_err(|e| {
            temps_core::problemdetails::new(StatusCode::INTERNAL_SERVER_ERROR)
                .with_title("Query Error")
                .with_detail(format!("Failed to get entity info: {}", e))
        })?;

    let fields = entity_info
        .schema
        .map(|s| {
            s.fields
                .into_iter()
                .map(|f| FieldResponse {
                    name: f.name,
                    field_type: format!("{:?}", f.field_type),
                    nullable: f.nullable,
                })
                .collect()
        })
        .unwrap_or_default();

    // Get sort schema from QueryService
    let sort_schema = app_state
        .query_service
        .get_sort_schema(service_id, &path, &entity)
        .await
        .ok();

    let response = EntityInfoResponse {
        container_path: segments,
        entity: entity_info.name.clone(),
        entity_type: entity_info.entity_type.clone(),
        fields,
        sort_schema,
        row_count: entity_info.row_count,
        size_bytes: entity_info.size_bytes,
        metadata: entity_info.metadata,
    };

    Ok(Json(response))
}

/// Query data from an entity with optional filters, pagination, and sorting
#[utoipa::path(
    post,
    path = "/external-services/{service_id}/query/containers/{path}/entities/{entity}/data",
    tag = "External Services - Query",
    request_body = QueryDataRequest,
    responses(
        (status = 200, description = "Query results", body = QueryDataResponse),
        (status = 400, description = "Invalid query"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Service, container, or entity not found"),
        (status = 500, description = "Internal server error")
    ),
    security(("bearer_auth" = []))
)]
pub async fn query_data(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<AppState>>,
    Path((service_id, path_str, entity)): Path<(i32, String, String)>,
    Json(request): Json<QueryDataRequest>,
) -> Result<impl IntoResponse, Problem> {
    permission_guard!(auth, ExternalServicesRead);

    let segments: Vec<String> = path_str.split('/').map(String::from).collect();
    let path = ContainerPath::new(segments);

    let options = QueryOptions {
        limit: Some(request.limit),
        offset: Some(request.offset),
        cursor: None,
        sort_by: request.sort_by,
        sort_order: request.sort_order,
        timeout_ms: None,
        include_nulls: true,
    };

    let result = app_state
        .query_service
        .query_data(service_id, &path, &entity, request.filters, options)
        .await
        .map_err(|e| {
            // Determine if this is a user error (400) or server error (500)
            let (status, title) = match &e {
                temps_query::DataError::QueryFailed(_) => {
                    // Query syntax errors are user errors
                    (StatusCode::BAD_REQUEST, "Query Failed")
                }
                temps_query::DataError::InvalidQuery(_) => {
                    (StatusCode::BAD_REQUEST, "Invalid Query")
                }
                temps_query::DataError::NotFound(_) => (StatusCode::NOT_FOUND, "Not Found"),
                _ => {
                    // Other errors are server errors
                    (StatusCode::INTERNAL_SERVER_ERROR, "Query Error")
                }
            };

            temps_core::problemdetails::new(status)
                .with_title(title)
                .with_detail(e.to_string()) // Use to_string() instead of format! to avoid extra nesting
        })?;

    let response = QueryDataResponse {
        fields: result
            .schema
            .fields
            .into_iter()
            .map(|f| FieldResponse {
                name: f.name,
                field_type: format!("{:?}", f.field_type),
                nullable: f.nullable,
            })
            .collect(),
        rows: result
            .rows
            .into_iter()
            .map(|row| serde_json::to_value(row).unwrap_or_default())
            .collect(),
        total_count: result.stats.total_rows.unwrap_or(result.stats.row_count) as u64,
        returned_count: result.stats.row_count,
        execution_time_ms: result.stats.execution_ms,
    };

    Ok(Json(response))
}

/// Download an object (S3 only) as a streaming response
#[utoipa::path(
    get,
    path = "/external-services/{service_id}/query/containers/{path}/entities/{entity}/download",
    tag = "External Services - Query",
    responses(
        (status = 200, description = "Object data stream", content_type = "application/octet-stream"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Object not found"),
        (status = 500, description = "Internal server error")
    ),
    security(("bearer_auth" = []))
)]
pub async fn download_object(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<AppState>>,
    Path((service_id, path_str, entity)): Path<(i32, String, String)>,
) -> Result<impl IntoResponse, Problem> {
    permission_guard!(auth, ExternalServicesRead);

    // Parse path
    let segments: Vec<&str> = if path_str.is_empty() {
        vec![]
    } else {
        path_str.split('/').collect()
    };
    let container_path = ContainerPath::from_slice(&segments);

    // Download object stream
    let (stream, content_type) = app_state
        .query_service
        .download(service_id, &container_path, &entity)
        .await
        .map_err(|e| {
            let (status, title) = match &e {
                temps_query::DataError::NotFound(_) => (StatusCode::NOT_FOUND, "Object Not Found"),
                temps_query::DataError::InvalidQuery(_) => {
                    (StatusCode::BAD_REQUEST, "Invalid Request")
                }
                temps_query::DataError::OperationNotSupported(_) => {
                    (StatusCode::BAD_REQUEST, "Operation Not Supported")
                }
                _ => (StatusCode::INTERNAL_SERVER_ERROR, "Download Error"),
            };
            temps_core::problemdetails::new(status)
                .with_title(title)
                .with_detail(e.to_string())
        })?;

    // Convert stream to Axum Body
    let body = Body::from_stream(stream);

    // Set response headers
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        content_type
            .unwrap_or_else(|| "application/octet-stream".to_string())
            .parse()
            .unwrap(),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        format!("attachment; filename=\"{}\"", entity)
            .parse()
            .unwrap(),
    );

    Ok((headers, body))
}

// ============================================================================
// AI data access opt-in
// ============================================================================

#[derive(Debug, Deserialize, ToSchema)]
pub struct ToggleAiDataAccessRequest {
    /// Whether the AI assistant may read row data from this service.
    #[schema(example = false)]
    pub enabled: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AiDataAccessResponse {
    /// Service id
    pub service_id: i32,
    /// Whether the AI assistant may read row data from this service
    #[schema(example = false)]
    pub enabled: bool,
}

/// Report whether the AI assistant may read row data from this service.
///
/// Always answers (rather than 404-ing when disabled) so the console can render
/// the capability with an "off — here's how to turn it on" state instead of
/// hiding it, and so the agent can tell "not set up" apart from "not supported".
#[utoipa::path(
    get,
    path = "/external-services/{service_id}/query/ai-data-access",
    operation_id = "get_ai_data_access",
    tag = "External Services - Query",
    params(("service_id" = i32, Path, description = "External service id")),
    responses(
        (status = 200, description = "Current AI data access setting", body = AiDataAccessResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Service not found"),
    ),
    security(("bearer_auth" = []))
)]
pub async fn get_ai_data_access(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<AppState>>,
    Path(service_id): Path<i32>,
) -> Result<impl IntoResponse, Problem> {
    // Read side deliberately carries no `assert_service_owned_by_caller`,
    // unlike `set_ai_data_access` below. It matches every other read in this
    // module (explorer-support, containers, entities), and the response is a
    // single boolean about the platform's own configuration — no tenant data.
    // The write path is where widening access must be tenancy-checked.
    permission_guard!(auth, ExternalServicesRead);

    let service = app_state
        .external_service_manager
        .get_service(service_id)
        .await
        .map_err(|e| {
            temps_core::problemdetails::new(StatusCode::NOT_FOUND)
                .with_title("Service Not Found")
                .with_detail(e.to_string())
        })?;

    Ok(Json(AiDataAccessResponse {
        service_id,
        enabled: service.ai_data_access,
    }))
}

/// Enable or disable AI assistant access to this service's row data.
///
/// Off by default. Row contents can include password hashes, API tokens and
/// personal data, and enabling this sends them to the configured AI provider —
/// so it is a deliberate, audited, per-service decision by the operator.
#[utoipa::path(
    patch,
    path = "/external-services/{service_id}/query/ai-data-access",
    operation_id = "set_ai_data_access",
    tag = "External Services - Query",
    request_body = ToggleAiDataAccessRequest,
    params(("service_id" = i32, Path, description = "External service id")),
    responses(
        (status = 200, description = "Setting applied", body = AiDataAccessResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 404, description = "Service not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(("bearer_auth" = []))
)]
pub async fn set_ai_data_access(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<AppState>>,
    Path(service_id): Path<i32>,
    axum::Extension(metadata): axum::Extension<temps_core::RequestMetadata>,
    Json(request): Json<ToggleAiDataAccessRequest>,
) -> Result<impl IntoResponse, Problem> {
    permission_guard!(auth, ExternalServicesWrite);
    // Same tenancy check the metrics toggle uses: holding
    // ExternalServicesWrite must not let a caller widen AI access to a
    // service that isn't theirs.
    super::metrics_handlers::assert_service_owned_by_caller(service_id, &auth, &app_state).await?;

    let service = app_state
        .external_service_manager
        .get_service(service_id)
        .await
        .map_err(|e| {
            temps_core::problemdetails::new(StatusCode::NOT_FOUND)
                .with_title("Service Not Found")
                .with_detail(e.to_string())
        })?;

    let active = temps_entities::external_services::ActiveModel {
        id: sea_orm::ActiveValue::Set(service_id),
        ai_data_access: sea_orm::ActiveValue::Set(request.enabled),
        ..Default::default()
    };
    sea_orm::EntityTrait::update(active)
        .exec(app_state.db.as_ref())
        .await
        .map_err(|e| {
            tracing::error!(service_id, error = %e, "Failed to update ai_data_access");
            temps_core::problemdetails::new(StatusCode::INTERNAL_SERVER_ERROR)
                .with_title("Update Failed")
                .with_detail(format!(
                    "Failed to update AI data access for service {service_id}: {e}"
                ))
        })?;

    // Audited: this widens what a third party (the AI provider) can see, so it
    // must be attributable. A failure here must not fail the request — the
    // setting is already persisted.
    let audit = AiDataAccessChangedAudit {
        context: temps_core::audit::AuditContext {
            user_id: auth.user_id(),
            ip_address: Some(metadata.ip_address.clone()),
            user_agent: metadata.user_agent.clone(),
        },
        service_id,
        service_name: service.name.clone(),
        enabled: request.enabled,
    };
    if let Err(e) = app_state.audit_service.create_audit_log(&audit).await {
        tracing::error!(service_id, error = %e, "Failed to write ai_data_access audit log");
    }

    Ok(Json(AiDataAccessResponse {
        service_id,
        enabled: request.enabled,
    }))
}

// ============================================================================
// Route Configuration
// ============================================================================

pub fn configure_query_routes() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        // Explorer support check
        .route(
            "/external-services/{service_id}/query/explorer-support",
            axum::routing::get(check_explorer_support),
        )
        // Root containers
        .route(
            "/external-services/{service_id}/query/containers",
            axum::routing::get(list_root_containers),
        )
        // Containers at path
        .route(
            "/external-services/{service_id}/query/containers/{path}",
            axum::routing::get(list_containers_at_path),
        )
        // Container info
        .route(
            "/external-services/{service_id}/query/containers/{path}/info",
            axum::routing::get(get_container_info),
        )
        // Entities in container
        .route(
            "/external-services/{service_id}/query/containers/{path}/entities",
            axum::routing::get(list_entities),
        )
        // Entity info
        .route(
            "/external-services/{service_id}/query/containers/{path}/entities/{entity}",
            axum::routing::get(get_entity_info),
        )
        // Query entity data
        .route(
            "/external-services/{service_id}/query/containers/{path}/entities/{entity}/data",
            axum::routing::post(query_data).get(read_entity_rows),
        )
        // AI data access opt-in (read + toggle)
        .route(
            "/external-services/{service_id}/query/ai-data-access",
            axum::routing::get(get_ai_data_access).patch(set_ai_data_access),
        )
        // Download object (S3 only)
        .route(
            "/external-services/{service_id}/query/containers/{path}/entities/{entity}/download",
            axum::routing::get(download_object),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_row_limit_clamps_every_caller_not_just_the_agent() {
        // Rows are materialised into a Vec before serialising, so an
        // unbounded limit is an OOM on a 4 GB box. The clamp originally only
        // applied to agent calls, leaving `?limit=10000000` open to anyone.
        assert_eq!(effective_row_limit(10_000_000, false), MAX_ROWS);
        assert_eq!(effective_row_limit(10_000_000, true), AI_MAX_ROWS);
    }

    #[test]
    fn effective_row_limit_leaves_reasonable_requests_alone() {
        assert_eq!(effective_row_limit(20, false), 20);
        assert_eq!(effective_row_limit(20, true), 20);
        // Agents get the tighter ceiling so a response stays well-formed
        // rather than being truncated mid-body by the tool caller.
        assert_eq!(effective_row_limit(500, false), 500);
        assert_eq!(effective_row_limit(500, true), AI_MAX_ROWS);
    }

    #[test]
    fn ai_may_read_rows_requires_the_per_service_opt_in() {
        // The whole point of the gate: an agent call against a service that
        // has not opted in must be refused, even though the caller's
        // ExternalServicesRead permission would otherwise allow it.
        assert!(!ai_may_read_rows(true, false));
        assert!(ai_may_read_rows(true, true));
    }

    #[test]
    fn ai_may_read_rows_never_blocks_human_callers() {
        // The opt-in governs the built-in assistant only. Console and CLI
        // traffic is governed by permissions, and must be unaffected whatever
        // the flag says — otherwise enabling the AI feature would silently
        // change who can browse their own data.
        assert!(ai_may_read_rows(false, false));
        assert!(ai_may_read_rows(false, true));
    }

    #[test]
    fn parse_row_filter_returns_none_for_absent_or_blank() {
        // A caller that omits the filter, or sends `?filter=`, means "no
        // filter" — not "filter that matches nothing".
        assert!(parse_row_filter(None, None).unwrap().is_none());
        assert!(parse_row_filter(Some(""), None).unwrap().is_none());
        assert!(parse_row_filter(Some("   "), None).unwrap().is_none());
    }

    #[test]
    fn parse_row_filter_parses_backend_json() {
        let parsed = parse_row_filter(Some(r#"{"where":"id > 5"}"#), None)
            .expect("valid JSON filter should parse")
            .expect("non-empty filter should be Some");

        // The value is handed to the data source untouched — Postgres reads
        // `where`, MongoDB reads its own shape, etc.
        assert_eq!(parsed["where"], "id > 5");
    }

    #[test]
    fn parse_row_filter_rejects_unparsable_filter_rather_than_ignoring_it() {
        // Silently dropping a malformed filter would return a full unfiltered
        // table that looks like a correct answer — the worst possible failure
        // for both a human and an agent.
        let err = parse_row_filter(Some("id > 5"), None)
            .expect_err("a bare WHERE clause is not JSON and must be rejected");

        assert_eq!(err.status_code, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn parse_row_filter_echoes_expected_schema_so_callers_can_self_correct() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "where": { "type": "string" } }
        });

        let err = parse_row_filter(Some("oops"), Some(&schema))
            .expect_err("invalid filter must be rejected");

        // The agent recovers from a 400 only if the response tells it the
        // shape it should have sent.
        let echoed = err
            .body
            .get("expected_filter_schema")
            .expect("problem should carry the expected filter schema");
        assert_eq!(echoed["properties"]["where"]["type"], "string");
    }
}
