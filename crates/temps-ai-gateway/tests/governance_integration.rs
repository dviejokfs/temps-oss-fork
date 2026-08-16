//! Integration tests for the AI gateway governance service.
//!
//! These tests require a running PostgreSQL instance.  They use
//! `temps_database::test_utils::TestDatabase` which either spins up a
//! testcontainers-based TimescaleDB or connects to an existing database via
//! `TEMPS_TEST_DATABASE_URL`.  If Docker is unavailable the tests skip
//! gracefully — they never use `#[ignore]`.

use std::sync::Arc;
use temps_ai_gateway::services::{AiUsageAttribution, GovernanceService};
use temps_database::test_utils::{is_container_runtime_unavailable, TestDatabase};

/// Sets up a TestDatabase with all migrations applied and returns a
/// GovernanceService backed by the isolated test schema.
async fn setup() -> anyhow::Result<Option<(TestDatabase, Arc<GovernanceService>)>> {
    let test_db = match TestDatabase::with_migrations().await {
        Ok(db) => db,
        Err(e) => {
            let msg = e.to_string();
            if is_container_runtime_unavailable(&msg) {
                eprintln!("Docker not available, skipping governance integration test: {msg}");
                return Ok(None);
            }
            return Err(e);
        }
    };

    let db_arc = test_db.connection_arc();
    let svc = Arc::new(GovernanceService::new(db_arc));
    Ok(Some((test_db, svc)))
}

fn attribution() -> AiUsageAttribution {
    AiUsageAttribution {
        user_id: Some(1),
        project_id: None,
        environment_id: None,
        deployment_id: None,
        deployment_token_id: None,
    }
}

// ============================================================================
// Config CRUD
// ============================================================================

#[tokio::test]
async fn config_upsert_list_delete_roundtrip() -> anyhow::Result<()> {
    let Some((test_db, svc)) = setup().await? else {
        return Ok(());
    };

    // Start with empty list
    let configs = svc.list_configs().await?;
    assert!(
        configs.is_empty(),
        "fresh schema should have no governance configs"
    );

    // Upsert an instance config
    let created = svc
        .upsert_config(
            "instance",
            Some(serde_json::json!(["gpt-4o", "gpt-4o-mini"])),
            Some(60),
            Some(10_000_000),
        )
        .await?;
    assert_eq!(created.scope, "instance");
    assert_eq!(created.max_requests_per_minute, Some(60));

    // List should now have one
    let configs = svc.list_configs().await?;
    assert_eq!(configs.len(), 1);

    // Upsert again (update)
    let updated = svc.upsert_config("instance", None, Some(120), None).await?;
    assert_eq!(updated.scope, "instance");
    assert_eq!(updated.max_requests_per_minute, Some(120));
    assert!(updated.max_cost_per_month_microcents.is_none());

    // Delete
    svc.delete_config("instance").await?;
    let configs = svc.list_configs().await?;
    assert!(configs.is_empty());

    drop(test_db);
    Ok(())
}

#[tokio::test]
async fn delete_nonexistent_scope_returns_not_found() -> anyhow::Result<()> {
    let Some((test_db, svc)) = setup().await? else {
        return Ok(());
    };

    let result = svc.delete_config("project:9999").await;
    assert!(
        result.is_err(),
        "deleting a nonexistent scope should return an error"
    );

    drop(test_db);
    Ok(())
}

// ============================================================================
// Allowlist enforcement
// ============================================================================

#[tokio::test]
async fn allowlist_blocks_unlisted_model() -> anyhow::Result<()> {
    let Some((test_db, svc)) = setup().await? else {
        return Ok(());
    };

    // Allow only gpt-4o-mini at the instance level
    svc.upsert_config(
        "instance",
        Some(serde_json::json!(["gpt-4o-mini"])),
        None,
        None,
    )
    .await?;

    let attr = attribution();
    // gpt-4o should be rejected
    let result = svc.check_request(&attr, "gpt-4o", false, None, None).await;
    assert!(result.is_err(), "gpt-4o should be blocked by the allowlist");

    // gpt-4o-mini should be allowed
    let reservation = svc
        .check_request(&attr, "gpt-4o-mini", false, None, None)
        .await?;
    // Release the reservation (no actual usage)
    svc.release_cost_reservation(&reservation).await?;

    drop(test_db);
    Ok(())
}

#[tokio::test]
async fn empty_allowlist_blocks_all_models() -> anyhow::Result<()> {
    let Some((test_db, svc)) = setup().await? else {
        return Ok(());
    };

    // Empty array = deny everything
    svc.upsert_config("instance", Some(serde_json::json!([])), None, None)
        .await?;

    let attr = attribution();
    let result = svc
        .check_request(&attr, "gpt-4o-mini", false, None, None)
        .await;
    assert!(
        result.is_err(),
        "an empty allowlist should block all models"
    );

    drop(test_db);
    Ok(())
}

// ============================================================================
// Budget reservation lifecycle
// ============================================================================

#[tokio::test]
async fn reservation_is_created_and_released() -> anyhow::Result<()> {
    let Some((test_db, svc)) = setup().await? else {
        return Ok(());
    };

    // Set a generous budget so the reservation succeeds
    svc.upsert_config("instance", None, None, Some(1_000_000_000))
        .await?;

    let attr = attribution();
    // A request with max_tokens produces a reservation
    let reservation = svc
        .check_request(&attr, "gpt-4o", false, Some(100), Some(1000))
        .await?;

    // Release it (simulating upstream failure)
    svc.release_cost_reservation(&reservation).await?;

    // After release the reservation row is gone — a second release should no-op
    // gracefully rather than panic (it may return Err, that's fine).
    let _ = svc.release_cost_reservation(&reservation).await;

    drop(test_db);
    Ok(())
}

#[tokio::test]
async fn byok_requests_skip_budget_check() -> anyhow::Result<()> {
    let Some((test_db, svc)) = setup().await? else {
        return Ok(());
    };

    // Set an exhausted budget (1 microcent limit)
    svc.upsert_config("instance", None, None, Some(1)).await?;

    let attr = attribution();
    // BYOK requests (is_byok=true) should bypass the budget and succeed
    let reservation = svc
        .check_request(
            &attr,
            "gpt-4o",
            true, /* is_byok */
            Some(100),
            Some(1000),
        )
        .await?;
    svc.release_cost_reservation(&reservation).await?;

    drop(test_db);
    Ok(())
}

// ============================================================================
// Cleanup of expired reservations
// ============================================================================

#[tokio::test]
async fn cleanup_expired_state_runs_without_error() -> anyhow::Result<()> {
    let Some((test_db, svc)) = setup().await? else {
        return Ok(());
    };

    // Running cleanup on an empty schema should succeed without errors.
    svc.run_cleanup().await?;

    drop(test_db);
    Ok(())
}

// ============================================================================
// Validation
// ============================================================================

#[tokio::test]
async fn invalid_scope_is_rejected() -> anyhow::Result<()> {
    let Some((test_db, svc)) = setup().await? else {
        return Ok(());
    };

    let result = svc
        .upsert_config("bad:scope:format:extra", None, None, None)
        .await;
    assert!(result.is_err(), "unknown scope format should be rejected");

    drop(test_db);
    Ok(())
}

#[tokio::test]
async fn negative_rpm_limit_is_rejected() -> anyhow::Result<()> {
    let Some((test_db, svc)) = setup().await? else {
        return Ok(());
    };

    let result = svc.upsert_config("instance", None, Some(-1), None).await;
    assert!(result.is_err(), "negative RPM limit should be rejected");

    drop(test_db);
    Ok(())
}
