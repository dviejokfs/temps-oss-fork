//! Capability and preference endpoints for the ADR-037 provider switching seam.
//!
//! `GET /api/ai/provider-status`  — returns the current routing preference and
//!   availability state so the UI can onboard instead of disappearing.
//!
//! `PUT /api/ai/provider-preference` — lets an operator switch the instance-level
//!   routing preference between the BYOK gateway and a subscription-backed agent
//!   CLI; emits an audit event on every write.

use anyhow::Result as AnyhowResult;
use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post, put},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use temps_auth::permission_guard;
use temps_auth::RequireAuth;
use temps_core::problemdetails::{Problem, ProblemDetails};
use temps_core::{problemdetails, AuditContext, AuditOperation, RequestMetadata};
use utoipa::{OpenApi, ToSchema};

use crate::handlers::types::AiGatewayAppState;
use crate::services::ProviderPreferenceError;

// ============================================================================
// Error conversion
// ============================================================================

impl From<ProviderPreferenceError> for Problem {
    fn from(error: ProviderPreferenceError) -> Self {
        match error {
            ProviderPreferenceError::Validation { .. } => {
                problemdetails::new(StatusCode::BAD_REQUEST)
                    .with_title("Validation Error")
                    .with_detail(error.to_string())
            }
            ProviderPreferenceError::Database(_) => {
                problemdetails::new(StatusCode::INTERNAL_SERVER_ERROR)
                    .with_title("Internal Server Error")
                    .with_detail(error.to_string())
            }
        }
    }
}

// ============================================================================
// OpenAPI schema
// ============================================================================

#[derive(OpenApi)]
#[openapi(
    paths(
        get_ai_provider_status,
        refresh_ai_provider_status,
        update_ai_provider_preference
    ),
    components(schemas(
        AiProviderStatusResponse,
        AvailableAiProviderDto,
        AiModelOptionDto,
        AiSelectOptionDto,
        AiCliStatusDto,
        UpdateProviderPreferenceRequest,
    )),
    info(
        title = "AI Provider Status API",
        description = "Inspect and update the instance-level AI provider preference (ADR-037)",
        version = "1.0.0"
    ),
    tags(
        (name = "AI Provider Status", description = "Provider preference and availability endpoints")
    )
)]
pub struct AiProviderStatusApiDoc;

pub fn configure_provider_status_routes() -> Router<Arc<AiGatewayAppState>> {
    Router::new()
        .route("/ai/provider-status", get(get_ai_provider_status))
        .route(
            "/ai/provider-status/refresh",
            post(refresh_ai_provider_status),
        )
        .route(
            "/ai/provider-preference",
            put(update_ai_provider_preference),
        )
}

// ============================================================================
// DTOs
// ============================================================================

/// Current AI provider routing preference and availability for this instance.
///
/// The `configured` field drives the UI onboarding state: when `false` the UI
/// must show _exactly what is missing_ (`reason`) and _where to fix it_
/// (`setup_path`), not hide the feature.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AiProviderStatusResponse {
    /// Active preference: `"gateway"` (BYOK) or `"agent_cli"` (subscription).
    pub active_provider_type: String,
    /// Catalog id of the active agent CLI provider, or `null` when
    /// `active_provider_type` is `"gateway"`.
    pub agent_cli_provider_id: Option<String>,
    /// Whether the active provider is ready to serve requests.
    pub configured: bool,
    /// Human-readable explanation of why `configured` is `false`.
    pub reason: Option<String>,
    /// Console path the operator should visit to fix the missing configuration.
    pub setup_path: Option<String>,
    /// Whether at least one active BYOK provider key exists.
    pub gateway_available: bool,
    /// Providers a chat user may choose for a new conversation. Authentication
    /// source is descriptive metadata only and never contains credentials.
    pub available_providers: Vec<AvailableAiProviderDto>,
    /// Live status of the selected agent CLI, or `null` when
    /// `active_provider_type` is `"gateway"`.
    pub agent_cli_status: Option<AiCliStatusDto>,
    /// Whether the active provider path supports mid-turn interactive tools
    /// (`AskUserQuestion`, `ExitPlanMode`, tool permission prompts).
    ///
    /// Truth table (ADR-038 Phase 2):
    /// - Gateway / BYOK provider: always `true` — function-calling is native.
    /// - Agent CLI, provider != `claude_cli`: always `false` — no control protocol.
    /// - Agent CLI, `claude_cli`, `interactive_bridge_enabled = false`: `false`.
    /// - Agent CLI, `claude_cli`, `interactive_bridge_enabled = true`: `true`.
    pub supports_interactive_tools: bool,
    /// Live health of the interactive bridge, when opted in.
    ///
    /// - `"healthy"`: bridge is enabled AND the Claude CLI reports
    ///   `host_authenticated = true`; tool approvals will route correctly.
    /// - `"unavailable"`: bridge is enabled but the CLI is not installed or not
    ///   authenticated; tool approvals cannot be bridged until auth is fixed.
    /// - `null`: the bridge is not opted in, or the active provider is not
    ///   `claude_cli` — the field is not meaningful in those cases.
    pub interactive_bridge_status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AvailableAiProviderDto {
    pub id: String,
    pub name: String,
    /// `configured_key` for the gateway or `host_environment` for an ambient
    /// CLI login discovered in the Temps process environment.
    pub auth_source: String,
    pub models: Vec<AiModelOptionDto>,
    pub default_model_id: Option<String>,
    /// `ready` when the model list was loaded, `unavailable` when the provider
    /// can still run with its own default but live discovery failed.
    pub model_discovery_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_discovery_error: Option<String>,
    pub permission_modes: Vec<AiSelectOptionDto>,
    pub default_permission_mode_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AiModelOptionDto {
    pub id: String,
    pub name: String,
    pub thinking_options: Vec<AiSelectOptionDto>,
    pub default_thinking_option_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AiSelectOptionDto {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

fn option(id: &str, name: &str, description: &str) -> AiSelectOptionDto {
    AiSelectOptionDto {
        id: id.to_string(),
        name: name.to_string(),
        description: Some(description.to_string()),
    }
}

fn thinking_options(provider_id: &str, model: &str) -> (Vec<AiSelectOptionDto>, Option<String>) {
    if provider_id == "claude_cli" {
        if model.contains("haiku") {
            return (Vec::new(), None);
        }
        let supports_xhigh = matches!(model, "sonnet" | "opus")
            || model.contains("-5")
            || model.contains("-4-7")
            || model.contains("-4-8");
        let mut options = vec![
            option("off", "Off", "Disable extended thinking for this model"),
            option("low", "Low", "Use a small reasoning budget"),
            option("medium", "Medium", "Use a balanced reasoning budget"),
            option("high", "High", "Use a larger reasoning budget"),
        ];
        if supports_xhigh {
            options.push(option(
                "xhigh",
                "Extra high",
                "Use the model's extra-high reasoning budget",
            ));
        }
        options.push(option("max", "Max", "Use the maximum reasoning budget"));
        if supports_xhigh {
            options.push(option(
                "ultracode",
                "Ultra code",
                "Use extra-high effort with Claude's Ultracode mode",
            ));
        }
        return (options, Some("high".to_string()));
    }
    let (ids, default): (&[(&str, &str)], &str) = match provider_id {
        "codex_cli" if model.contains("mini") => (
            &[("low", "Low"), ("medium", "Medium"), ("high", "High")],
            "medium",
        ),
        "codex_cli" => (
            &[
                ("low", "Low"),
                ("medium", "Medium"),
                ("high", "High"),
                ("xhigh", "Extra high"),
            ],
            "high",
        ),
        "opencode" => (&[("default", "Default"), ("high", "High")], "default"),
        "openai" if model.starts_with("gpt-5") || model.starts_with('o') => (
            &[
                ("low", "Low"),
                ("medium", "Medium"),
                ("high", "High"),
                ("xhigh", "Extra high"),
            ],
            "medium",
        ),
        _ => return (Vec::new(), None),
    };
    (
        ids.iter()
            .map(|(id, name)| option(id, name, "Provider-specific reasoning depth"))
            .collect(),
        Some(default.to_string()),
    )
}

fn permission_options(provider_id: &str) -> (Vec<AiSelectOptionDto>, Option<String>) {
    let modes = match provider_id {
        "claude_cli" => vec![
            option("default", "Default", "Ask before sensitive actions"),
            option(
                "accept-edits",
                "Accept edits",
                "Automatically approve file edits",
            ),
            option("plan", "Plan", "Plan without making changes"),
            option("full-access", "Full access", "Bypass permission prompts"),
        ],
        "codex_cli" => vec![
            option(
                "auto",
                "Default permissions",
                "Automatic approvals in a workspace-write sandbox",
            ),
            option(
                "auto-review",
                "Auto-review",
                "Ask for approval when the CLI judges it necessary",
            ),
            option(
                "full-access",
                "Full access",
                "Disable sandbox and approval prompts",
            ),
        ],
        "opencode" => vec![
            option("build", "Build", "Use OpenCode's build agent"),
            option("plan", "Plan", "Use OpenCode's read-only planning agent"),
        ],
        _ => vec![option(
            "confirm-actions",
            "Confirm actions",
            "Require confirmation for write actions",
        )],
    };
    let default = match provider_id {
        "codex_cli" => "auto",
        "opencode" => "build",
        "claude_cli" => "default",
        _ => "confirm-actions",
    };
    (modes, Some(default.to_string()))
}

async fn cli_provider_option(provider_id: &str) -> Option<AvailableAiProviderDto> {
    let catalog = temps_agents::ai_cli::find_provider(provider_id)?;
    let models = temps_agents::ai_cli::discover_model_capabilities(provider_id)
        .await
        .into_iter()
        .map(|model| AiModelOptionDto {
            id: model.id,
            name: model.name,
            thinking_options: model
                .reasoning_options
                .iter()
                .map(|option_id| {
                    option(
                        option_id,
                        &display_option_name(option_id),
                        "Supported by this model",
                    )
                })
                .collect(),
            default_thinking_option_id: model.default_reasoning_option,
        })
        .collect::<Vec<_>>();
    let model_discovery_status = if models.is_empty() {
        "unavailable"
    } else {
        "ready"
    };
    let model_discovery_error = models.is_empty().then(|| {
        format!(
            "Could not query {} for its current model list. The CLI default remains usable; retry provider discovery to load model controls.",
            catalog.name
        )
    });
    let (permission_modes, default_permission_mode_id) = permission_options(provider_id);
    Some(AvailableAiProviderDto {
        id: provider_id.to_string(),
        name: catalog.name.to_string(),
        auth_source: "host_environment".to_string(),
        models,
        default_model_id: None,
        model_discovery_status: model_discovery_status.to_string(),
        model_discovery_error,
        permission_modes,
        default_permission_mode_id,
    })
}

fn display_option_name(id: &str) -> String {
    match id {
        "xhigh" => "Extra high".to_string(),
        "none" => "None".to_string(),
        value => {
            let mut characters = value.chars();
            characters
                .next()
                .map(|first| first.to_uppercase().collect::<String>() + characters.as_str())
                .unwrap_or_default()
        }
    }
}

/// Mirrors `temps_agents::ai_cli::AiCliStatus` with utoipa `ToSchema` added.
/// `AiCliStatus` itself does not derive `ToSchema`, so this local projection is
/// used for OpenAPI generation only — the fields are identical.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AiCliStatusDto {
    pub provider: String,
    pub installed: bool,
    pub version: Option<String>,
    pub authenticated: bool,
    pub auth_method: Option<String>,
    pub subscription_type: Option<String>,
    /// Instructions for the operator when not installed or not authenticated.
    pub setup_hint: Option<String>,
}

const PROVIDER_STATUS_CACHE_TTL: Duration = Duration::from_secs(60);

struct CachedProviderStatus {
    cached_at: Instant,
    response: AiProviderStatusResponse,
}

/// Short-lived capability snapshot. CLI authentication/model discovery can
/// involve several subprocesses, so it must never run on every page render.
pub struct AiProviderStatusCache {
    value: tokio::sync::RwLock<Option<CachedProviderStatus>>,
    refresh: tokio::sync::Mutex<()>,
    generation: AtomicU64,
}

impl Default for AiProviderStatusCache {
    fn default() -> Self {
        Self {
            value: tokio::sync::RwLock::new(None),
            refresh: tokio::sync::Mutex::new(()),
            generation: AtomicU64::new(0),
        }
    }
}

impl AiProviderStatusCache {
    async fn get(&self) -> Option<AiProviderStatusResponse> {
        self.value.read().await.as_ref().and_then(|cached| {
            (cached.cached_at.elapsed() < PROVIDER_STATUS_CACHE_TTL)
                .then(|| cached.response.clone())
        })
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    async fn store_if_current(&self, generation: u64, response: AiProviderStatusResponse) -> bool {
        let mut value = self.value.write().await;
        if self.generation() != generation {
            return false;
        }
        *value = Some(CachedProviderStatus {
            cached_at: Instant::now(),
            response,
        });
        true
    }

    pub async fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        *self.value.write().await = None;
    }
}

impl From<temps_agents::ai_cli::AiCliStatus> for AiCliStatusDto {
    fn from(s: temps_agents::ai_cli::AiCliStatus) -> Self {
        Self {
            provider: s.provider,
            installed: s.installed,
            version: s.version,
            authenticated: s.authenticated,
            auth_method: s.auth_method,
            subscription_type: s.subscription_type,
            setup_hint: s.setup_hint,
        }
    }
}

/// Request body for `PUT /api/ai/provider-preference`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateProviderPreferenceRequest {
    /// `"gateway"` or `"agent_cli"`.
    pub provider_type: String,
    /// Required when `provider_type` is `"agent_cli"`.
    pub agent_cli_provider_id: Option<String>,
    /// Opt in to the interactive Claude CLI bridge (ADR-038 Phase 2).
    ///
    /// When `true`, `ConversationService` routes chat turns through
    /// `ClaudeCliProvider::run_interactive` (long-lived subprocess with
    /// `--permission-prompt-tool stdio`) instead of the one-shot
    /// `--dangerously-skip-permissions` path, enabling mid-turn tool
    /// approval, `AskUserQuestion`, and `ExitPlanMode` (milestone 3+).
    ///
    /// Setting this to `true` is only valid when `provider_type == "agent_cli"`
    /// AND `agent_cli_provider_id == "claude_cli"` — Codex and OpenCode have
    /// no equivalent interactive control protocol.  Omitting the field
    /// (`null`) preserves the existing toggle value in the database.
    pub interactive_bridge_enabled: Option<bool>,
}

// ============================================================================
// Audit event
// ============================================================================

#[derive(Debug, Clone, Serialize)]
struct ProviderPreferenceUpdatedAudit {
    context: AuditContext,
    scope: String,
    provider_type: String,
    agent_cli_provider_id: Option<String>,
}

impl AuditOperation for ProviderPreferenceUpdatedAudit {
    fn operation_type(&self) -> String {
        "ai_gateway.provider_preference.updated".to_string()
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

    fn serialize(&self) -> AnyhowResult<String> {
        serde_json::to_string(self)
            .map_err(|e| anyhow::anyhow!("Failed to serialize audit operation: {e}"))
    }
}

// ============================================================================
// Shared helper
// ============================================================================

/// Assemble the full `AiProviderStatusResponse` from the current preference row
/// and live provider checks.  Shared between GET and the PUT response so the
/// two handlers never diverge.
async fn build_status_response(
    app_state: &AiGatewayAppState,
) -> Result<AiProviderStatusResponse, Problem> {
    // Chat may advertise and execute a host CLI only after an administrator
    // explicitly selected it. Ambient authentication proves availability, not
    // authorization to expose the host process to conversation prompts.
    let preference = app_state
        .provider_preference_service
        .get("instance")
        .await
        .map_err(Problem::from)?;
    let agent_cli_provider_id = preference
        .as_ref()
        .filter(|row| row.provider_type == "agent_cli")
        .and_then(|row| row.agent_cli_provider_id.clone());
    let active_provider_type = if agent_cli_provider_id.is_some() {
        "agent_cli".to_string()
    } else {
        "gateway".to_string()
    };

    // Determine gateway availability: at least one active provider key exists.
    let active_keys = app_state
        .provider_key_service
        .list_active()
        .await
        .map_err(Problem::from)?;
    let gateway_available = !active_keys.is_empty();
    let mut available_providers = Vec::new();
    for key in &active_keys {
        let adapter_models = app_state
            .gateway_service
            .available_models_for_provider(&key.provider);
        let mut model_ids: Vec<String> = adapter_models
            .into_iter()
            .map(|m| m.id)
            .filter(|id| !id.starts_with("text-embedding-"))
            .collect();
        if let Some(default_model) = key.default_model.as_deref().filter(|m| !m.is_empty()) {
            if !model_ids.iter().any(|model| model == default_model) {
                model_ids.insert(0, default_model.to_string());
            }
        }
        let models = model_ids
            .iter()
            .map(|model| {
                let (thinking_options, default_thinking_option_id) =
                    thinking_options(&key.provider, model);
                AiModelOptionDto {
                    id: model.clone(),
                    name: model.clone(),
                    thinking_options,
                    default_thinking_option_id,
                }
            })
            .collect();
        let (permission_modes, default_permission_mode_id) = permission_options(&key.provider);
        available_providers.push(AvailableAiProviderDto {
            id: format!("gateway_key:{}", key.id),
            name: key.display_name.clone(),
            auth_source: "configured_key".to_string(),
            models,
            default_model_id: key
                .default_model
                .clone()
                .or_else(|| model_ids.first().cloned()),
            model_discovery_status: "ready".to_string(),
            model_discovery_error: None,
            permission_modes,
            default_permission_mode_id,
        });
    }
    // Probe independent CLIs concurrently. A cold cache is bounded by the
    // slowest provider rather than the sum of every subprocess timeout.
    let cli_probes =
        futures_util::future::join_all(temps_agents::ai_cli::PROVIDER_NAMES.iter().map(
            |(provider_id, _label)| async move {
                let provider = temps_agents::ai_cli::create_provider(provider_id)?;
                let (status, option) =
                    tokio::join!(provider.get_status(), cli_provider_option(provider_id));
                Some(((*provider_id).to_string(), status, option))
            },
        ))
        .await;
    let mut cli_statuses = HashMap::new();
    for (provider_id, status, option) in cli_probes.into_iter().flatten() {
        if status.installed && status.authenticated {
            if let Some(option) = option {
                available_providers.push(option);
            }
        }
        cli_statuses.insert(provider_id, status);
    }

    let (configured, reason, setup_path, agent_cli_status) = if active_provider_type == "agent_cli"
    {
        let provider_id = agent_cli_provider_id.as_deref().unwrap_or("");
        match cli_statuses.get(provider_id).cloned() {
            Some(status) => {
                let configured = status.installed && status.authenticated;
                let reason = if !configured {
                    Some(if !status.installed {
                        format!(
                            "The {} CLI is not installed on the Temps host",
                            status.provider
                        )
                    } else {
                        format!(
                            "The {} CLI is installed but not authenticated",
                            status.provider
                        )
                    })
                } else {
                    None
                };
                let setup_path = if !configured {
                    Some("/agent-sandbox/providers".to_string())
                } else {
                    None
                };
                let dto = AiCliStatusDto::from(status);
                (configured, reason, setup_path, Some(dto))
            }
            None => {
                // Stored id doesn't match any known provider — surface as
                // misconfigured so the operator can fix it.
                (
                        false,
                        Some(format!(
                            "Unknown agent CLI provider '{}' — update the preference to a valid provider id",
                            provider_id
                        )),
                        Some("/agent-sandbox/providers".to_string()),
                        None,
                    )
            }
        }
    } else {
        // Gateway path.
        let (configured, reason, setup_path) = if gateway_available {
            (true, None, None)
        } else {
            (
                false,
                Some("No AI provider key is configured".to_string()),
                Some("/ai-gateway".to_string()),
            )
        };
        (configured, reason, setup_path, None)
    };

    // Read `interactive_bridge_enabled` from the raw config row (not surfaced by
    // `resolve_active_agent_cli_provider`, which only returns the provider id).
    let interactive_bridge_enabled = match app_state
        .provider_preference_service
        .get("instance")
        .await
        .map_err(Problem::from)?
    {
        Some(row) => row.interactive_bridge_enabled,
        None => false,
    };

    // Compute `supports_interactive_tools` based on the full truth table:
    //   - Gateway / BYOK: always true (native function-calling).
    //   - Agent CLI, not claude_cli: always false (no control protocol).
    //   - Agent CLI, claude_cli, bridge disabled: false.
    //   - Agent CLI, claude_cli, bridge enabled: true.
    let is_claude_cli_active = active_provider_type == "agent_cli"
        && agent_cli_provider_id.as_deref() == Some("claude_cli");
    let supports_interactive_tools =
        active_provider_type == "gateway" || (is_claude_cli_active && interactive_bridge_enabled);

    // Derive `interactive_bridge_status` — DB-state only, no live subprocess
    // health check.  Only meaningful when the bridge is opted in.
    let interactive_bridge_status = if is_claude_cli_active && interactive_bridge_enabled {
        // Determine if the CLI is currently authenticated so we can report
        // "healthy" vs. "unavailable". The CLI status is already available in
        // `agent_cli_status` when `active_provider_type == "agent_cli"`.
        let authenticated = agent_cli_status
            .as_ref()
            .map(|s| s.authenticated)
            .unwrap_or(false);
        if authenticated {
            Some("healthy".to_string())
        } else {
            Some("unavailable".to_string())
        }
    } else {
        None
    };

    Ok(AiProviderStatusResponse {
        active_provider_type,
        agent_cli_provider_id,
        configured,
        reason,
        setup_path,
        gateway_available,
        available_providers,
        agent_cli_status,
        supports_interactive_tools,
        interactive_bridge_status,
    })
}

async fn cached_status_response(
    app_state: &AiGatewayAppState,
) -> Result<AiProviderStatusResponse, Problem> {
    if let Some(response) = app_state.provider_status_cache.get().await {
        return Ok(response);
    }

    // Single-flight cold/expired refresh: concurrent page mounts wait for the
    // same probe instead of each launching their own CLI processes.
    let _refresh = app_state.provider_status_cache.refresh.lock().await;
    if let Some(response) = app_state.provider_status_cache.get().await {
        return Ok(response);
    }
    let generation = app_state.provider_status_cache.generation();
    let response = build_status_response(app_state).await?;
    app_state
        .provider_status_cache
        .store_if_current(generation, response.clone())
        .await;
    Ok(response)
}

// ============================================================================
// Handlers
// ============================================================================

#[utoipa::path(
    tag = "AI Provider Status",
    get,
    path = "/ai/provider-status",
    responses(
        (status = 200, description = "Current provider preference and availability", body = AiProviderStatusResponse),
        (status = 401, description = "Unauthorized", body = ProblemDetails),
        (status = 403, description = "Insufficient permissions", body = ProblemDetails),
        (status = 500, description = "Internal server error", body = ProblemDetails)
    ),
    security(("bearer_auth" = []))
)]
async fn get_ai_provider_status(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<AiGatewayAppState>>,
) -> Result<impl IntoResponse, Problem> {
    permission_guard!(auth, AiGatewayRead);

    let response = cached_status_response(&app_state).await?;
    Ok(Json(response))
}

#[utoipa::path(
    tag = "AI Provider Status",
    post,
    path = "/ai/provider-status/refresh",
    responses(
        (status = 200, description = "Fresh provider authentication and model capability snapshot", body = AiProviderStatusResponse),
        (status = 401, description = "Unauthorized", body = ProblemDetails),
        (status = 403, description = "Insufficient permissions", body = ProblemDetails),
        (status = 500, description = "Provider refresh failed", body = ProblemDetails)
    ),
    security(("bearer_auth" = []))
)]
async fn refresh_ai_provider_status(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<AiGatewayAppState>>,
) -> Result<impl IntoResponse, Problem> {
    // A forced refresh launches authenticated host CLI subprocesses. Keep it
    // behind the settings write permission so read-only users cannot use the
    // endpoint to repeatedly consume host resources.
    permission_guard!(auth, AiGatewayWrite);

    app_state.provider_status_cache.invalidate().await;
    let response = cached_status_response(&app_state).await?;
    Ok(Json(response))
}

#[utoipa::path(
    tag = "AI Provider Status",
    put,
    path = "/ai/provider-preference",
    request_body = UpdateProviderPreferenceRequest,
    responses(
        (status = 200, description = "Updated provider preference and availability", body = AiProviderStatusResponse),
        (status = 400, description = "Validation error", body = ProblemDetails),
        (status = 401, description = "Unauthorized", body = ProblemDetails),
        (status = 403, description = "Insufficient permissions", body = ProblemDetails),
        (status = 500, description = "Internal server error", body = ProblemDetails)
    ),
    security(("bearer_auth" = []))
)]
async fn update_ai_provider_preference(
    RequireAuth(auth): RequireAuth,
    State(app_state): State<Arc<AiGatewayAppState>>,
    axum::extract::Extension(metadata): axum::extract::Extension<RequestMetadata>,
    Json(request): Json<UpdateProviderPreferenceRequest>,
) -> Result<impl IntoResponse, Problem> {
    permission_guard!(auth, AiGatewayWrite);

    app_state
        .provider_preference_service
        .set(
            "instance",
            request.provider_type.clone(),
            request.agent_cli_provider_id.clone(),
            request.interactive_bridge_enabled,
        )
        .await
        .map_err(Problem::from)?;
    app_state.provider_status_cache.invalidate().await;

    // Audit the write — failure is logged but does not fail the request.
    let audit = ProviderPreferenceUpdatedAudit {
        context: AuditContext {
            user_id: auth.user_id(),
            ip_address: Some(metadata.ip_address.clone()),
            user_agent: metadata.user_agent.clone(),
        },
        scope: "instance".to_string(),
        provider_type: request.provider_type.clone(),
        agent_cli_provider_id: request.agent_cli_provider_id.clone(),
    };
    if let Err(e) = app_state.audit_service.create_audit_log(&audit).await {
        tracing::error!(
            provider_type = %request.provider_type,
            "Failed to create audit log for provider preference update: {e}"
        );
    }

    let response = cached_status_response(&app_state).await?;
    Ok(Json(response))
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn cached_response() -> AiProviderStatusResponse {
        AiProviderStatusResponse {
            active_provider_type: "gateway".to_string(),
            agent_cli_provider_id: None,
            configured: true,
            reason: None,
            setup_path: None,
            gateway_available: true,
            available_providers: Vec::new(),
            agent_cli_status: None,
            supports_interactive_tools: true,
            interactive_bridge_status: None,
        }
    }

    #[tokio::test]
    async fn provider_status_cache_returns_and_invalidates_snapshot() {
        let cache = AiProviderStatusCache::default();
        cache
            .store_if_current(cache.generation(), cached_response())
            .await;
        assert!(cache.get().await.is_some());
        cache.invalidate().await;
        assert!(cache.get().await.is_none());
    }

    #[tokio::test]
    async fn invalidation_discards_an_in_flight_refresh_result() {
        let cache = AiProviderStatusCache::default();
        let refresh_generation = cache.generation();
        cache.invalidate().await;
        assert!(
            !cache
                .store_if_current(refresh_generation, cached_response())
                .await
        );
        assert!(cache.get().await.is_none());
    }

    #[tokio::test]
    async fn provider_status_cache_expires_old_snapshot() {
        let cache = AiProviderStatusCache::default();
        *cache.value.write().await = Some(CachedProviderStatus {
            cached_at: Instant::now() - PROVIDER_STATUS_CACHE_TTL,
            response: cached_response(),
        });
        assert!(cache.get().await.is_none());
    }

    #[test]
    fn claude_thinking_options_are_model_specific() {
        let (latest, default) = thinking_options("claude_cli", "sonnet");
        assert_eq!(
            latest
                .iter()
                .map(|option| option.id.as_str())
                .collect::<Vec<_>>(),
            ["off", "low", "medium", "high", "xhigh", "max", "ultracode"]
        );
        assert_eq!(default.as_deref(), Some("high"));

        let (sonnet_46, _) = thinking_options("claude_cli", "claude-sonnet-4-6");
        assert_eq!(
            sonnet_46
                .iter()
                .map(|option| option.id.as_str())
                .collect::<Vec<_>>(),
            ["off", "low", "medium", "high", "max"]
        );
        assert!(thinking_options("claude_cli", "haiku").0.is_empty());
    }

    /// Truth table for `supports_interactive_tools` and
    /// `interactive_bridge_status` (ADR-038 Phase 2, deliverable 3).
    ///
    /// These are pure boolean / string computations extracted from
    /// `build_status_response` so they can be verified without a DB or
    /// running server.
    fn compute_supports_interactive_tools(
        active_provider_type: &str,
        agent_cli_provider_id: Option<&str>,
        interactive_bridge_enabled: bool,
    ) -> bool {
        let is_claude_cli_active =
            active_provider_type == "agent_cli" && agent_cli_provider_id == Some("claude_cli");
        active_provider_type == "gateway" || (is_claude_cli_active && interactive_bridge_enabled)
    }

    fn compute_interactive_bridge_status(
        active_provider_type: &str,
        agent_cli_provider_id: Option<&str>,
        interactive_bridge_enabled: bool,
        cli_authenticated: bool,
    ) -> Option<String> {
        let is_claude_cli_active =
            active_provider_type == "agent_cli" && agent_cli_provider_id == Some("claude_cli");
        if is_claude_cli_active && interactive_bridge_enabled {
            if cli_authenticated {
                Some("healthy".to_string())
            } else {
                Some("unavailable".to_string())
            }
        } else {
            None
        }
    }

    // --- supports_interactive_tools truth table ---

    #[test]
    fn gateway_always_supports_interactive_tools() {
        assert!(compute_supports_interactive_tools("gateway", None, false));
        assert!(compute_supports_interactive_tools("gateway", None, true));
    }

    #[test]
    fn claude_cli_with_bridge_enabled_supports_interactive_tools() {
        assert!(compute_supports_interactive_tools(
            "agent_cli",
            Some("claude_cli"),
            true
        ));
    }

    #[test]
    fn claude_cli_with_bridge_disabled_does_not_support_interactive_tools() {
        assert!(!compute_supports_interactive_tools(
            "agent_cli",
            Some("claude_cli"),
            false
        ));
    }

    #[test]
    fn non_claude_cli_provider_does_not_support_interactive_tools() {
        // codex_cli and opencode have no permission-prompt-tool protocol.
        assert!(!compute_supports_interactive_tools(
            "agent_cli",
            Some("codex_cli"),
            true
        ));
        assert!(!compute_supports_interactive_tools(
            "agent_cli",
            Some("opencode"),
            true
        ));
    }

    // --- interactive_bridge_status truth table ---

    #[test]
    fn bridge_status_is_none_for_gateway() {
        assert_eq!(
            compute_interactive_bridge_status("gateway", None, false, true),
            None
        );
    }

    #[test]
    fn bridge_status_is_none_when_bridge_disabled() {
        assert_eq!(
            compute_interactive_bridge_status("agent_cli", Some("claude_cli"), false, true),
            None
        );
    }

    #[test]
    fn bridge_status_healthy_when_enabled_and_authenticated() {
        assert_eq!(
            compute_interactive_bridge_status("agent_cli", Some("claude_cli"), true, true),
            Some("healthy".to_string())
        );
    }

    #[test]
    fn bridge_status_unavailable_when_enabled_but_not_authenticated() {
        assert_eq!(
            compute_interactive_bridge_status("agent_cli", Some("claude_cli"), true, false),
            Some("unavailable".to_string())
        );
    }
}
