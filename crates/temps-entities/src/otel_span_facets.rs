use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};
use temps_core::DBDateTime;

/// A registered OTel span attribute facet.
///
/// When an attribute key is registered as a facet, new spans that carry that
/// attribute will have its value written into the corresponding ClickHouse
/// `facet_attr_N` slot column, and existing spans are back-filled via a
/// ClickHouse mutation. This makes filtering by that attribute value use a
/// bloom-filter skip index rather than a full JSON parse on every row.
///
/// The `slot` is a number 1..=20 mapping to the pre-allocated DDL column
/// `facet_attr_N`. Slots are global across the platform (one ClickHouse
/// `spans` table) and cannot be reused until the previous assignment is
/// deleted and the column cleared.
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "otel_span_facets")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    /// The OTel attribute key (e.g. `enduser.id`, `galachain.contract`).
    /// Must be unique across all facets.
    pub attribute_key: String,
    /// The slot column index 1..=20, mapping to `facet_attr_N` in ClickHouse.
    pub slot: i16,
    /// The user who created this facet mapping, if known.
    pub created_by: Option<i32>,
    pub created_at: DBDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
