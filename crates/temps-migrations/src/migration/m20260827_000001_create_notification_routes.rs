//! Create severity-based notification routes.
//!
//! Routes deliberately sit between notifications and providers: providers
//! describe destinations, while routes decide which destinations receive an
//! event. Existing installations get one permissive default route so the
//! upgrade preserves their previous fan-out behavior.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                r#"
CREATE TABLE notification_routes (
    id SERIAL PRIMARY KEY,
    name VARCHAR NOT NULL,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    min_severity VARCHAR NOT NULL DEFAULT 'debug',
    max_severity VARCHAR NOT NULL DEFAULT 'emergency',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT notification_routes_name_unique UNIQUE (name),
    CONSTRAINT notification_routes_min_severity_valid
        CHECK (min_severity IN ('debug', 'info', 'warning', 'error', 'critical', 'emergency')),
    CONSTRAINT notification_routes_max_severity_valid
        CHECK (max_severity IN ('debug', 'info', 'warning', 'error', 'critical', 'emergency')),
    CONSTRAINT notification_routes_severity_range_valid
        CHECK (
            array_position(ARRAY['debug', 'info', 'warning', 'error', 'critical', 'emergency'], min_severity)
            <= array_position(ARRAY['debug', 'info', 'warning', 'error', 'critical', 'emergency'], max_severity)
        )
);

CREATE INDEX idx_notification_routes_enabled
    ON notification_routes (enabled);

CREATE TABLE notification_route_providers (
    route_id INTEGER NOT NULL REFERENCES notification_routes(id) ON DELETE CASCADE,
    provider_id INTEGER NOT NULL REFERENCES notification_providers(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (route_id, provider_id)
);

CREATE INDEX idx_notification_route_providers_provider_id
    ON notification_route_providers (provider_id);

DO $$
DECLARE
    default_route_id INTEGER;
BEGIN
    IF EXISTS (SELECT 1 FROM notification_providers) THEN
        INSERT INTO notification_routes (name, enabled, min_severity, max_severity)
        VALUES ('Default route', TRUE, 'debug', 'emergency')
        RETURNING id INTO default_route_id;

        INSERT INTO notification_route_providers (route_id, provider_id)
        SELECT default_route_id, id
        FROM notification_providers;
    END IF;
END $$;
"#,
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                r#"
DROP TABLE IF EXISTS notification_route_providers;
DROP TABLE IF EXISTS notification_routes;
"#,
            )
            .await?;

        Ok(())
    }
}
