//! Adds the partial index used by the bounded permission-denial retention
//! worker. Without it, each small delete batch could scan the entire audit
//! history as the table grows, defeating the bounded-maintenance contract.

use sea_orm_migration::prelude::*;

const INDEX_NAME: &str = "idx_audit_logs_permission_denied_retention";

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Migrations run transactionally, so PostgreSQL does not permit
        // CONCURRENTLY. Bound both lock acquisition and build time rather than
        // allowing startup to hang indefinitely on a busy audit table.
        manager
            .get_connection()
            .execute_unprepared("SET LOCAL lock_timeout = '5s'")
            .await?;
        manager
            .get_connection()
            .execute_unprepared("SET LOCAL statement_timeout = '30s'")
            .await?;
        manager
            .get_connection()
            .execute_unprepared(&format!(
                "CREATE INDEX IF NOT EXISTS {INDEX_NAME} \
                 ON audit_logs (audit_date ASC, id ASC) \
                 WHERE operation_type = 'PERMISSION_DENIED'"
            ))
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared("SET LOCAL lock_timeout = '5s'")
            .await?;
        manager
            .get_connection()
            .execute_unprepared("SET LOCAL statement_timeout = '30s'")
            .await?;
        manager
            .get_connection()
            .execute_unprepared(&format!("DROP INDEX IF EXISTS {INDEX_NAME}"))
            .await?;
        Ok(())
    }
}
