//! Shared service-level authorization for the backup/restore/upgrade handlers.
//!
//! `RequireAuth` proves a caller is authenticated and `permission_guard!`
//! proves their role holds a permission, but plain `Role::User` holds most
//! backup/restore/upgrade permissions **instance-wide** — there is no project
//! qualifier on the role itself. The thing that actually confines a caller to
//! only the projects they belong to is [`temps_core::ProjectAccessChecker`],
//! an optional extension point that is `None` in plain OSS (a no-op there,
//! since there is no team boundary yet) and gets registered by the Teams
//! plugin in EE. Every handler here that is keyed by an `external_services`
//! id must additionally call [`require_service_access`] before touching the
//! target resource, or it is a cross-tenant IDOR the moment Teams is
//! installed.
//!
//! Originally written for the restore endpoints (a restore reads one
//! service's data and writes it into another — both halves are privileged),
//! but the same gap exists anywhere a handler is keyed by a bare
//! `service_id`/`schedule_id` path or body parameter, so this module is
//! shared by `restore_handler`, `backup_handler` and `pg_upgrade_handler`.

use axum::http::StatusCode;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use temps_core::problemdetails::{self, Problem};
use tracing::error;

use crate::handlers::types::BackupAppState;

/// Projects an external service is linked to, via the `project_services`
/// join table. An empty result means the service is linked to no project.
pub(crate) async fn linked_project_ids(
    db: &sea_orm::DatabaseConnection,
    service_id: i32,
) -> Result<Vec<i32>, Problem> {
    let links = temps_entities::project_services::Entity::find()
        .filter(temps_entities::project_services::Column::ServiceId.eq(service_id))
        .all(db)
        .await
        .map_err(|e| {
            error!(service_id, error = %e, "service authz: failed to resolve linked projects");
            problemdetails::new(StatusCode::INTERNAL_SERVER_ERROR)
                .with_title("Internal Server Error")
                .with_detail("Failed to verify service access")
        })?;
    Ok(links.into_iter().map(|link| link.project_id).collect())
}

/// A deployment token is minted for exactly one project, so it may only reach
/// services linked to that project. Pure so the rule is testable without a
/// database; a service linked to no project is reachable by no token.
pub(crate) fn deployment_token_may_access(token_project_id: i32, project_ids: &[i32]) -> bool {
    project_ids.contains(&token_project_id)
}

fn access_denied(what: &str, id: i32, operation: &str) -> Problem {
    problemdetails::new(StatusCode::FORBIDDEN)
        .with_title("Insufficient Permissions")
        .with_detail(format!(
            "You do not have access to the {} ({}) involved in this {}",
            what, id, operation
        ))
}

/// Deny unless the caller may act on `service_id`.
///
/// * Instance-wide Admin/PlatformAdmin bypass, matching the documented
///   contract of [`temps_core::ProjectAccessChecker`].
/// * A deployment token is confined to its own project — enforced here even
///   when no checker is registered, since it needs no external policy.
/// * Otherwise, if a checker is registered, the caller must be able to reach
///   at least one project the service is linked to. With no checker (plain
///   OSS) this is a no-op, which is the documented fail-open-when-unconfigured
///   behaviour of the extension point.
///
/// `what` names the resource in the denial message (e.g. `"target service"`,
/// `"external service"`); `operation` names what the caller was attempting
/// (e.g. `"restore"`, `"backup"`, `"PostgreSQL upgrade"`), so the same
/// message shape reads naturally from every call site.
pub(crate) async fn require_service_access(
    app_state: &BackupAppState,
    auth: &temps_auth::AuthContext,
    service_id: i32,
    what: &str,
    operation: &str,
) -> Result<(), Problem> {
    if auth.is_admin() || auth.has_role(&temps_auth::Role::PlatformAdmin) {
        return Ok(());
    }

    if let Some(token_project_id) = auth.project_id() {
        let project_ids = linked_project_ids(app_state.db.as_ref(), service_id).await?;
        if !deployment_token_may_access(token_project_id, &project_ids) {
            return Err(access_denied(what, service_id, operation));
        }
        return Ok(());
    }

    let Some(checker) = app_state.project_access_checker.as_deref() else {
        return Ok(());
    };

    let project_ids = linked_project_ids(app_state.db.as_ref(), service_id).await?;
    match checker_grants_access(checker, auth.user_id(), &project_ids).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(access_denied(what, service_id, operation)),
        Err(error) => {
            error!(service_id, error = %error, "service authz: project access check failed");
            Err(problemdetails::new(StatusCode::INTERNAL_SERVER_ERROR)
                .with_title("Project Access Check Failed")
                .with_detail("Could not verify project access; please try again"))
        }
    }
}

/// Whether `user_id` may reach at least one of `project_ids`, per the
/// registered checker. Pulled out of [`require_service_access`] so the
/// tri-state result (granted / denied / infrastructure failure) is testable
/// against a mock [`temps_core::ProjectAccessChecker`] without needing a full
/// `BackupAppState` (which pulls in Docker-backed backup/restore/upgrade
/// services that don't matter for this decision).
///
/// Fail-closed: an infrastructure error on *any* project short-circuits to
/// `Err` even if a later project would have granted access, and an empty
/// `project_ids` (service linked to no project) returns `Ok(false)` — never
/// reads as "unrestricted".
async fn checker_grants_access(
    checker: &dyn temps_core::ProjectAccessChecker,
    user_id: i32,
    project_ids: &[i32],
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let mut infrastructure_error = None;
    for project_id in project_ids {
        match checker.user_can_access_project(user_id, *project_id).await {
            Ok(true) => return Ok(true),
            Ok(false) => {}
            Err(error) => infrastructure_error = Some(error),
        }
    }

    match infrastructure_error {
        Some(error) => Err(error),
        None => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deployment token is minted for one project; access to a service
    /// outside it must be refused even before any plugin-provided
    /// project-access checker is consulted.
    #[test]
    fn deployment_token_confined_to_its_own_project() {
        assert!(deployment_token_may_access(7, &[3, 7]));
        assert!(!deployment_token_may_access(7, &[3, 9]));
    }

    /// A service linked to no project is reachable by no deployment token —
    /// an empty link set must never read as "unrestricted".
    #[test]
    fn deployment_token_denied_for_unlinked_service() {
        assert!(!deployment_token_may_access(7, &[]));
    }

    #[test]
    fn access_denied_names_resource_and_operation() {
        let problem = access_denied("external service", 42, "backup");
        assert_eq!(problem.status_code, StatusCode::FORBIDDEN);
        let body = serde_json::to_value(&problem.body).unwrap();
        let detail = body["detail"].as_str().unwrap_or("");
        assert!(detail.contains("external service"));
        assert!(detail.contains("42"));
        assert!(detail.contains("backup"));
    }

    // -----------------------------------------------------------------
    // checker_grants_access — the EE-style Teams checker path
    // -----------------------------------------------------------------
    //
    // This is the regression coverage for the actual vulnerability: with
    // `TeamProjectAccessChecker` registered (EE Teams installed), a user who
    // is not a member of the project a service belongs to must be denied,
    // even though their `Role::User` holds the instance-wide
    // Backups*/ExternalServices* permission the handler's `permission_guard!`
    // checks.

    /// Grants access to an explicit allow-list of project ids, or always
    /// errors if `infra_failure` is set — stands in for
    /// `temps-ee-teams::TeamProjectAccessChecker` without pulling in EE.
    struct StubChecker {
        allowed_project_ids: Vec<i32>,
        infra_failure: bool,
    }

    #[async_trait::async_trait]
    impl temps_core::ProjectAccessChecker for StubChecker {
        async fn user_can_access_project(
            &self,
            _user_id: i32,
            project_id: i32,
        ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
            if self.infra_failure {
                return Err("stub checker: simulated infrastructure failure".into());
            }
            Ok(self.allowed_project_ids.contains(&project_id))
        }
    }

    /// A user who belongs to none of the projects a service is linked to —
    /// the cross-tenant IDOR this whole module exists to close — must be
    /// denied even though they hold the instance-wide permission that got
    /// them past `permission_guard!`.
    #[tokio::test]
    async fn checker_denies_user_outside_the_linked_projects() {
        let checker = StubChecker {
            allowed_project_ids: vec![3],
            infra_failure: false,
        };
        let granted = checker_grants_access(&checker, 99, &[7, 8]).await.unwrap();
        assert!(
            !granted,
            "user with no membership in projects 7 or 8 must be denied"
        );
    }

    /// A user who belongs to at least one linked project is granted, even if
    /// the service is (unusually) linked to several.
    #[tokio::test]
    async fn checker_grants_user_in_any_linked_project() {
        let checker = StubChecker {
            allowed_project_ids: vec![8],
            infra_failure: false,
        };
        let granted = checker_grants_access(&checker, 42, &[7, 8]).await.unwrap();
        assert!(granted);
    }

    /// A service linked to zero projects must never read as "unrestricted" —
    /// fail closed, matching the deployment-token rule.
    #[tokio::test]
    async fn checker_denies_service_linked_to_no_project() {
        let checker = StubChecker {
            allowed_project_ids: vec![1, 2, 3],
            infra_failure: false,
        };
        let granted = checker_grants_access(&checker, 1, &[]).await.unwrap();
        assert!(!granted);
    }

    /// An infrastructure failure while checking project access must fail
    /// closed (`Err`), never silently fall through to "allow".
    #[tokio::test]
    async fn checker_infrastructure_failure_is_not_silently_allowed() {
        let checker = StubChecker {
            allowed_project_ids: vec![7],
            infra_failure: true,
        };
        let result = checker_grants_access(&checker, 1, &[7]).await;
        assert!(result.is_err());
    }
}
