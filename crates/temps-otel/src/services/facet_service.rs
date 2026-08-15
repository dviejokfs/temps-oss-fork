//! Service for managing OTel span attribute facets.
//!
//! A "facet" is an arbitrary OTel attribute key (e.g. `enduser.id`,
//! `galachain.contract`) that an admin has marked for fast filtering. Instead
//! of parsing the `attributes` JSON blob for every span on every query, the
//! value is written into a pre-allocated `facet_attr_N` slot column in the
//! ClickHouse `spans` table and indexed with a bloom filter.
//!
//! The mapping from attribute key → slot is stored in the Postgres
//! `otel_span_facets` table and cached in memory via an `ArcSwap`
//! for lock-free reads on the hot ingest/query paths.
//!
//! # Scale note
//!
//! ClickHouse mutations (`ALTER TABLE … UPDATE`) are synchronous here because
//! the demo dataset is small (~6800 rows, near-instant). At 500M+ row scale,
//! mutations are expensive and should be tracked asynchronously with progress
//! monitoring. A production implementation would return immediately after
//! kicking off the mutation and expose a status endpoint.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use chrono::Utc;
use sea_orm::ActiveValue::Set;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use utoipa::ToSchema;

use temps_entities::otel_span_facets::{ActiveModel, Column, Entity};

// ── Error type ─────────────────────────────────────────────────────────────

#[derive(Error, Debug)]
pub enum FacetError {
    #[error("Attribute key '{attribute_key}' is already registered as a facet")]
    AlreadyFaceted { attribute_key: String },

    #[error("All {max_slots} facet slots are in use; delete a facet before adding a new one")]
    CapacityExceeded { max_slots: u8 },

    #[error("Facet for attribute key '{attribute_key}' not found")]
    NotFound { attribute_key: String },

    #[error("Validation error: {message}")]
    Validation { message: String },

    #[error("Database error: {0}")]
    Database(#[from] sea_orm::DbErr),

    #[error("ClickHouse storage error during facet operation: {message}")]
    Storage { message: String },
}

// ── DTO ─────────────────────────────────────────────────────────────────────

/// Public representation of a registered span attribute facet.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct FacetInfo {
    /// The OTel attribute key, e.g. `enduser.id` or `galachain.contract`.
    pub attribute_key: String,
    /// The slot column index 1..=20 in ClickHouse (`facet_attr_N`).
    pub slot: u8,
    #[schema(example = "2025-10-12T12:15:47.609192Z")]
    pub created_at: String,
}

impl From<temps_entities::otel_span_facets::Model> for FacetInfo {
    fn from(m: temps_entities::otel_span_facets::Model) -> Self {
        Self {
            attribute_key: m.attribute_key,
            slot: m.slot as u8,
            created_at: m.created_at.to_rfc3339(),
        }
    }
}

// ── The maximum pool of pre-allocated slots ──────────────────────────────────

pub const MAX_FACET_SLOTS: u8 = 20;

// ── Service ─────────────────────────────────────────────────────────────────

/// Shared type alias for the facet attribute-key → slot cache.
///
/// Values are 1-indexed (1..=20). Lock-free reads via `ArcSwap::load()`.
pub type FacetCache = Arc<ArcSwap<HashMap<String, u8>>>;

/// Service managing OTel span attribute facets.
///
/// Holds a Postgres connection for config persistence, an optional ClickHouse
/// client for DDL mutations, and a shared in-memory cache for the hot ingest
/// and query paths.
pub struct FacetService {
    db: Arc<DatabaseConnection>,
    /// Optional: present when ClickHouse is configured. Absent means
    /// `create_facet` and `delete_facet` skip the ClickHouse mutation step
    /// and log a warning — the Postgres facet table is still managed.
    ch_client: Option<::clickhouse::Client>,
    /// Shared with `ClickHouseOtelStorage` so both ingest and query paths see
    /// the same mapping without any extra coordination.
    pub facet_cache: FacetCache,
}

impl FacetService {
    /// Construct a new `FacetService`.
    ///
    /// Call [`refresh_cache`] after construction to load the initial facet map
    /// from Postgres.
    pub fn new(
        db: Arc<DatabaseConnection>,
        ch_client: Option<::clickhouse::Client>,
        facet_cache: FacetCache,
    ) -> Self {
        Self {
            db,
            ch_client,
            facet_cache,
        }
    }

    // ── Public API ─────────────────────────────────────────────────────────

    /// List all registered facets (newest first).
    pub async fn list_facets(&self) -> Result<Vec<FacetInfo>, FacetError> {
        let models = Entity::find()
            .order_by_desc(Column::CreatedAt)
            .all(self.db.as_ref())
            .await?;

        Ok(models.into_iter().map(FacetInfo::from).collect())
    }

    /// Register `attribute_key` as a facet.
    ///
    /// Validates the key, assigns the lowest free slot (1..=20), inserts the
    /// Postgres row, and runs a ClickHouse backfill mutation so existing spans
    /// that carry the attribute are populated in the new slot column.
    ///
    /// If the ClickHouse mutation fails, the Postgres row is rolled back so
    /// no partial state is left behind.
    pub async fn create_facet(
        &self,
        attribute_key: String,
        created_by: Option<i32>,
    ) -> Result<FacetInfo, FacetError> {
        // 1. Validate
        let key = attribute_key.trim().to_string();
        if key.is_empty() {
            return Err(FacetError::Validation {
                message: "attribute_key must not be empty".to_string(),
            });
        }
        if key.len() > 200 {
            return Err(FacetError::Validation {
                message: format!(
                    "attribute_key must be at most 200 characters (got {})",
                    key.len()
                ),
            });
        }

        // 2. Check for duplicate
        let existing: Vec<temps_entities::otel_span_facets::Model> =
            Entity::find().all(self.db.as_ref()).await?;

        if existing.iter().any(|m| m.attribute_key == key) {
            return Err(FacetError::AlreadyFaceted { attribute_key: key });
        }

        // 3. Find the lowest free slot (1..=MAX_FACET_SLOTS)
        let used_slots: std::collections::HashSet<i16> = existing.iter().map(|m| m.slot).collect();
        let slot = (1..=(MAX_FACET_SLOTS as i16))
            .find(|s| !used_slots.contains(s))
            .ok_or(FacetError::CapacityExceeded {
                max_slots: MAX_FACET_SLOTS,
            })?;

        // 4. Insert the Postgres row
        let now = Utc::now();
        let active = ActiveModel {
            attribute_key: Set(key.clone()),
            slot: Set(slot),
            created_by: Set(created_by),
            created_at: Set(now),
            ..Default::default()
        };
        let model = active.insert(self.db.as_ref()).await?;

        // 5. Run ClickHouse backfill mutation for existing historical spans.
        //
        // This is a ClickHouse UPDATE mutation:
        //   ALTER TABLE spans UPDATE facet_attr_{slot} = JSONExtractString(attributes, {key})
        //   WHERE JSONHas(attributes, {key})
        //
        // NOTE: At 500M+ row scale this mutation would be expensive and should
        // be queued as a background job with progress tracking. For this
        // prototype (demo dataset ~6800 rows) it runs synchronously — at that
        // scale it completes in milliseconds.
        if let Some(ref ch) = self.ch_client {
            let column = facet_column_name(slot as u8);
            let backfill_sql = format!(
                "ALTER TABLE spans UPDATE {column} = JSONExtractString(attributes, ?) \
                 WHERE JSONHas(attributes, ?)"
            );
            if let Err(e) = ch
                .query(&backfill_sql)
                .bind(key.as_str())
                .bind(key.as_str())
                .execute()
                .await
            {
                // Roll back the Postgres row to avoid partial state.
                let delete_result =
                    temps_entities::otel_span_facets::Entity::delete_by_id(model.id)
                        .exec(self.db.as_ref())
                        .await;
                if let Err(del_err) = delete_result {
                    tracing::error!(
                        attribute_key = %key,
                        slot,
                        error = %del_err,
                        "Failed to roll back facet Postgres row after ClickHouse mutation failure"
                    );
                }
                return Err(FacetError::Storage {
                    message: format!(
                        "ClickHouse backfill mutation for facet slot {slot} \
                         (attribute_key={key:?}) failed: {e}"
                    ),
                });
            }
        } else {
            tracing::warn!(
                attribute_key = %key,
                slot,
                "ClickHouse client not configured; skipping backfill mutation for new facet. \
                 Existing spans will not have this attribute pre-populated in the slot column."
            );
        }

        // 6. Refresh the in-memory cache.
        self.refresh_cache().await?;

        Ok(FacetInfo::from(model))
    }

    /// Remove a facet by `attribute_key`.
    ///
    /// Clears the slot column in ClickHouse (to prevent stale data leaking
    /// into a future facet that reuses the same slot), then deletes the
    /// Postgres row and refreshes the cache.
    pub async fn delete_facet(&self, attribute_key: &str) -> Result<(), FacetError> {
        // 1. Look up the row
        let model = Entity::find()
            .filter(Column::AttributeKey.eq(attribute_key))
            .one(self.db.as_ref())
            .await?
            .ok_or_else(|| FacetError::NotFound {
                attribute_key: attribute_key.to_string(),
            })?;

        let slot = model.slot as u8;

        // 2. Clear the ClickHouse column before freeing the slot.
        //
        // Correctness: if we free the slot first (delete Postgres row), a
        // concurrent `create_facet` could reuse it and see the old values.
        // Clearing first ensures the column is empty when the slot is reused.
        if let Some(ref ch) = self.ch_client {
            let column = facet_column_name(slot);
            let clear_sql =
                format!("ALTER TABLE spans UPDATE {column} = NULL WHERE {column} IS NOT NULL");
            if let Err(e) = ch.query(&clear_sql).execute().await {
                return Err(FacetError::Storage {
                    message: format!("ClickHouse clear mutation for facet slot {slot} failed: {e}"),
                });
            }
        } else {
            tracing::warn!(
                attribute_key = %attribute_key,
                slot,
                "ClickHouse client not configured; skipping column-clear mutation for deleted facet. \
                 Stale values may remain in the slot column until the next CH rewrite."
            );
        }

        // 3. Delete the Postgres row
        temps_entities::otel_span_facets::Entity::delete_by_id(model.id)
            .exec(self.db.as_ref())
            .await?;

        // 4. Refresh the cache
        self.refresh_cache().await?;

        Ok(())
    }

    // ── Internal helpers ───────────────────────────────────────────────────

    /// Reload all facets from Postgres and swap the in-memory cache.
    ///
    /// Called after every create/delete and once at construction time.
    pub async fn refresh_cache(&self) -> Result<(), FacetError> {
        let rows: Vec<temps_entities::otel_span_facets::Model> =
            Entity::find().all(self.db.as_ref()).await?;

        let map: HashMap<String, u8> = rows
            .into_iter()
            .map(|m| (m.attribute_key, m.slot as u8))
            .collect();

        self.facet_cache.store(Arc::new(map));
        Ok(())
    }
}

// ── Column name helper ───────────────────────────────────────────────────────

/// Map a 1-indexed slot number (1..=20) to the ClickHouse column name.
///
/// Returns `"facet_attr_N"` — must match the DDL in `0008_facet_slots.sql`
/// exactly. The slot is validated to be in range before this function is
/// called.
pub fn facet_column_name(slot: u8) -> String {
    format!("facet_attr_{slot}")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facet_column_name_correct() {
        assert_eq!(facet_column_name(1), "facet_attr_1");
        assert_eq!(facet_column_name(10), "facet_attr_10");
        assert_eq!(facet_column_name(20), "facet_attr_20");
    }

    #[test]
    fn facet_error_display_already_faceted() {
        let err = FacetError::AlreadyFaceted {
            attribute_key: "enduser.id".to_string(),
        };
        assert!(err.to_string().contains("enduser.id"));
        assert!(err.to_string().contains("already registered"));
    }

    #[test]
    fn facet_error_display_capacity_exceeded() {
        let err = FacetError::CapacityExceeded { max_slots: 20 };
        assert!(err.to_string().contains("20"));
        assert!(err.to_string().contains("slots are in use"));
    }

    #[test]
    fn facet_error_display_not_found() {
        let err = FacetError::NotFound {
            attribute_key: "galachain.contract".to_string(),
        };
        assert!(err.to_string().contains("galachain.contract"));
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn facet_error_display_validation() {
        let err = FacetError::Validation {
            message: "attribute_key must not be empty".to_string(),
        };
        assert!(err.to_string().contains("Validation error"));
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn facet_info_from_model() {
        let model = temps_entities::otel_span_facets::Model {
            id: 1,
            attribute_key: "enduser.id".to_string(),
            slot: 3,
            created_by: Some(42),
            created_at: chrono::Utc::now(),
        };
        let info = FacetInfo::from(model);
        assert_eq!(info.attribute_key, "enduser.id");
        assert_eq!(info.slot, 3);
        assert!(info.created_at.ends_with('Z') || info.created_at.contains('+'));
    }
}
