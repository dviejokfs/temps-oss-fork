use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// Adds governance infrastructure:
///
/// 1. `ai_gateway_rate_events` — a sliding-window log used by the advisory-lock
///    serialised RPM check.  Rows older than 60 s are not read and can be
///    vacuumed; the index on `scope, occurred_at` supports the window query.
///
/// 2. `ai_gateway_cost_reservations` — conservative cost holds created before
///    each request so budget checks remain coherent under concurrency.
///    Rows are atomically replaced by an `ai_usage_logs` insert on success, or
///    deleted explicitly on upstream failure.  A background worker converts
///    timed-out rows (requests that died without updating) into durable debits.
///
/// 3. New columns on `ai_usage_logs`:
///    - `project_id`, `environment_id`, `deployment_id`, `deployment_token_id`
///      for deployment-token attribution (NULL for interactive user sessions).
///    - `billing_period` (DATE) — the first day of the UTC month in which the
///      request completed.  Used for monthly budget roll-up queries.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // ── 1. ai_gateway_rate_events ────────────────────────────────────────
        manager
            .create_table(
                Table::create()
                    .table(AiGatewayRateEvents::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(AiGatewayRateEvents::Id)
                            .big_integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(AiGatewayRateEvents::Scope)
                            .string_len(200)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(AiGatewayRateEvents::OccurredAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .table(AiGatewayRateEvents::Table)
                    .name("idx_ai_gateway_rate_events_scope_occurred_at")
                    .col(AiGatewayRateEvents::Scope)
                    .col(AiGatewayRateEvents::OccurredAt)
                    .to_owned(),
            )
            .await?;

        // ── 2. ai_gateway_cost_reservations ─────────────────────────────────
        manager
            .create_table(
                Table::create()
                    .table(AiGatewayCostReservations::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(AiGatewayCostReservations::RequestId)
                            .string_len(64)
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(AiGatewayCostReservations::Scope)
                            .string_len(200)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(AiGatewayCostReservations::ReservedMicrocents)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(AiGatewayCostReservations::BillingPeriod)
                            .date()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(AiGatewayCostReservations::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .col(
                        ColumnDef::new(AiGatewayCostReservations::ExpiresAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(AiGatewayCostReservations::IsConservativeDebit)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .table(AiGatewayCostReservations::Table)
                    .name("idx_ai_gateway_cost_reservations_scope_billing_period")
                    .col(AiGatewayCostReservations::Scope)
                    .col(AiGatewayCostReservations::BillingPeriod)
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .table(AiGatewayCostReservations::Table)
                    .name("idx_ai_gateway_cost_reservations_expires_at")
                    .col(AiGatewayCostReservations::ExpiresAt)
                    .to_owned(),
            )
            .await?;

        // ── 3. New columns on ai_usage_logs ──────────────────────────────────
        manager
            .alter_table(
                Table::alter()
                    .table(AiUsageLogs::Table)
                    .add_column(ColumnDef::new(AiUsageLogs::ProjectId).integer().null())
                    .to_owned(),
            )
            .await?;

        manager
            .alter_table(
                Table::alter()
                    .table(AiUsageLogs::Table)
                    .add_column(ColumnDef::new(AiUsageLogs::EnvironmentId).integer().null())
                    .to_owned(),
            )
            .await?;

        manager
            .alter_table(
                Table::alter()
                    .table(AiUsageLogs::Table)
                    .add_column(ColumnDef::new(AiUsageLogs::DeploymentId).integer().null())
                    .to_owned(),
            )
            .await?;

        manager
            .alter_table(
                Table::alter()
                    .table(AiUsageLogs::Table)
                    .add_column(
                        ColumnDef::new(AiUsageLogs::DeploymentTokenId)
                            .integer()
                            .null(),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .alter_table(
                Table::alter()
                    .table(AiUsageLogs::Table)
                    .add_column(ColumnDef::new(AiUsageLogs::BillingPeriod).date().null())
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Remove new ai_usage_logs columns
        manager
            .alter_table(
                Table::alter()
                    .table(AiUsageLogs::Table)
                    .drop_column(AiUsageLogs::BillingPeriod)
                    .to_owned(),
            )
            .await?;

        manager
            .alter_table(
                Table::alter()
                    .table(AiUsageLogs::Table)
                    .drop_column(AiUsageLogs::DeploymentTokenId)
                    .to_owned(),
            )
            .await?;

        manager
            .alter_table(
                Table::alter()
                    .table(AiUsageLogs::Table)
                    .drop_column(AiUsageLogs::DeploymentId)
                    .to_owned(),
            )
            .await?;

        manager
            .alter_table(
                Table::alter()
                    .table(AiUsageLogs::Table)
                    .drop_column(AiUsageLogs::EnvironmentId)
                    .to_owned(),
            )
            .await?;

        manager
            .alter_table(
                Table::alter()
                    .table(AiUsageLogs::Table)
                    .drop_column(AiUsageLogs::ProjectId)
                    .to_owned(),
            )
            .await?;

        // Drop governance tables
        manager
            .drop_table(
                Table::drop()
                    .table(AiGatewayCostReservations::Table)
                    .to_owned(),
            )
            .await?;

        manager
            .drop_table(Table::drop().table(AiGatewayRateEvents::Table).to_owned())
            .await?;

        Ok(())
    }
}

#[derive(DeriveIden)]
enum AiGatewayRateEvents {
    Table,
    Id,
    Scope,
    OccurredAt,
}

#[derive(DeriveIden)]
enum AiGatewayCostReservations {
    Table,
    RequestId,
    Scope,
    ReservedMicrocents,
    BillingPeriod,
    CreatedAt,
    ExpiresAt,
    IsConservativeDebit,
}

#[derive(DeriveIden)]
enum AiUsageLogs {
    Table,
    ProjectId,
    EnvironmentId,
    DeploymentId,
    DeploymentTokenId,
    BillingPeriod,
}
