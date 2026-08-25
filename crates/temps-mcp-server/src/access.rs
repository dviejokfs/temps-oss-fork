use temps_auth::context::AuthContext;
use temps_auth::permissions::Role;
use temps_core::project_access::ProjectAccessChecker;
use tracing::error;

use crate::error::McpError;

/// Check whether `auth` may access `project_id` via the optional checker.
///
/// Mirrors the `resolve_hidden_projects` REST handler's admin / deployment-token
/// exemption: platform admins, instance admins, and deployment tokens are
/// allowed unconditionally without ever consulting the `ProjectAccessChecker`.
///
/// - Admin bypass (`is_deployment_token`, `is_admin`, or `PlatformAdmin` role)
///   → return `Ok(())` immediately, checker is not called.
/// - `None` checker → no RBAC configured → allow (OSS default).
/// - `Ok(true)` from checker → explicitly allowed.
/// - `Ok(false)` from checker → denied.
/// - `Err(_)` from checker → infrastructure failure → fail closed (deny).
pub(crate) async fn check_project_access(
    checker: Option<&dyn ProjectAccessChecker>,
    auth: &AuthContext,
    project_id: i32,
) -> Result<(), McpError> {
    // Admin / deployment-token bypass: matches resolve_hidden_projects semantics.
    // Neither the checker nor the OSS-default path is consulted for these
    // principals — they see everything, unconditionally.
    if auth.is_deployment_token() || auth.is_admin() || auth.has_role(&Role::PlatformAdmin) {
        return Ok(());
    }

    let Some(checker) = checker else {
        // No checker registered → OSS default: allow everything.
        return Ok(());
    };

    let user_id = auth.user_id();
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

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::Utc;
    use temps_entities::users;

    // ── AuthContext helpers ───────────────────────────────────────────────────

    fn make_user(id: i32) -> users::Model {
        let now = Utc::now();
        users::Model {
            id,
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

    fn non_admin_auth() -> AuthContext {
        AuthContext::new_session(make_user(1), Role::User)
    }

    fn admin_auth() -> AuthContext {
        AuthContext::new_session(make_user(1), Role::Admin)
    }

    fn platform_admin_auth() -> AuthContext {
        AuthContext::new_session(make_user(1), Role::PlatformAdmin)
    }

    fn deployment_token_auth() -> AuthContext {
        AuthContext::new_deployment_token(1, None, None, 1, "test-token".to_string(), vec![])
    }

    // ── Checker mocks ─────────────────────────────────────────────────────────

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

    // ── Existing behaviour (non-admin principals) ─────────────────────────────

    #[tokio::test]
    async fn check_project_access_none_checker_allows() {
        // No checker registered → OSS default → allow.
        let result = check_project_access(None, &non_admin_auth(), 42).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn check_project_access_allows_permitted_project() {
        let checker = DenyingChecker {
            denied_project_id: 99,
        };
        let result = check_project_access(Some(&checker), &non_admin_auth(), 42).await;
        assert!(result.is_ok(), "project 42 must be allowed");
    }

    #[tokio::test]
    async fn check_project_access_denies_forbidden_project() {
        let checker = DenyingChecker {
            denied_project_id: 42,
        };
        let result = check_project_access(Some(&checker), &non_admin_auth(), 42).await;
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
        let result = check_project_access(Some(&checker), &non_admin_auth(), 42).await;
        assert!(
            matches!(
                result,
                Err(McpError::ProjectAccessDenied { project_id: 42 })
            ),
            "infra failure must fail closed as ProjectAccessDenied"
        );
    }

    // ── Admin bypass (parity with resolve_hidden_projects REST handler) ────────

    /// A PlatformAdmin must bypass the `ProjectAccessChecker` entirely and be
    /// allowed unconditionally — even if the checker would have denied them.
    #[tokio::test]
    async fn check_project_access_platform_admin_bypasses_checker() {
        let checker = DenyingChecker {
            denied_project_id: 42,
        };
        let result = check_project_access(Some(&checker), &platform_admin_auth(), 42).await;
        assert!(
            result.is_ok(),
            "PlatformAdmin must bypass the checker and be allowed for any project"
        );
    }

    /// Instance admins (Role::Admin) must also bypass the checker.
    #[tokio::test]
    async fn check_project_access_admin_bypasses_checker() {
        let checker = DenyingChecker {
            denied_project_id: 42,
        };
        let result = check_project_access(Some(&checker), &admin_auth(), 42).await;
        assert!(
            result.is_ok(),
            "Role::Admin must bypass the checker and be allowed for any project"
        );
    }

    /// Deployment tokens must also bypass the `ProjectAccessChecker` — the
    /// token is already project-scoped by its own tenant-boundary mechanism
    /// (`is_scoped_to_project`), which the handler layer enforces separately.
    #[tokio::test]
    async fn check_project_access_deployment_token_bypasses_checker() {
        let checker = DenyingChecker {
            denied_project_id: 42,
        };
        let result = check_project_access(Some(&checker), &deployment_token_auth(), 42).await;
        assert!(
            result.is_ok(),
            "deployment token must bypass the checker and be allowed"
        );
    }
}
