//! Runtime configuration service for the admin gate.
//!
//! The gate itself (see `admin_gate.rs`) holds an atomic `AdminGateHandle`
//! that the middleware reads per request. This service owns the *source* of
//! that handle:
//!
//! 1. **Env precedence.** When any of `TEMPS_ADMIN_ALLOWED_IPS`,
//!    `TEMPS_ADMIN_ALLOWED_HOSTS`, or `TEMPS_ADMIN_TRUST_FORWARDED_FOR` is
//!    set, the env values win and the DB is ignored. UI writes are rejected
//!    with a 409. This keeps GitOps/Ansible setups predictable.
//!
//! 2. **DB-backed otherwise.** Settings are stored as a JSON sub-document on
//!    the existing `settings` singleton row under the key `admin_gate`. On
//!    boot, the service loads that row and pushes the result into the
//!    handle. Subsequent writes go through `update()` which validates,
//!    persists, then atomic-swaps the handle.
//!
//! The DB is only touched at boot and on explicit writes — never on the
//! request path.

use std::net::IpAddr;
use std::sync::Arc;

use sea_orm::{ActiveModelTrait, ActiveValue::Set, DatabaseConnection, EntityTrait};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::info;

use super::admin_gate::{AdminGateConfig, AdminGateConfigError, AdminGateHandle, AdminGateSource};

/// JSON shape stored under `settings.data["admin_gate"]`. Versioned so we
/// can evolve the schema without a migration.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AdminGateSettings {
    #[serde(default)]
    pub allowed_ips: Vec<String>,
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    #[serde(default)]
    pub trust_forwarded_for: bool,
}

#[derive(Debug, Error)]
pub enum AdminGateServiceError {
    #[error("Admin gate config invalid: {0}")]
    Invalid(#[from] AdminGateConfigError),

    #[error("Admin gate config is read-only because TEMPS_ADMIN_* env vars are set; unset them to enable runtime configuration")]
    EnvOverridden,

    #[error(
        "Refusing to save: the new rules would deny the caller's connection \
        (ip={caller_ip}, host={caller_host:?}). Add your address/host to the \
        lists or clear the gate before saving."
    )]
    WouldLockOut {
        caller_ip: IpAddr,
        caller_host: Option<String>,
    },

    #[error("Database error: {0}")]
    Database(#[from] sea_orm::DbErr),

    #[error("Failed to (de)serialize admin gate settings: {0}")]
    Serde(#[from] serde_json::Error),
}

/// Runtime service that owns the gate handle and persists user updates.
#[derive(Clone)]
pub struct AdminGateService {
    db: Arc<DatabaseConnection>,
    handle: AdminGateHandle,
    /// True when env vars dictate the active config. Set once at construction
    /// time and never changes — the process must restart to flip modes.
    env_overridden: bool,
}

impl AdminGateService {
    /// Build the service. Resolves the *initial* config according to env
    /// precedence, pushes it into a fresh handle, and returns both halves.
    ///
    /// - If any of the env-derived args is non-default, the active config is
    ///   `AdminGateSource::Env` and the DB row (if any) is left untouched.
    /// - Otherwise, the service reads the `admin_gate` JSON subkey from the
    ///   `settings` row and uses that. Empty row → `AdminGateSource::Default`.
    pub async fn new(
        db: Arc<DatabaseConnection>,
        env_allowed_ips: &[String],
        env_allowed_hosts: &[String],
        env_trust_forwarded_for: bool,
    ) -> Result<(Self, AdminGateHandle), AdminGateServiceError> {
        let env_active =
            !env_allowed_ips.is_empty() || !env_allowed_hosts.is_empty() || env_trust_forwarded_for;

        let initial = if env_active {
            info!(
                allowed_ips = ?env_allowed_ips,
                allowed_hosts = ?env_allowed_hosts,
                trust_forwarded_for = env_trust_forwarded_for,
                "Admin gate: using env-supplied config (DB-backed UI will be read-only)"
            );
            AdminGateConfig::from_env(env_allowed_ips, env_allowed_hosts, env_trust_forwarded_for)?
        } else {
            match load_from_db(db.as_ref()).await {
                Ok(Some(settings)) => {
                    info!(
                        allowed_ips = ?settings.allowed_ips,
                        allowed_hosts = ?settings.allowed_hosts,
                        trust_forwarded_for = settings.trust_forwarded_for,
                        "Admin gate: loaded config from settings row"
                    );
                    AdminGateConfig::from_parts(
                        &settings.allowed_ips,
                        &settings.allowed_hosts,
                        settings.trust_forwarded_for,
                        AdminGateSource::Db,
                    )?
                }
                Ok(None) => AdminGateConfig::from_parts(&[], &[], false, AdminGateSource::Default)?,
                Err(e) => {
                    // SECURITY: fail-CLOSED on load error. Previously
                    // we silently installed a noop config here, which
                    // meant any DB problem (corrupt settings row,
                    // transient DB outage, JSON parse failure) would
                    // open the management surface to the world. That
                    // turns "the gate config is broken" into a
                    // privilege-escalation event for anyone who can
                    // reach the box. Refuse to boot instead — the
                    // operator must explicitly intervene (fix the
                    // row, or set TEMPS_ADMIN_* env vars to bypass
                    // the DB path).
                    tracing::error!(
                        target: "temps_cli::admin_gate",
                        error = %e,
                        "Admin gate: failed to load settings from DB. Refusing to start with an open gate. \
                         Fix the `settings` row, or set TEMPS_ADMIN_ALLOWED_IPS / TEMPS_ADMIN_ALLOWED_HOSTS \
                         to override via env."
                    );
                    return Err(e);
                }
            }
        };

        let handle = AdminGateHandle::new(initial);
        Ok((
            Self {
                db,
                handle: handle.clone(),
                env_overridden: env_active,
            },
            handle,
        ))
    }

    /// True when env vars are the source of truth and the UI must show
    /// read-only.
    pub fn env_overridden(&self) -> bool {
        self.env_overridden
    }

    /// Snapshot the current config.
    pub fn snapshot(&self) -> Arc<AdminGateConfig> {
        self.handle.current()
    }

    /// Persist a new config and swap the live handle.
    ///
    /// `caller_ip` / `caller_host` are used for a lockout pre-flight: if the
    /// new rules would deny the caller, the write is rejected.
    pub async fn update(
        &self,
        new_settings: AdminGateSettings,
        caller_ip: IpAddr,
        caller_host: Option<&str>,
    ) -> Result<Arc<AdminGateConfig>, AdminGateServiceError> {
        if self.env_overridden {
            return Err(AdminGateServiceError::EnvOverridden);
        }

        // Build the candidate config — this also validates CIDRs/hosts.
        let candidate = AdminGateConfig::from_parts(
            &new_settings.allowed_ips,
            &new_settings.allowed_hosts,
            new_settings.trust_forwarded_for,
            AdminGateSource::Db,
        )?;

        // Lockout pre-flight: only meaningful when the new config is *not*
        // a noop. A noop config allows everyone, so it can never lock out.
        if !candidate.is_noop() && !candidate.would_allow(caller_ip, caller_host) {
            return Err(AdminGateServiceError::WouldLockOut {
                caller_ip,
                caller_host: caller_host.map(|s| s.to_string()),
            });
        }

        persist_to_db(self.db.as_ref(), &new_settings).await?;
        let prev = self.handle.store(candidate.clone());
        info!(
            allowed_ips = ?new_settings.allowed_ips,
            allowed_hosts = ?new_settings.allowed_hosts,
            trust_forwarded_for = new_settings.trust_forwarded_for,
            previous_source = ?prev.source,
            "Admin gate: configuration reloaded from DB"
        );
        Ok(self.handle.current())
    }

    /// Re-read the gate config from the DB and swap the live handle.
    ///
    /// Used by the `settings_change` listener so a process that did NOT
    /// perform the write (the standalone `temps proxy`, whose console lives in
    /// a different process) still converges on the operator's current config.
    ///
    /// Fails SAFE: on a load error the previous config is retained rather than
    /// falling back to an open gate — a transient DB blip must never widen the
    /// management surface. Env-overridden processes are a no-op, matching
    /// `update()`'s precedence rule.
    pub async fn reload_from_db(&self) -> Result<Arc<AdminGateConfig>, AdminGateServiceError> {
        if self.env_overridden {
            return Ok(self.handle.current());
        }

        let config = match load_from_db(self.db.as_ref()).await? {
            Some(settings) => AdminGateConfig::from_parts(
                &settings.allowed_ips,
                &settings.allowed_hosts,
                settings.trust_forwarded_for,
                AdminGateSource::Db,
            )?,
            None => AdminGateConfig::from_parts(&[], &[], false, AdminGateSource::Default)?,
        };

        let prev = self.handle.store(config);
        info!(
            previous_source = ?prev.source,
            "Admin gate: reloaded after settings_change notification"
        );
        Ok(self.handle.current())
    }

    /// Spawn the background task that LISTENs on the Postgres `settings_change`
    /// channel and reloads the gate whenever ANY process writes the settings
    /// row.
    ///
    /// In the single-binary `temps serve` the console and the Pingora proxy
    /// share one `AdminGateHandle`, so a UI save is visible to both instantly.
    /// In the ADR-017 split topology they are separate processes with separate
    /// handles: without this listener the standalone `temps proxy` enforces
    /// whatever allowlist existed when it booted, forever. That divergence is
    /// visible in both directions — a newly saved allowlist isn't enforced at
    /// the proxy (management surface stays reachable from disallowed hosts),
    /// and a cleared allowlist keeps 404ing hosts that should now fall through
    /// to the console.
    ///
    /// Startup failure to connect is non-fatal: the gate keeps its boot-time
    /// config (the pre-existing behavior) and the operator can still restart
    /// the process to pick up changes.
    pub fn start_settings_listener(self: &Arc<Self>, database_url: String) {
        let service = Arc::clone(self);

        // Env precedence: the DB is not the source of truth here, so there is
        // nothing to converge on.
        if service.env_overridden {
            return;
        }

        tokio::spawn(async move {
            use sqlx::postgres::{PgListener, PgPool};

            let pool = match PgPool::connect(&database_url).await {
                Ok(pool) => pool,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "Admin gate: failed to connect for settings_change LISTEN; \
                         the gate will keep its boot-time config until this process restarts"
                    );
                    return;
                }
            };

            let mut listener = match PgListener::connect_with(&pool).await {
                Ok(listener) => listener,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "Admin gate: failed to create PgListener; \
                         the gate will keep its boot-time config until this process restarts"
                    );
                    return;
                }
            };

            if let Err(e) = listener.listen("settings_change").await {
                tracing::warn!(
                    error = %e,
                    "Admin gate: failed to subscribe to settings_change; \
                     the gate will keep its boot-time config until this process restarts"
                );
                return;
            }
            info!("Admin gate: listening for settings_change to refresh the allowlist");

            loop {
                match listener.recv().await {
                    Ok(_) => {
                        if let Err(e) = service.reload_from_db().await {
                            // Keep the previous (stricter or equal) config.
                            tracing::error!(
                                error = %e,
                                "Admin gate: reload after settings_change failed; \
                                 keeping the previously active configuration"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "Admin gate: settings_change listener error; reconnecting"
                        );
                        // Reconnect, then reload once to catch anything missed
                        // during the gap.
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        if listener.listen("settings_change").await.is_ok() {
                            let _ = service.reload_from_db().await;
                        } else {
                            return;
                        }
                    }
                }
            }
        });
    }
}

/// Read the `admin_gate` key off the singleton `settings` row. Returns
/// `Ok(None)` when either the row doesn't exist yet or the key isn't set —
/// both mean "no DB config", and the caller will fall back to defaults.
async fn load_from_db(
    db: &DatabaseConnection,
) -> Result<Option<AdminGateSettings>, AdminGateServiceError> {
    let row = temps_entities::settings::Entity::find_by_id(1)
        .one(db)
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    match row.data.get("admin_gate").cloned() {
        Some(val) if !val.is_null() => {
            let settings: AdminGateSettings = serde_json::from_value(val)?;
            Ok(Some(settings))
        }
        _ => Ok(None),
    }
}

/// Write `admin_gate` into the singleton `settings` row, creating it if
/// necessary. Uses an upsert via either insert (no row) or update (row
/// present + merge sub-key).
async fn persist_to_db(
    db: &DatabaseConnection,
    new_settings: &AdminGateSettings,
) -> Result<(), AdminGateServiceError> {
    let now = chrono::Utc::now();
    let row = temps_entities::settings::Entity::find_by_id(1)
        .one(db)
        .await?;
    let new_value = serde_json::to_value(new_settings)?;

    match row {
        Some(existing) => {
            let mut data = existing.data.clone();
            match data.as_object_mut() {
                Some(map) => {
                    map.insert("admin_gate".to_string(), new_value);
                }
                None => {
                    // Settings row had a non-object blob — replace it with a
                    // fresh object that contains just our key. Other keys
                    // would already be lost in this case.
                    data = serde_json::json!({ "admin_gate": new_value });
                }
            }
            let mut am: temps_entities::settings::ActiveModel = existing.into();
            am.data = Set(data);
            am.updated_at = Set(now);
            am.update(db).await?;
        }
        None => {
            let am = temps_entities::settings::ActiveModel {
                id: Set(1),
                data: Set(serde_json::json!({ "admin_gate": new_value })),
                created_at: Set(now),
                updated_at: Set(now),
            };
            am.insert(db).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{DatabaseBackend, MockDatabase};
    use std::net::Ipv4Addr;

    fn settings_row(data: serde_json::Value) -> temps_entities::settings::Model {
        temps_entities::settings::Model {
            id: 1,
            data,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn env_active_skips_db() {
        // MockDatabase with zero queued results — if we touched the DB this
        // would panic.
        let db = MockDatabase::new(DatabaseBackend::Postgres).into_connection();
        let (svc, handle) =
            AdminGateService::new(Arc::new(db), &["10.0.0.0/8".to_string()], &[], false)
                .await
                .unwrap();
        assert!(svc.env_overridden());
        assert_eq!(handle.current().source, AdminGateSource::Env);
        assert_eq!(handle.current().allowed_nets.len(), 1);
    }

    #[tokio::test]
    async fn env_unset_loads_from_db_when_present() {
        let db = MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results(vec![vec![settings_row(serde_json::json!({
                "admin_gate": {
                    "allowed_ips": ["192.168.0.0/16"],
                    "allowed_hosts": ["admin.example.com"],
                    "trust_forwarded_for": true
                }
            }))]])
            .into_connection();
        let (svc, handle) = AdminGateService::new(Arc::new(db), &[], &[], false)
            .await
            .unwrap();
        assert!(!svc.env_overridden());
        let cfg = handle.current();
        assert_eq!(cfg.source, AdminGateSource::Db);
        assert_eq!(cfg.allowed_nets.len(), 1);
        assert_eq!(cfg.allowed_hosts.len(), 1);
        assert!(cfg.trust_forwarded_for);
    }

    #[tokio::test]
    async fn env_unset_no_db_row_uses_default() {
        let db = MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results(vec![Vec::<temps_entities::settings::Model>::new()])
            .into_connection();
        let (_svc, handle) = AdminGateService::new(Arc::new(db), &[], &[], false)
            .await
            .unwrap();
        let cfg = handle.current();
        assert_eq!(cfg.source, AdminGateSource::Default);
        assert!(cfg.is_noop());
    }

    #[tokio::test]
    async fn update_refused_when_env_overridden() {
        let db = MockDatabase::new(DatabaseBackend::Postgres).into_connection();
        let (svc, _handle) =
            AdminGateService::new(Arc::new(db), &["10.0.0.0/8".to_string()], &[], false)
                .await
                .unwrap();
        let result = svc
            .update(
                AdminGateSettings::default(),
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                None,
            )
            .await;
        assert!(matches!(
            result.unwrap_err(),
            AdminGateServiceError::EnvOverridden
        ));
    }

    #[tokio::test]
    async fn update_refused_when_caller_would_be_locked_out() {
        // Boot with no env, no DB row → default (open) config.
        let db = MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results(vec![Vec::<temps_entities::settings::Model>::new()])
            .into_connection();
        let (svc, _handle) = AdminGateService::new(Arc::new(db), &[], &[], false)
            .await
            .unwrap();

        // Try to lock the gate to a CIDR that doesn't include the caller.
        let result = svc
            .update(
                AdminGateSettings {
                    allowed_ips: vec!["10.0.0.0/8".to_string()],
                    ..Default::default()
                },
                IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)),
                Some("anywhere"),
            )
            .await;
        assert!(matches!(
            result.unwrap_err(),
            AdminGateServiceError::WouldLockOut { .. }
        ));
    }

    /// The split-topology convergence path: a write performed by the *console*
    /// process must become effective in the *proxy* process, which only sees
    /// the `settings_change` NOTIFY.
    #[tokio::test]
    async fn reload_from_db_picks_up_another_process_write() {
        let db = MockDatabase::new(DatabaseBackend::Postgres)
            // Boot: gate empty.
            .append_query_results(vec![vec![settings_row(serde_json::json!({}))]])
            // After the console saved an allowlist.
            .append_query_results(vec![vec![settings_row(serde_json::json!({
                "admin_gate": {
                    "allowed_ips": [],
                    "allowed_hosts": ["app.temps.kfs.es"],
                    "trust_forwarded_for": false
                }
            }))]])
            .into_connection();

        let (svc, handle) = AdminGateService::new(Arc::new(db), &[], &[], false)
            .await
            .unwrap();
        assert!(handle.current().is_noop(), "boots with an empty gate");

        svc.reload_from_db().await.unwrap();

        let cfg = handle.current();
        assert_eq!(cfg.source, AdminGateSource::Db);
        assert_eq!(cfg.allowed_hosts.len(), 1);
    }

    /// A cleared allowlist must also converge — otherwise the proxy keeps
    /// 404ing hosts the operator has since unblocked.
    #[tokio::test]
    async fn reload_from_db_converges_when_gate_is_cleared() {
        let db = MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results(vec![vec![settings_row(serde_json::json!({
                "admin_gate": {
                    "allowed_ips": [],
                    "allowed_hosts": ["app.temps.kfs.es"],
                    "trust_forwarded_for": false
                }
            }))]])
            // Console cleared the key.
            .append_query_results(vec![vec![settings_row(serde_json::json!({}))]])
            .into_connection();

        let (svc, handle) = AdminGateService::new(Arc::new(db), &[], &[], false)
            .await
            .unwrap();
        assert!(!handle.current().is_noop());

        svc.reload_from_db().await.unwrap();

        assert!(handle.current().is_noop());
        assert_eq!(handle.current().source, AdminGateSource::Default);
    }

    /// Env precedence holds on the reload path too: a DB write must not be
    /// able to override an env-pinned gate.
    #[tokio::test]
    async fn reload_from_db_is_noop_when_env_overridden() {
        // Zero queued results — touching the DB would panic.
        let db = MockDatabase::new(DatabaseBackend::Postgres).into_connection();
        let (svc, handle) =
            AdminGateService::new(Arc::new(db), &["10.0.0.0/8".to_string()], &[], false)
                .await
                .unwrap();

        svc.reload_from_db().await.unwrap();

        assert_eq!(handle.current().source, AdminGateSource::Env);
        assert_eq!(handle.current().allowed_nets.len(), 1);
    }

    #[tokio::test]
    async fn update_allows_when_caller_in_new_range() {
        // Boot finds an existing (empty) settings row → the persist path
        // takes the UPDATE branch. Sea-ORM's `ActiveModel::update()` issues
        // `UPDATE ... RETURNING *` on PostgreSQL, so the mock needs the
        // returning row queued as a query result, not an exec result.
        let bootstrap_row = settings_row(serde_json::json!({}));
        let returned_row = settings_row(serde_json::json!({
            "admin_gate": {
                "allowed_ips": ["10.0.0.0/8"],
                "allowed_hosts": ["admin.example.com"],
                "trust_forwarded_for": false
            }
        }));
        let db = MockDatabase::new(DatabaseBackend::Postgres)
            // Initial load: empty admin_gate key.
            .append_query_results(vec![vec![bootstrap_row.clone()]])
            // persist_to_db re-reads the row.
            .append_query_results(vec![vec![bootstrap_row.clone()]])
            // UPDATE ... RETURNING * — returns the new row.
            .append_query_results(vec![vec![returned_row]])
            .into_connection();
        let (svc, handle) = AdminGateService::new(Arc::new(db), &[], &[], false)
            .await
            .unwrap();

        let caller = IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3));
        svc.update(
            AdminGateSettings {
                allowed_ips: vec!["10.0.0.0/8".to_string()],
                allowed_hosts: vec!["admin.example.com".to_string()],
                trust_forwarded_for: false,
            },
            caller,
            Some("admin.example.com"),
        )
        .await
        .unwrap();

        let cfg = handle.current();
        assert_eq!(cfg.source, AdminGateSource::Db);
        assert_eq!(cfg.allowed_nets.len(), 1);
        assert_eq!(cfg.allowed_hosts.len(), 1);
    }
}
