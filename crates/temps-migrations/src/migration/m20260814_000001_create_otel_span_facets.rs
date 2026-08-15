use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// Create the `otel_span_facets` table.
///
/// Each row maps an OTel attribute key (arbitrary string) to a pre-allocated
/// generic slot column in the ClickHouse `spans` table (`facet_attr_1` through
/// `facet_attr_20`). The slot value is 1-indexed and bounded by a CHECK
/// constraint.
///
/// The `attribute_key` and `slot` columns both have UNIQUE constraints to
/// prevent two keys mapping to the same slot and to prevent the same key being
/// registered twice.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();

        db.execute_unprepared(
            r#"CREATE TABLE IF NOT EXISTS otel_span_facets (
                id           SERIAL PRIMARY KEY,
                attribute_key VARCHAR(200) NOT NULL,
                slot         SMALLINT NOT NULL,
                created_by   INTEGER NULL,
                created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                CONSTRAINT uq_otel_span_facets_key  UNIQUE (attribute_key),
                CONSTRAINT uq_otel_span_facets_slot UNIQUE (slot),
                CONSTRAINT ck_otel_span_facets_slot CHECK (slot BETWEEN 1 AND 20)
            )"#,
        )
        .await?;

        db.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_otel_span_facets_created_at \
             ON otel_span_facets (created_at DESC)",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        db.execute_unprepared("DROP TABLE IF EXISTS otel_span_facets")
            .await?;
        Ok(())
    }
}
