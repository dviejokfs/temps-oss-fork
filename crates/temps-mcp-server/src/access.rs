use temps_core::project_access::ProjectAccessChecker;
use tracing::error;

use crate::error::McpError;

/// Check whether `user_id` may access `project_id` via the optional checker.
///
/// - `None` checker → no RBAC configured → allow (OSS default).
/// - `Ok(true)` → explicitly allowed.
/// - `Ok(false)` → denied.
/// - `Err(_)` → infrastructure failure → fail closed (deny).
pub(crate) async fn check_project_access(
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

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

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
