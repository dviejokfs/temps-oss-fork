//! The control-plane dump leaves high-volume tables schema-only. A kept table
//! that references an excluded one makes the dump unrestorable (its rows are
//! dumped, the parent's are not, and `ADD CONSTRAINT` fails on restore), so
//! the exclusion set must be closed over foreign keys against the real
//! schema. This runs the resolver against a freshly migrated database.
//!
//! Skips (and passes) when Docker is unavailable, like the migration tests.

use std::collections::HashSet;
use std::time::Duration;

use sea_orm::{ConnectionTrait, Database, DatabaseConnection, Statement};
use sea_orm_migration::MigratorTrait;
use temps_backup::engines::control_plane::{resolve_excluded_data, EXCLUDED_DATA_TABLES};
use temps_migrations::Migrator;
use testcontainers::{core::WaitFor, runners::AsyncRunner, GenericImage, ImageExt};

async fn connect_with_retries(url: &str) -> Result<DatabaseConnection, sea_orm::DbErr> {
    let mut last = None;
    for _ in 0..20 {
        match Database::connect(url).await {
            Ok(db) => return Ok(db),
            Err(error) => {
                last = Some(error);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
    Err(last.expect("at least one attempt"))
}

#[tokio::test]
async fn excluded_data_is_closed_over_foreign_keys_on_the_real_schema(
) -> Result<(), Box<dyn std::error::Error>> {
    let container = match GenericImage::new("timescale/timescaledb-ha", "pg18")
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_env_var("POSTGRES_DB", "postgres")
        .with_env_var("POSTGRES_USER", "postgres")
        .with_env_var("POSTGRES_PASSWORD", "postgres")
        .with_env_var("POSTGRES_HOST_AUTH_METHOD", "trust")
        .with_cmd(vec![
            "postgres",
            "-c",
            "timescaledb.max_background_workers=0",
        ])
        .with_startup_timeout(Duration::from_secs(120))
        .start()
        .await
    {
        Ok(container) => container,
        Err(error) => {
            eprintln!("Skipping control-plane exclusion test: Docker unavailable: {error}");
            return Ok(());
        }
    };
    let port = container.get_host_port_ipv4(5432).await?;
    let url = format!("postgresql://postgres:postgres@localhost:{port}/postgres");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let db = connect_with_retries(&url).await?;
    Migrator::up(&db, None).await?;

    let excluded = resolve_excluded_data(&db).await;
    let set: HashSet<&str> = excluded.tables.iter().map(String::as_str).collect();

    for seed in EXCLUDED_DATA_TABLES {
        assert!(set.contains(seed), "seed {seed} must stay excluded");
        assert!(
            excluded
                .patterns
                .iter()
                .any(|p| p == &format!("public.{seed}")),
            "seed {seed} must have its --exclude-table-data pattern"
        );
    }

    // Every foreign key from a kept table into an excluded table is a dump
    // that cannot be restored; there must be none left.
    let rows = db
        .query_all(Statement::from_string(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT c.conname, child.relname AS child, parent.relname AS parent \
             FROM pg_constraint c \
             JOIN pg_class child ON child.oid = c.conrelid \
             JOIN pg_class parent ON parent.oid = c.confrelid \
             JOIN pg_namespace n ON n.oid = child.relnamespace \
             WHERE c.contype = 'f' AND n.nspname = 'public' AND c.conrelid <> c.confrelid"
                .to_string(),
        ))
        .await?;
    let mut dangling = Vec::new();
    for row in rows {
        let child: String = row.try_get("", "child")?;
        let parent: String = row.try_get("", "parent")?;
        let name: String = row.try_get("", "conname")?;
        if set.contains(parent.as_str()) && !set.contains(child.as_str()) {
            dangling.push(format!("{name}: {child} -> {parent}"));
        }
    }
    assert!(
        dangling.is_empty(),
        "kept tables still reference excluded data: {dangling:?}"
    );

    // The case that reached production: request_logs keeps its rows while
    // request_sessions (a seed) does not, and its FK is ON DELETE SET NULL,
    // so the dumped rows carry session ids the dump never restores.
    assert!(
        set.contains("request_logs"),
        "request_logs references request_sessions and must be excluded with it: {:?}",
        excluded.tables
    );
    assert!(excluded.patterns.iter().any(|p| p == "public.request_logs"));

    // Hypertables among the excluded tables get their chunk patterns.
    assert!(
        excluded
            .patterns
            .iter()
            .any(|p| p.starts_with("_timescaledb_internal._hyper_")),
        "hypertable chunk patterns must be present: {:?}",
        excluded.patterns
    );
    Ok(())
}
