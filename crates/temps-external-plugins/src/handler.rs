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

/// A plugin this instance knows how to fetch and install: its registry
/// manifest URL and the binary filename it installs as inside the plugins
/// directory.
///
/// `name` is the identity key everywhere it matters — it is the plugin's own
/// manifest-declared name once running, and `ExternalPluginManager` indexes
/// running processes by exactly this string (see
/// `ExternalPluginManager::start_plugin`, which inserts under
/// `result_manifest.name`). Any status/reload/is_running check MUST use
/// `name`, never a derived binary filename — a prior version of this code
/// checked `is_running(&binary_name)`, which never matched the manager's key
/// and made a running plugin permanently report as "not configured", and
/// made the install flow always take the "start fresh" branch instead of
/// "reload", leaking the old process (it was silently overwritten in the
/// process map without ever being shut down).
struct KnownPlugin {
    /// User-facing plugin name and the manager's process-table key.
    name: &'static str,
    /// Registry manifest URL. This is the **only** URL ever fetched for this
    /// plugin — never taken from an untrusted caller, to prevent SSRF / RCE.
    /// If the release host changes this must be updated and the binary
    /// redeployed.
    manifest_url: &'static str,
    /// Binary filename as it appears inside the release tarball and as it is
    /// written into the plugins directory.
    binary_name: &'static str,
}

/// Fixed set of plugins this instance knows how to install. Intentionally
/// small and compile-time — this is not a general marketplace where a caller
/// picks an arbitrary URL, both because the manifest is the trust root for
/// the whole install flow (its asset URLs and SHA-256 digests are what get
/// downloaded and executed) and because every self-hosted instance needs the
/// same known-good set. Add an entry here to make a new plugin installable.
const KNOWN_PLUGINS: &[KnownPlugin] = &[KnownPlugin {
    name: "vibetemps",
    // Served by temps.sh rather than a code-hosting release page: the
    // manifest is the trust root for the whole install flow, so it has to
    // live on a host we control and can serve to every self-hosted instance.
    manifest_url: "https://temps.sh/api/plugins/vibetemps/manifest.json",
    binary_name: "temps-vibetemps-plugin",
}];

fn known_plugin(name: &str) -> Option<&'static KnownPlugin> {
    KNOWN_PLUGINS.iter().find(|p| p.name == name)
}

fn known_plugin_names() -> String {
    KNOWN_PLUGINS
        .iter()
        .map(|p| p.name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// One entry in the installable-plugin registry listing.
#[derive(Debug, Serialize, ToSchema)]
pub struct PluginRegistryEntry {
    /// Plugin name (registry key).
    pub name: String,
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
    /// Name of the plugin to install — must match a `KNOWN_PLUGINS` entry.
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

/// List every plugin this instance knows how to install, whether it's
/// already installed, and its registry manifest.
///
/// Iterates `KNOWN_PLUGINS` — today that's a single entry (VibeTemps), but
/// the endpoint returns a list rather than a singular "the one plugin"
/// response so adding a second installable plugin never needs an API
/// change, only a new `KNOWN_PLUGINS` entry. Requires `SystemAdmin`
/// permission.
#[utoipa::path(
    tag = "External Plugins",
    get,
    path = "/x/plugins/registry",
    responses(
        (status = 200, description = "Installable-plugin registry", body = Vec<PluginRegistryEntry>),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Insufficient permissions"),
    ),
    security(("bearer_auth" = []))
)]
async fn list_plugin_registry(
    RequireAuth(auth): RequireAuth,
    State(state): State<ExternalPluginsAppState>,
) -> Result<(StatusCode, Json<Vec<PluginRegistryEntry>>), Problem> {
    permission_guard!(auth, SystemAdmin);

    let plugins_dir = state.service.manager().config().plugins_dir.clone();
    let mut entries = Vec::with_capacity(KNOWN_PLUGINS.len());

    for known in KNOWN_PLUGINS {
        let installed = plugins_dir.join(known.binary_name).exists();
        let (manifest, reason) = match PluginInstaller::fetch_manifest(known.manifest_url).await {
            Ok(manifest) => (Some(manifest), None),
            Err(e) => (
                None,
                Some(format!(
                    "Could not fetch {} manifest from {}: {}",
                    known.name, known.manifest_url, e
                )),
            ),
        };
        entries.push(PluginRegistryEntry {
            name: known.name.to_string(),
            installed,
            manifest,
            reason,
        });
    }

    Ok((StatusCode::OK, Json(entries)))
}

/// Download, verify, and install an external plugin binary.
///
/// `name` must match a `KNOWN_PLUGINS` entry. After a successful install the
/// plugin process is (re)started automatically. Requires `SystemAdmin`
/// permission.
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

    let known = known_plugin(&request.name).ok_or_else(|| {
        error_builder::bad_request()
            .title("Unsupported Plugin")
            .detail(format!(
                "'{}' is not a known installable plugin. Known plugins: {}.",
                request.name,
                known_plugin_names()
            ))
            .build()
    })?;

    let manifest = PluginInstaller::fetch_manifest(known.manifest_url)
        .await
        .map_err(|e| {
            error_builder::internal_server_error()
                .title("Manifest Fetch Failed")
                .detail(format!(
                    "Could not fetch {} manifest: {}. Check network connectivity and try again.",
                    known.name, e
                ))
                .build()
        })?;

    let plugins_dir = state.service.manager().config().plugins_dir.clone();
    let installer = PluginInstaller::new();

    let installed_path = installer
        .install(known.binary_name, &manifest, &plugins_dir)
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
    // installed; operator can trigger a manual reload). `manifest.name` (the
    // plugin's own declared identity, e.g. "vibetemps") is what
    // ExternalPluginManager indexes running processes by — NOT
    // `known.binary_name` — so it must be the identity passed here for
    // is_running/reload to find the right entry. `known.binary_name` is only
    // needed for the fresh-start filesystem path.
    let reloaded = match state
        .service
        .start_or_reload_plugin(&manifest.name, known.binary_name)
        .await
    {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!(
                plugin = %manifest.name,
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
                    "{} plugin v{} installed and started successfully.",
                    manifest.name, manifest.version
                )
            } else {
                format!(
                    "{} plugin v{} installed. Process start failed — use POST /x/plugins/reload to activate it.",
                    manifest.name, manifest.version
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
    // The manager indexes running processes by the plugin's own
    // manifest-declared name (see ExternalPluginManager::start_plugin, which
    // inserts under `result_manifest.name`) — so `name` itself, not a
    // derived binary filename, is the correct key here.
    if state.service.manager().is_running(&name).await {
        return Ok((
            StatusCode::OK,
            Json(PluginStatusResponse {
                configured: true,
                reason: None,
                setup_path: None,
            }),
        ));
    }

    // Not running. For a plugin we know how to install, distinguish "never
    // installed" from "binary present but process not running" using the
    // registry's known binary filename. For anything else (a plugin dropped
    // in manually, outside the install flow) we have no reliable filename to
    // check, so just report that it isn't running.
    let reason = match known_plugin(&name) {
        Some(known) => {
            let plugins_dir = state.service.manager().config().plugins_dir.clone();
            if plugins_dir.join(known.binary_name).exists() {
                format!(
                    "The {} plugin binary is installed but the process is not running. \
                     Trigger a reload via the plugin settings page.",
                    name
                )
            } else {
                format!(
                    "The {} plugin is not installed. Install it from the plugin settings page.",
                    name
                )
            }
        }
        None => format!("The {} plugin is not currently running.", name),
    };

    Ok((
        StatusCode::OK,
        Json(PluginStatusResponse {
            configured: false,
            reason: Some(reason),
            setup_path: Some("/settings/plugins".to_string()),
        }),
    ))
}

/// Build the router for external plugin management endpoints.
pub fn configure_routes() -> Router<ExternalPluginsAppState> {
    Router::new()
        .route("/x/plugins", get(list_external_plugins))
        .route("/x/plugins/reload", post(reload_plugins))
        .route("/x/plugins/registry", get(list_plugin_registry))
        .route("/x/plugins/install", post(install_plugin))
        .route("/x/plugins/{name}/status", get(get_plugin_status))
}

#[derive(OpenApiTrait)]
#[openapi(
    paths(
        list_external_plugins,
        reload_plugins,
        list_plugin_registry,
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
            PluginRegistryEntry,
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
