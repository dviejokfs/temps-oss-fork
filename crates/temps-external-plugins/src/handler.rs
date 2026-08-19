//! HTTP handlers for external plugin management endpoints.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use temps_auth::{permission_guard, RequireAuth};
use temps_core::error_builder;
use temps_core::external_plugin::{NavEntry, NavSection, PluginManifest, UiManifest, UiRoute};
use temps_core::problemdetails::Problem;
use utoipa::{OpenApi as OpenApiTrait, ToSchema};

use crate::install::{PlatformAsset, PluginInstaller, PluginRegistryManifest};

use crate::service::ExternalPluginsService;

/// Handler state for the external plugins API.
#[derive(Clone)]
pub struct ExternalPluginsAppState {
    pub service: Arc<ExternalPluginsService>,
}

/// List all running external plugins and their manifests.
///
/// Requires only a valid session/token (no specific permission) since the
/// manifest drives sidebar navigation rendering for every authenticated
/// user, not just admins.
#[utoipa::path(
    tag = "External Plugins",
    get,
    path = "/x/plugins",
    responses(
        (status = 200, description = "List of all running external plugins", body = Vec<PluginManifest>),
        (status = 401, description = "Unauthorized"),
    ),
    security(("bearer_auth" = []))
)]
async fn list_external_plugins(
    RequireAuth(_auth): RequireAuth,
    State(state): State<ExternalPluginsAppState>,
) -> Json<Vec<PluginManifest>> {
    Json(state.service.manifests().await)
}

/// Response from the reload endpoint.
#[derive(Debug, Serialize, ToSchema)]
pub struct ReloadResponse {
    /// Number of plugins successfully loaded after reload
    pub loaded: usize,
    /// Names of loaded plugins
    pub plugins: Vec<String>,
    /// Human-readable status message
    pub message: String,
}

/// Reload all external plugins.
///
/// Stops all running plugin processes, re-scans the plugins directory,
/// starts any discovered binaries, and hot-swaps the proxy router so new
/// and removed plugins take effect immediately without a server restart.
///
/// Requires `SystemAdmin` permission.
#[utoipa::path(
    tag = "External Plugins",
    post,
    path = "/x/plugins/reload",
    responses(
        (status = 200, description = "Plugins reloaded successfully", body = ReloadResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
    ),
    security(("bearer_auth" = []))
)]
async fn reload_plugins(
    RequireAuth(auth): RequireAuth,
    State(state): State<ExternalPluginsAppState>,
) -> Result<(StatusCode, Json<ReloadResponse>), Problem> {
    permission_guard!(auth, SystemAdmin);

    tracing::info!("Admin triggered plugin reload");

    let manifests = state.service.reload_plugins().await;
    let names: Vec<String> = manifests.iter().map(|m| m.name.clone()).collect();
    let count = names.len();

    Ok((
        StatusCode::OK,
        Json(ReloadResponse {
            loaded: count,
            plugins: names,
            message: format!("Reload complete. {} plugin(s) loaded.", count),
        }),
    ))
}

// ---------------------------------------------------------------------------
// Plugin install endpoints
// ---------------------------------------------------------------------------

/// Manifest URL for the builder plugin. This is the **only** URL this endpoint
/// will ever fetch — the URL is never taken from an untrusted caller to prevent
/// SSRF / RCE. If the release host changes this constant must be updated and
/// the binary redeployed.
///
/// Served by temps.sh rather than a code-hosting release page: the manifest is
/// the trust root for the whole install flow (asset URLs and their SHA-256
/// digests both come from it), so it has to live on a host we control and can
/// serve to every self-hosted instance.
const VIBETEMPS_MANIFEST_URL: &str = "https://temps.sh/api/plugins/vibetemps/manifest.json";

/// Binary name of the VibeTemps plugin as it appears inside the release
/// tarball and as it is written into the plugins directory.
const VIBETEMPS_BINARY_NAME: &str = "temps-vibetemps-plugin";

/// Response for the plugin availability check endpoint.
#[derive(Debug, Serialize, ToSchema)]
pub struct PluginAvailabilityResponse {
    /// Whether the plugin binary is already installed (present on disk).
    pub installed: bool,
    /// The manifest fetched from the registry, if reachable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest: Option<PluginRegistryManifest>,
    /// Human-readable reason when the manifest could not be fetched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Request body for the plugin install endpoint.
#[derive(Debug, Deserialize, ToSchema)]
pub struct InstallPluginRequest {
    /// Name of the plugin to install. Currently only `"vibetemps"` is valid.
    pub name: String,
    /// Specific version hint (currently unused; install always fetches latest).
    pub version: Option<String>,
}

/// Response for the plugin install endpoint.
#[derive(Debug, Serialize, ToSchema)]
pub struct InstallPluginResponse {
    /// Name of the installed plugin.
    pub name: String,
    /// Version that was installed.
    pub version: String,
    /// Absolute path of the installed binary.
    pub path: String,
    /// Whether the plugin process was reloaded after install.
    pub reloaded: bool,
    /// Human-readable status message.
    pub message: String,
}

/// Response for the per-plugin status endpoint.
#[derive(Debug, Serialize, ToSchema)]
pub struct PluginStatusResponse {
    /// Whether the plugin binary is present in the plugins directory **and**
    /// the plugin process is currently running.
    pub configured: bool,
    /// Why the plugin is not configured (when `configured` is false).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Console path the operator should visit to configure or install it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setup_path: Option<String>,
}

/// Check whether a plugin is available for install and whether it's already installed.
///
/// Fetches the registry manifest for the named plugin and returns it alongside
/// an `installed` flag. Requires `SystemAdmin` permission.
#[utoipa::path(
    tag = "External Plugins",
    get,
    path = "/x/plugins/available",
    responses(
        (status = 200, description = "Plugin availability info", body = PluginAvailabilityResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
    ),
    security(("bearer_auth" = []))
)]
async fn get_available_plugins(
    RequireAuth(auth): RequireAuth,
    State(state): State<ExternalPluginsAppState>,
) -> Result<(StatusCode, Json<PluginAvailabilityResponse>), Problem> {
    permission_guard!(auth, SystemAdmin);

    let plugins_dir = state.service.manager().config().plugins_dir.clone();
    let binary_path = plugins_dir.join(VIBETEMPS_BINARY_NAME);
    let installed = binary_path.exists();

    match PluginInstaller::fetch_manifest(VIBETEMPS_MANIFEST_URL).await {
        Ok(manifest) => Ok((
            StatusCode::OK,
            Json(PluginAvailabilityResponse {
                installed,
                manifest: Some(manifest),
                reason: None,
            }),
        )),
        Err(e) => Ok((
            StatusCode::OK,
            Json(PluginAvailabilityResponse {
                installed,
                manifest: None,
                reason: Some(format!(
                    "Could not fetch VibeTemps manifest from {}: {}",
                    VIBETEMPS_MANIFEST_URL, e
                )),
            }),
        )),
    }
}

/// Download, verify, and install an external plugin binary.
///
/// Currently only `"vibetemps"` is a valid plugin name. After a successful
/// install the plugin process is (re)started automatically. Requires
/// `SystemAdmin` permission.
#[utoipa::path(
    tag = "External Plugins",
    post,
    path = "/x/plugins/install",
    request_body = InstallPluginRequest,
    responses(
        (status = 200, description = "Plugin installed and started", body = InstallPluginResponse),
        (status = 400, description = "Invalid or unsupported plugin name"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
        (status = 500, description = "Download, checksum, or install failure"),
    ),
    security(("bearer_auth" = []))
)]
async fn install_plugin(
    RequireAuth(auth): RequireAuth,
    State(state): State<ExternalPluginsAppState>,
    Json(request): Json<InstallPluginRequest>,
) -> Result<(StatusCode, Json<InstallPluginResponse>), Problem> {
    permission_guard!(auth, SystemAdmin);

    // v1: only "vibetemps" is a valid plugin name. This is an intentionally
    // fixed single-entry registry, not a general marketplace.
    if request.name != "vibetemps" {
        return Err(error_builder::bad_request()
            .title("Unsupported Plugin")
            .detail(format!(
                "'{}' is not a known installable plugin. Currently only 'vibetemps' is supported.",
                request.name
            ))
            .build());
    }

    let manifest = PluginInstaller::fetch_manifest(VIBETEMPS_MANIFEST_URL)
        .await
        .map_err(|e| {
            error_builder::internal_server_error()
                .title("Manifest Fetch Failed")
                .detail(format!(
                    "Could not fetch VibeTemps manifest: {}. Check network connectivity and try again.",
                    e
                ))
                .build()
        })?;

    let plugins_dir = state.service.manager().config().plugins_dir.clone();
    let installer = PluginInstaller::new();

    let installed_path = installer
        .install(VIBETEMPS_BINARY_NAME, &manifest, &plugins_dir)
        .await
        .map_err(|e| {
            // Surface distinct error types with actionable messages.
            let detail = e.to_string();
            let title = if detail.contains("Checksum mismatch") {
                "Checksum Verification Failed"
            } else if detail.contains("unsupported platform")
                || detail.contains("no release for platform")
            {
                "Unsupported Platform"
            } else if detail.contains("not found in tarball") {
                "Tarball Extraction Failed"
            } else if detail.contains("HTTP") {
                "Download Failed"
            } else {
                "Plugin Install Failed"
            };
            error_builder::internal_server_error()
                .title(title)
                .detail(detail)
                .build()
        })?;

    // Start or reload the plugin process — non-fatal on failure (binary is
    // installed; operator can trigger a manual reload).
    let reloaded = match state
        .service
        .start_or_reload_plugin(VIBETEMPS_BINARY_NAME)
        .await
    {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!(
                plugin = VIBETEMPS_BINARY_NAME,
                "Plugin binary installed but process start failed: {}. \
                 Trigger a manual reload via POST /x/plugins/reload.",
                e
            );
            false
        }
    };

    Ok((
        StatusCode::OK,
        Json(InstallPluginResponse {
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            path: installed_path.display().to_string(),
            reloaded,
            message: if reloaded {
                format!(
                    "VibeTemps plugin v{} installed and started successfully.",
                    manifest.version
                )
            } else {
                format!(
                    "VibeTemps plugin v{} installed. Process start failed — use POST /x/plugins/reload to activate it.",
                    manifest.version
                )
            },
        }),
    ))
}

/// Get the running status of a named external plugin.
///
/// Returns `configured: true` when the plugin binary is present on disk
/// **and** the plugin process is currently running. Any authenticated user
/// may call this endpoint (same permission level as `GET /x/plugins`).
#[utoipa::path(
    tag = "External Plugins",
    get,
    path = "/x/plugins/{name}/status",
    params(
        ("name" = String, Path, description = "Plugin name (e.g. 'vibetemps')")
    ),
    responses(
        (status = 200, description = "Plugin status", body = PluginStatusResponse),
        (status = 401, description = "Unauthorized"),
    ),
    security(("bearer_auth" = []))
)]
async fn get_plugin_status(
    RequireAuth(_auth): RequireAuth,
    Path(name): Path<String>,
    State(state): State<ExternalPluginsAppState>,
) -> Result<(StatusCode, Json<PluginStatusResponse>), Problem> {
    let plugins_dir = state.service.manager().config().plugins_dir.clone();
    // Derive the binary name: for "vibetemps" -> "temps-vibetemps-plugin"
    let binary_name = plugin_name_to_binary_name(&name);
    let binary_path = plugins_dir.join(&binary_name);
    let binary_present = binary_path.exists();
    let process_running = state.service.manager().is_running(&binary_name).await;
    let configured = binary_present && process_running;

    let response = if configured {
        PluginStatusResponse {
            configured: true,
            reason: None,
            setup_path: None,
        }
    } else if !binary_present {
        PluginStatusResponse {
            configured: false,
            reason: Some(format!(
                "The {} plugin is not installed. Install it from the plugin settings page.",
                name
            )),
            setup_path: Some("/settings/plugins".to_string()),
        }
    } else {
        // Binary present but process not running
        PluginStatusResponse {
            configured: false,
            reason: Some(format!(
                "The {} plugin binary is installed but the process is not running. \
                 Trigger a reload via the plugin settings page.",
                name
            )),
            setup_path: Some("/settings/plugins".to_string()),
        }
    };

    Ok((StatusCode::OK, Json(response)))
}

/// Map a user-facing plugin name to the binary filename on disk.
///
/// Convention: `"vibetemps"` -> `"temps-vibetemps-plugin"`.
/// Unknown names fall back to using the name as-is.
fn plugin_name_to_binary_name(name: &str) -> String {
    match name {
        "vibetemps" => VIBETEMPS_BINARY_NAME.to_string(),
        other => other.to_string(),
    }
}

/// Build the router for external plugin management endpoints.
pub fn configure_routes() -> Router<ExternalPluginsAppState> {
    Router::new()
        .route("/x/plugins", get(list_external_plugins))
        .route("/x/plugins/reload", post(reload_plugins))
        .route("/x/plugins/available", get(get_available_plugins))
        .route("/x/plugins/install", post(install_plugin))
        .route("/x/plugins/{name}/status", get(get_plugin_status))
}

#[derive(OpenApiTrait)]
#[openapi(
    paths(
        list_external_plugins,
        reload_plugins,
        get_available_plugins,
        install_plugin,
        get_plugin_status,
    ),
    components(
        schemas(
            PluginManifest,
            NavEntry,
            NavSection,
            UiManifest,
            UiRoute,
            ReloadResponse,
            PluginAvailabilityResponse,
            PluginRegistryManifest,
            PlatformAsset,
            InstallPluginRequest,
            InstallPluginResponse,
            PluginStatusResponse,
        )
    ),
    tags(
        (name = "External Plugins", description = "External plugin management and discovery")
    )
)]
pub struct ExternalPluginsApiDoc;

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::Utc;
    use temps_auth::context::AuthContext;
    use temps_auth::permissions::Role;
    use temps_entities::users;

    use crate::manager::ExternalPluginConfig;

    fn mock_db() -> Arc<sea_orm::DatabaseConnection> {
        Arc::new(sea_orm::MockDatabase::new(sea_orm::DatabaseBackend::Postgres).into_connection())
    }

    fn test_state() -> ExternalPluginsAppState {
        let config = ExternalPluginConfig::new(
            std::env::temp_dir().join("temps-external-plugins-handler-test"),
            "postgres://localhost/test".to_string(),
        );
        ExternalPluginsAppState {
            service: Arc::new(ExternalPluginsService::new_empty(config, None, mock_db())),
        }
    }

    fn test_user(id: i32) -> users::Model {
        let now = Utc::now();
        users::Model {
            id,
            name: "Test User".to_string(),
            email: format!("user{id}@example.com"),
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

    fn user_auth(role: Role) -> RequireAuth {
        RequireAuth(AuthContext::new_session(test_user(1), role))
    }

    // Regression tests for the unauthenticated-access finding: `reload_plugins`
    // stopped/restarted every plugin process and `list_external_plugins`
    // leaked the full plugin manifest to any caller because neither handler
    // had a `RequireAuth` extractor, despite the OpenAPI docs on this file
    // claiming `SystemAdmin` was required for reload.

    #[tokio::test]
    async fn reload_plugins_rejects_non_admin() {
        let state = test_state();
        let err = reload_plugins(user_auth(Role::User), State(state))
            .await
            .expect_err("a plain User role must not be able to reload plugins");
        assert_eq!(err.status_code, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn reload_plugins_allows_platform_admin() {
        let state = test_state();
        let (status, _) = reload_plugins(user_auth(Role::PlatformAdmin), State(state))
            .await
            .expect("a PlatformAdmin must be able to reload plugins");
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn list_external_plugins_allows_any_authenticated_role() {
        // Any signed-in user must be able to list plugins — the sidebar nav
        // for every authenticated user depends on this endpoint. Only
        // unauthenticated (no session at all) callers should be rejected,
        // which `RequireAuth`'s extractor enforces at the HTTP layer before
        // this handler body ever runs.
        let state = test_state();
        let Json(manifests) = list_external_plugins(user_auth(Role::User), State(state)).await;
        assert!(manifests.is_empty());
    }

    #[test]
    fn test_openapi_spec_has_plugins_path() {
        let spec = ExternalPluginsApiDoc::openapi();
        assert!(
            spec.paths.paths.contains_key("/x/plugins"),
            "OpenAPI spec must contain /x/plugins path"
        );
    }

    #[test]
    fn test_openapi_spec_has_schemas() {
        let spec = ExternalPluginsApiDoc::openapi();
        let components = spec.components.expect("should have components");
        assert!(
            components.schemas.contains_key("PluginManifest"),
            "OpenAPI spec must contain PluginManifest schema"
        );
        assert!(
            components.schemas.contains_key("NavEntry"),
            "OpenAPI spec must contain NavEntry schema"
        );
        assert!(
            components.schemas.contains_key("NavSection"),
            "OpenAPI spec must contain NavSection schema"
        );
    }

    #[test]
    fn test_openapi_spec_has_reload_path() {
        let spec = ExternalPluginsApiDoc::openapi();
        assert!(
            spec.paths.paths.contains_key("/x/plugins/reload"),
            "OpenAPI spec must contain /x/plugins/reload path"
        );
    }

    #[test]
    fn test_openapi_spec_has_reload_response_schema() {
        let spec = ExternalPluginsApiDoc::openapi();
        let components = spec.components.expect("should have components");
        assert!(
            components.schemas.contains_key("ReloadResponse"),
            "OpenAPI spec must contain ReloadResponse schema"
        );
    }

    #[test]
    fn test_reload_response_serialization() {
        let response = ReloadResponse {
            loaded: 2,
            plugins: vec!["seo-analyzer".into(), "monitoring".into()],
            message: "Reload complete. 2 plugin(s) loaded.".into(),
        };
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["loaded"], 2);
        assert_eq!(json["plugins"][0], "seo-analyzer");
        assert_eq!(json["plugins"][1], "monitoring");
    }
}
