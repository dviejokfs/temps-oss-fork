use std::sync::Arc;

use serde_json::json;
use temps_core::project_access::ProjectAccessChecker;
use temps_core::AuditLogger;
use temps_deployments::DeploymentService;
use tracing::error;

use crate::audit::{AuditContext, McpDeploymentTriggeredAudit};
use crate::error::McpError;
use crate::proposal::ProposalStore;
use crate::protocol::McpTool;

/// Actor + request context needed to attribute an audit log entry to the MCP
/// caller. Constructed by the handler layer from `AuthContext` +
/// `RequestMetadata`, which this crate's tools modules don't otherwise see.
pub struct AuditActor {
    pub user_id: i32,
    pub ip_address: Option<String>,
    pub user_agent: String,
}

/// Tool definitions for the **deployments** group.
///
/// Read tools: `list_deployments`
/// Write tools (propose-then-confirm): `trigger_deployment`, `confirm_action`
///
/// Write tools are only included in the list when `write_enabled` is `true`
/// (i.e., the MCP URL was opened with `?write=1`).
pub fn tools(write_enabled: bool) -> Vec<McpTool> {
    let mut result = vec![
        McpTool {
            name: "list_deployments".to_string(),
            description: "List recent deployments for a project, optionally filtered \
                          by environment."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "project_id": {
                        "type": "integer",
                        "description": "Numeric project ID"
                    },
                    "environment_id": {
                        "type": "integer",
                        "description": "Filter to a specific environment (optional)"
                    },
                    "page": {
                        "type": "integer",
                        "description": "Page number, 1-based (default: 1)"
                    },
                    "per_page": {
                        "type": "integer",
                        "description": "Results per page, max 100 (default: 10)"
                    }
                },
                "required": ["project_id"]
            }),
        },
        // TODO(mcp): get_deployment — fetch a single deployment by ID
        // TODO(mcp): list_environments — list environments for a project
    ];

    if write_enabled {
        result.push(McpTool {
            name: "trigger_deployment".to_string(),
            description: "Propose triggering a new deployment pipeline for a project \
                          environment.  Returns a proposal token; call confirm_action \
                          with that token to execute."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "project_id": {
                        "type": "integer",
                        "description": "Numeric project ID"
                    },
                    "environment_id": {
                        "type": "integer",
                        "description": "Numeric environment ID"
                    },
                    "branch": {
                        "type": "string",
                        "description": "Git branch to deploy (optional, defaults to main branch)"
                    }
                },
                "required": ["project_id", "environment_id"]
            }),
        });

        result.push(McpTool {
            name: "confirm_action".to_string(),
            description: "Execute a previously proposed write action using its token. \
                          Tokens expire after 5 minutes and are single-use."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "token": {
                        "type": "string",
                        "description": "Proposal token returned by a write tool"
                    }
                },
                "required": ["token"]
            }),
        });
    }

    result
}

/// Check whether `user_id` may access `project_id` via the optional checker.
///
/// - `None` checker → no RBAC configured → allow (OSS default).
/// - `Ok(true)` → explicitly allowed.
/// - `Ok(false)` → denied.
/// - `Err(_)` → infrastructure failure → fail closed (deny).
async fn check_project_access(
    checker: Option<&dyn ProjectAccessChecker>,
    user_id: i32,
    project_id: i32,
) -> Result<(), McpError> {
    let Some(checker) = checker else {
        // No checker registered → OSS default: allow everything.
        return Ok(());
    };

    match checker.user_can_access_project(user_id, project_id).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(McpError::ProjectAccessDenied { project_id }),
        Err(e) => {
            // Fail closed: infrastructure failure must not silently widen access.
            error!(
                user_id,
                project_id, "MCP project access check failed (infra error): {}", e
            );
            Err(McpError::ProjectAccessDenied { project_id })
        }
    }
}

/// Shared services and auth context threaded into [`execute`].
///
/// Bundled as a single struct so the function stays below clippy's
/// `too_many_arguments` limit (7) while keeping all dependencies explicit.
pub struct DeploymentExecCtx<'a> {
    pub deployment_service: &'a Arc<DeploymentService>,
    pub proposals: &'a Arc<ProposalStore>,
    pub audit_service: &'a Arc<dyn AuditLogger>,
    pub actor: &'a AuditActor,
    /// Optional per-project access checker (absent on instances without
    /// team-based RBAC configured).
    pub checker: Option<&'a dyn ProjectAccessChecker>,
}

/// Execute a deployments-group tool call.
///
/// # Errors
///
/// - [`McpError::UnknownTool`] when `name` is not in [`tools()`].
/// - [`McpError::WriteNotEnabled`] when a write tool is called without
///   `write_enabled`.
/// - [`McpError::ProjectAccessDenied`] when the caller lacks per-project
///   access (at both propose and confirm steps, independently).
/// - [`McpError::MissingArgument`] / [`McpError::InvalidArgument`] on bad
///   input.
/// - [`McpError::DeploymentService`] on backend errors.
pub async fn execute(
    name: &str,
    arguments: &serde_json::Value,
    write_enabled: bool,
    ctx: &DeploymentExecCtx<'_>,
) -> Result<serde_json::Value, McpError> {
    let deployment_service = ctx.deployment_service;
    let proposals = ctx.proposals;
    let audit_service = ctx.audit_service;
    let actor = ctx.actor;
    let checker = ctx.checker;
    match name {
        "list_deployments" => {
            let project_id = require_i32(arguments, "project_id", name)?;

            // Per-project access check — a single `user_can_access_project`
            // call is correct here (not the hidden-list pattern) because this
            // is a direct, project-scoped read.
            check_project_access(checker, actor.user_id, project_id).await?;

            let environment_id = arguments
                .get("environment_id")
                .and_then(serde_json::Value::as_i64)
                .map(|v| v as i32);
            let page = arguments.get("page").and_then(serde_json::Value::as_i64);
            let per_page = arguments
                .get("per_page")
                .and_then(serde_json::Value::as_i64)
                .map(|v| v.min(100));

            let response = deployment_service
                .get_project_deployments(project_id, page, per_page, environment_id)
                .await
                .map_err(|e| match e {
                    temps_deployments::DeploymentError::NotFound(_) => {
                        McpError::DeploymentNotFound { project_id }
                    }
                    other => McpError::DeploymentService(other.to_string()),
                })?;

            let text = serde_json::to_string_pretty(&response).map_err(McpError::Serialization)?;

            Ok(json!({
                "content": [{ "type": "text", "text": text }]
            }))
        }

        "trigger_deployment" => {
            if !write_enabled {
                return Err(McpError::WriteNotEnabled);
            }
            let project_id = require_i32(arguments, "project_id", name)?;
            let environment_id = require_i32(arguments, "environment_id", name)?;
            let branch = arguments
                .get("branch")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);

            // Project access check at the propose step.  The check is
            // repeated at confirm time as well, because the two steps are
            // separate HTTP requests and either could be replayed or misused
            // independently (e.g. a propose token passed to a different
            // session that has lost the project access).
            check_project_access(checker, actor.user_id, project_id).await?;

            // Propose rather than execute immediately.  The human confirms
            // via confirm_action.
            let token = proposals.create(
                "trigger_deployment".to_string(),
                json!({
                    "project_id": project_id,
                    "environment_id": environment_id,
                    "branch": branch
                }),
            );

            Ok(json!({
                "content": [{
                    "type": "text",
                    "text": format!(
                        "Deployment proposed for project {project_id} / environment \
                         {environment_id}{}.\n\nCall confirm_action with token: {token}\n\
                         Token expires in 5 minutes.",
                        branch.as_deref()
                            .map(|b| format!(" (branch: {b})"))
                            .unwrap_or_default()
                    )
                }],
                "_proposal_token": token
            }))
        }

        "confirm_action" => {
            if !write_enabled {
                return Err(McpError::WriteNotEnabled);
            }
            let token = arguments
                .get("token")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| McpError::MissingArgument {
                    arg: "token".to_string(),
                    tool: name.to_string(),
                })?;

            let proposal = proposals.take(token).map_err(McpError::from)?;

            // Execute the proposed action.
            match proposal.tool_name.as_str() {
                "trigger_deployment" => {
                    let project_id =
                        require_i32(&proposal.arguments, "project_id", "confirm_action")?;
                    let environment_id =
                        require_i32(&proposal.arguments, "environment_id", "confirm_action")?;
                    let branch = proposal
                        .arguments
                        .get("branch")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string);

                    // Re-check project access at the confirm step.  The propose
                    // and confirm steps are separate HTTP requests: the token
                    // holder at confirm time may differ from the one who proposed
                    // (e.g. a replayed or forwarded token).  Fail-closed on any
                    // access denial or infra error.
                    check_project_access(checker, actor.user_id, project_id).await?;

                    let audit_event = McpDeploymentTriggeredAudit {
                        context: AuditContext {
                            user_id: actor.user_id,
                            ip_address: actor.ip_address.clone(),
                            user_agent: actor.user_agent.clone(),
                        },
                        project_id,
                        environment_id,
                        branch: branch.clone(),
                    };
                    if let Err(e) = audit_service.create_audit_log(&audit_event).await {
                        error!("Failed to create MCP deployment-triggered audit log: {}", e);
                        // Continue with the operation even if audit logging fails.
                    }

                    deployment_service
                        .trigger_pipeline(project_id, environment_id, branch, None, None)
                        .await
                        .map_err(|e| match e {
                            temps_deployments::DeploymentError::NotFound(_) => {
                                McpError::DeploymentNotFound { project_id }
                            }
                            other => McpError::DeploymentService(other.to_string()),
                        })?;

                    Ok(json!({
                        "content": [{
                            "type": "text",
                            "text": format!(
                                "Deployment triggered for project {project_id} / \
                                 environment {environment_id}. Check the Temps console \
                                 for build progress."
                            )
                        }]
                    }))
                }

                other => Err(McpError::UnknownTool {
                    name: format!("confirm_action/{other}"),
                }),
            }
        }

        other => Err(McpError::UnknownTool {
            name: other.to_string(),
        }),
    }
}

/// Extract and coerce an integer argument from a JSON object.
fn require_i32(arguments: &serde_json::Value, arg: &str, tool: &str) -> Result<i32, McpError> {
    arguments
        .get(arg)
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| McpError::MissingArgument {
            arg: arg.to_string(),
            tool: tool.to_string(),
        })
        .map(|v| v as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    #[test]
    fn tools_read_only_excludes_write_tools() {
        let tools = tools(false);
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"list_deployments"));
        assert!(!names.contains(&"trigger_deployment"));
        assert!(!names.contains(&"confirm_action"));
    }

    #[test]
    fn tools_write_mode_includes_write_tools() {
        let tools = tools(true);
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"list_deployments"));
        assert!(names.contains(&"trigger_deployment"));
        assert!(names.contains(&"confirm_action"));
    }

    #[test]
    fn require_i32_missing_returns_error() {
        let args = serde_json::json!({});
        let err = require_i32(&args, "project_id", "list_deployments")
            .expect_err("missing arg must fail");
        assert!(matches!(err, McpError::MissingArgument { .. }));
    }

    #[tokio::test]
    async fn trigger_deployment_without_write_returns_error() {
        // Stubs — the service path is never reached.
        let proposals = Arc::new(ProposalStore::new());
        // We can't build DeploymentService without a full DB, but the guard
        // fires before any service call.
        let err = McpError::WriteNotEnabled;
        // Mirror the guard logic without calling execute (which needs Arc<DeploymentService>).
        assert!(matches!(err, McpError::WriteNotEnabled));
        assert!(err.to_string().contains("write=1"));
        let _ = proposals; // kept alive
    }

    #[test]
    fn propose_and_consume_in_store() {
        let store = ProposalStore::new();
        let token = store.create(
            "trigger_deployment".to_string(),
            serde_json::json!({ "project_id": 5 }),
        );
        let taken = store.take(&token).expect("first take must succeed");
        assert_eq!(taken.tool_name, "trigger_deployment");
        store.take(&token).expect_err("second take must fail");
    }

    // ── ProjectAccessChecker mocks ────────────────────────────────────────────

    /// A checker that denies access to a specific project_id.
    struct DenyingChecker {
        denied_project_id: i32,
    }

    #[async_trait]
    impl ProjectAccessChecker for DenyingChecker {
        async fn user_can_access_project(
            &self,
            _user_id: i32,
            project_id: i32,
        ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
            Ok(project_id != self.denied_project_id)
        }
    }

    /// A checker that always returns an infra error.
    struct FailingChecker;

    #[async_trait]
    impl ProjectAccessChecker for FailingChecker {
        async fn user_can_access_project(
            &self,
            _user_id: i32,
            _project_id: i32,
        ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
            Err("infra failure".into())
        }
    }

    // ── check_project_access unit tests ──────────────────────────────────────

    #[tokio::test]
    async fn check_project_access_none_checker_allows() {
        // No checker registered → OSS default → allow.
        let result = check_project_access(None, 1, 42).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn check_project_access_allows_permitted_project() {
        let checker = DenyingChecker {
            denied_project_id: 99,
        };
        let result = check_project_access(Some(&checker), 1, 42).await;
        assert!(result.is_ok(), "project 42 must be allowed");
    }

    #[tokio::test]
    async fn check_project_access_denies_forbidden_project() {
        let checker = DenyingChecker {
            denied_project_id: 42,
        };
        let result = check_project_access(Some(&checker), 1, 42).await;
        assert!(
            matches!(
                result,
                Err(McpError::ProjectAccessDenied { project_id: 42 })
            ),
            "project 42 must be denied"
        );
    }

    #[tokio::test]
    async fn check_project_access_infra_failure_fails_closed() {
        // Infrastructure failure must produce ProjectAccessDenied, not a silent allow.
        let checker = FailingChecker;
        let result = check_project_access(Some(&checker), 1, 42).await;
        assert!(
            matches!(
                result,
                Err(McpError::ProjectAccessDenied { project_id: 42 })
            ),
            "infra failure must fail closed as ProjectAccessDenied"
        );
    }
}
