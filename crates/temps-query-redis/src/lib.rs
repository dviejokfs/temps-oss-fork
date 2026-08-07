//! Redis implementation of the temps-query DataSource trait
//!
//! This crate provides Redis key-value store access through the generic query interface.
//!
//! ## Hierarchy
//!
//! Redis uses a flat namespace with databases (0-15 by default) and keys:
//! - Depth 0: List databases (0-15)
//! - Depth 1: List keys in database (with pattern matching)
//!
//! ## Example
//!
//! ```rust,no_run
//! use temps_query_redis::RedisSource;
//! use temps_query::DataSource;
//!
//! # async fn example() -> temps_query::Result<()> {
//! let source = RedisSource::new("redis://localhost:6379").await?;
//!
//! // List databases
//! let databases = source.list_containers(&temps_query::ContainerPath::root()).await?;
//!
//! // List keys in database 0
//! let path = temps_query::ContainerPath::from_slice(&["0"]);
//! let keys = source.list_entities(&path).await?;
//! # Ok(())
//! # }
//! ```

use async_trait::async_trait;
use redis::aio::ConnectionManager;
use redis::{AsyncCommands, RedisError};
use std::collections::HashMap;
use temps_query::{
    BoundedRows, Capability, ContainerCapabilities, ContainerInfo, ContainerPath, ContainerType,
    DataError, DataRow, DataSource, DatasetSchema, EntityCountHint, EntityInfo, FieldDef,
    FieldType, QueryOptions, QueryResult, QueryStats, Queryable, Result,
};
use tracing::{debug, error};

/// Redis data source implementation
pub struct RedisSource {
    connection: ConnectionManager,
}

/// Bounds the work performed by the atomic SSCAN/HSCAN admission scripts so a
/// data-browser request cannot monopolize the linked Redis server.
const MAX_REDIS_VALUE_OFFSET: usize = 10_000;

fn redis_aggregate_limit(options: &QueryOptions) -> usize {
    // A JSON array/object consumes at least two structural elements per Redis
    // item (container entry + scalar), and zset rows consume more. Dividing by
    // four is deliberately conservative and keeps the shared structural
    // limiter from accepting a backend page that was already too large.
    let structural_limit = (options.budget.max_value_elements_per_row / 4).max(1);
    options.limit.unwrap_or(100).min(structural_limit)
}

impl RedisSource {
    fn wire_budget_error(key: &str, limit: usize, observed: usize) -> DataError {
        DataError::ResultLimitExceeded {
            entity: key.to_string(),
            limit_kind: "cell_bytes",
            limit,
            observed,
        }
    }

    async fn scan_set_page(
        conn: &mut ConnectionManager,
        key: &str,
        offset: usize,
        limit: usize,
        max_cell_bytes: usize,
    ) -> Result<(Vec<String>, bool)> {
        const SCRIPT: &str = r#"
local cursor = '0'
local skipped = 0
local values = {}
local estimated = 2
local has_more = 0
repeat
  local page = redis.call('SSCAN', KEYS[1], cursor, 'COUNT', math.min(tonumber(ARGV[2]) + 1, 512))
  cursor = page[1]
  for _, value in ipairs(page[2]) do
    if skipped < tonumber(ARGV[1]) then
      skipped = skipped + 1
    elseif #values >= tonumber(ARGV[2]) then
      has_more = 1
      break
    else
      local next_estimate = estimated + string.len(value) * 6 + 8
      if next_estimate > tonumber(ARGV[3]) then
        return {0, next_estimate, 0, {}}
      end
      table.insert(values, value)
      estimated = next_estimate
    end
  end
until has_more == 1 or cursor == '0'
if cursor ~= '0' then has_more = 1 end
return {1, estimated, has_more, values}
"#;
        let (admitted, observed, has_more, values): (i64, usize, i64, Vec<String>) =
            redis::Script::new(SCRIPT)
                .key(key)
                .arg(offset)
                .arg(limit)
                .arg(max_cell_bytes)
                .invoke_async(conn)
                .await
                .map_err(|error: RedisError| {
                    error!(error = %error, "failed to safely page Redis set");
                    DataError::BackendQueryFailed {
                        backend: "Redis",
                        entity: key.to_string(),
                    }
                })?;
        if admitted == 0 {
            return Err(Self::wire_budget_error(key, max_cell_bytes, observed));
        }
        Ok((values, has_more != 0))
    }

    async fn scan_hash_page(
        conn: &mut ConnectionManager,
        key: &str,
        offset: usize,
        limit: usize,
        max_cell_bytes: usize,
    ) -> Result<(Vec<(String, String)>, bool)> {
        const SCRIPT: &str = r#"
local cursor = '0'
local skipped = 0
local values = {}
local estimated = 2
local has_more = 0
repeat
  local page = redis.call('HSCAN', KEYS[1], cursor, 'COUNT', math.min(tonumber(ARGV[2]) + 1, 512))
  cursor = page[1]
  for index = 1, #page[2], 2 do
    if skipped < tonumber(ARGV[1]) then
      skipped = skipped + 1
    elseif (#values / 2) >= tonumber(ARGV[2]) then
      has_more = 1
      break
    else
      local field = page[2][index]
      local value = page[2][index + 1]
      local next_estimate = estimated + (string.len(field) + string.len(value)) * 6 + 16
      if next_estimate > tonumber(ARGV[3]) then
        return {0, next_estimate, 0, {}}
      end
      table.insert(values, field)
      table.insert(values, value)
      estimated = next_estimate
    end
  end
until has_more == 1 or cursor == '0'
if cursor ~= '0' then has_more = 1 end
return {1, estimated, has_more, values}
"#;
        let (admitted, observed, has_more, flat_values): (i64, usize, i64, Vec<String>) =
            redis::Script::new(SCRIPT)
                .key(key)
                .arg(offset)
                .arg(limit)
                .arg(max_cell_bytes)
                .invoke_async(conn)
                .await
                .map_err(|error: RedisError| {
                    error!(error = %error, "failed to safely page Redis hash");
                    DataError::BackendQueryFailed {
                        backend: "Redis",
                        entity: key.to_string(),
                    }
                })?;
        if admitted == 0 {
            return Err(Self::wire_budget_error(key, max_cell_bytes, observed));
        }
        let values = flat_values
            .chunks_exact(2)
            .map(|pair| (pair[0].clone(), pair[1].clone()))
            .collect();
        Ok((values, has_more != 0))
    }

    async fn list_page(
        conn: &mut ConnectionManager,
        key: &str,
        offset: usize,
        limit: usize,
        max_cell_bytes: usize,
    ) -> Result<(Vec<String>, bool)> {
        const SCRIPT: &str = r#"
local values = redis.call('LRANGE', KEYS[1], tonumber(ARGV[1]), tonumber(ARGV[1]) + tonumber(ARGV[2]))
local has_more = (#values > tonumber(ARGV[2])) and 1 or 0
if has_more == 1 then table.remove(values) end
local estimated = 2
for _, value in ipairs(values) do
  estimated = estimated + string.len(value) * 6 + 8
  if estimated > tonumber(ARGV[3]) then return {0, estimated, 0, {}} end
end
return {1, estimated, has_more, values}
"#;
        let (admitted, observed, has_more, values): (i64, usize, i64, Vec<String>) =
            redis::Script::new(SCRIPT)
                .key(key)
                .arg(offset)
                .arg(limit)
                .arg(max_cell_bytes)
                .invoke_async(conn)
                .await
                .map_err(|error: RedisError| {
                    error!(error = %error, "failed to safely page Redis list");
                    DataError::BackendQueryFailed {
                        backend: "Redis",
                        entity: key.to_string(),
                    }
                })?;
        if admitted == 0 {
            return Err(Self::wire_budget_error(key, max_cell_bytes, observed));
        }
        Ok((values, has_more != 0))
    }

    async fn zset_page(
        conn: &mut ConnectionManager,
        key: &str,
        offset: usize,
        limit: usize,
        max_cell_bytes: usize,
    ) -> Result<(Vec<(String, f64)>, bool)> {
        const SCRIPT: &str = r#"
local values = redis.call('ZRANGE', KEYS[1], tonumber(ARGV[1]), tonumber(ARGV[1]) + tonumber(ARGV[2]), 'WITHSCORES')
local has_more = ((#values / 2) > tonumber(ARGV[2])) and 1 or 0
if has_more == 1 then table.remove(values); table.remove(values) end
local estimated = 2
for index = 1, #values, 2 do
  estimated = estimated + (string.len(values[index]) + string.len(values[index + 1])) * 6 + 24
  if estimated > tonumber(ARGV[3]) then return {0, estimated, 0, {}} end
end
return {1, estimated, has_more, values}
"#;
        let (admitted, observed, has_more, flat_values): (i64, usize, i64, Vec<String>) =
            redis::Script::new(SCRIPT)
                .key(key)
                .arg(offset)
                .arg(limit)
                .arg(max_cell_bytes)
                .invoke_async(conn)
                .await
                .map_err(|error: RedisError| {
                    error!(error = %error, "failed to safely page Redis sorted set");
                    DataError::BackendQueryFailed {
                        backend: "Redis",
                        entity: key.to_string(),
                    }
                })?;
        if admitted == 0 {
            return Err(Self::wire_budget_error(key, max_cell_bytes, observed));
        }
        let mut values = Vec::with_capacity(flat_values.len() / 2);
        for pair in flat_values.chunks_exact(2) {
            let score = pair[1].parse::<f64>().map_err(|error| {
                error!(error = %error, "failed to decode Redis sorted-set score");
                DataError::BackendQueryFailed {
                    backend: "Redis",
                    entity: key.to_string(),
                }
            })?;
            values.push((pair[0].clone(), score));
        }
        Ok((values, has_more != 0))
    }

    /// Create a new Redis data source
    ///
    /// # Arguments
    ///
    /// * `url` - Redis connection URL (e.g., "redis://localhost:6379")
    pub async fn new(url: &str) -> Result<Self> {
        debug!("Creating Redis source for URL: {}", url);

        let client = redis::Client::open(url).map_err(|e| {
            error!("Failed to create Redis client: {}", e);
            DataError::ConnectionFailed(format!("Failed to create Redis client: {}", e))
        })?;

        let connection = ConnectionManager::new(client).await.map_err(|e| {
            error!("Failed to connect to Redis: {}", e);
            DataError::ConnectionFailed(format!("Failed to connect to Redis: {}", e))
        })?;

        debug!("Redis client created successfully");

        Ok(Self { connection })
    }

    /// Get connection to a specific Redis database
    async fn get_db_connection(&self, db: i32) -> Result<ConnectionManager> {
        let mut conn = self.connection.clone();

        // Select the database
        redis::cmd("SELECT")
            .arg(db)
            .query_async::<()>(&mut conn)
            .await
            .map_err(|e: RedisError| {
                error!("Failed to select database {}: {}", db, e);
                DataError::QueryFailed(format!("Failed to select database {}: {}", db, e))
            })?;

        Ok(conn)
    }

    /// List keys in a specific database with optional pattern (limited to first 1000 keys)
    async fn list_keys_in_db(&self, db: i32, pattern: Option<&str>) -> Result<Vec<EntityInfo>> {
        // Use paginated version with limit for safety
        let (entities, _) = self
            .list_keys_in_db_paginated(db, pattern, 1000, None)
            .await?;
        Ok(entities)
    }

    /// List keys in a specific database with pagination using SCAN
    pub async fn list_keys_in_db_paginated(
        &self,
        db: i32,
        pattern: Option<&str>,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<(Vec<EntityInfo>, Option<String>)> {
        let mut conn = self.get_db_connection(db).await?;
        let pattern = pattern.unwrap_or("*");
        let start_cursor: u64 = cursor.as_ref().and_then(|c| c.parse().ok()).unwrap_or(0);

        debug!(
            "Listing keys in database {} with pattern: {}, cursor: {}, limit: {}",
            db, pattern, start_cursor, limit
        );

        let mut entities = Vec::with_capacity(limit);
        let mut current_cursor = start_cursor;

        // Use SCAN for efficient iteration
        loop {
            let result: (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(current_cursor)
                .arg("MATCH")
                .arg(pattern)
                .arg("COUNT")
                .arg(limit.min(100)) // Batch size
                .query_async(&mut conn)
                .await
                .map_err(|e: RedisError| {
                    error!("Failed to scan keys in database {}: {}", db, e);
                    DataError::QueryFailed(format!("Failed to scan keys: {}", e))
                })?;

            let (next_cursor, keys) = result;

            for key in keys {
                if entities.len() >= limit {
                    break;
                }
                entities.push(EntityInfo {
                    namespace: db.to_string(),
                    name: key,
                    entity_type: "key".to_string(),
                    row_count: Some(1),
                    size_bytes: None,
                    schema: None,
                    metadata: None,
                });
            }

            current_cursor = next_cursor;

            // Stop if we've collected enough keys or completed a full scan
            if entities.len() >= limit || current_cursor == 0 {
                break;
            }
        }

        debug!("Found {} keys in database {}", entities.len(), db);

        // Return next cursor if there are more keys
        let next_token = if current_cursor != 0 {
            Some(current_cursor.to_string())
        } else {
            None
        };

        Ok((entities, next_token))
    }

    /// List entities with pagination support - public method for query service
    pub async fn list_entities_paginated(
        &self,
        container_path: &ContainerPath,
        limit: usize,
        continuation_token: Option<String>,
    ) -> Result<(Vec<EntityInfo>, Option<String>)> {
        if container_path.depth() == 0 {
            return Err(DataError::InvalidQuery(
                "Cannot list entities at root level - specify a database path (0-15)".to_string(),
            ));
        }

        let db_str = &container_path.segments[0];
        let db_num: i32 = db_str.parse().map_err(|_| {
            DataError::InvalidQuery(format!(
                "Invalid database number '{}'. Must be 0-15",
                db_str
            ))
        })?;

        if !(0..=15).contains(&db_num) {
            return Err(DataError::InvalidQuery(format!(
                "Database number {} out of range. Must be 0-15",
                db_num
            )));
        }

        // If depth > 1, use remaining segments as key pattern
        let pattern = if container_path.depth() > 1 {
            Some(format!("{}*", container_path.segments[1..].join(":")))
        } else {
            None
        };

        self.list_keys_in_db_paginated(db_num, pattern.as_deref(), limit, continuation_token)
            .await
    }

    /// Get the value of a specific key
    async fn get_key_value(
        &self,
        db: i32,
        key: &str,
        options: &QueryOptions,
    ) -> Result<(DataRow, bool, Option<String>)> {
        let mut conn = self.get_db_connection(db).await?;

        debug!("Getting value for key '{}' in database {}", key, db);

        // Check if key exists
        let exists: bool = conn.exists(key).await.map_err(|e: RedisError| {
            error!("Failed to check if key exists: {}", e);
            DataError::QueryFailed(format!("Failed to check key existence: {}", e))
        })?;

        if !exists {
            return Err(DataError::NotFound(format!(
                "Key '{}' not found in database {}",
                key, db
            )));
        }

        // Get key type
        let key_type: String = redis::cmd("TYPE")
            .arg(key)
            .query_async(&mut conn)
            .await
            .map_err(|e: RedisError| {
                error!("Failed to get key type: {}", e);
                DataError::QueryFailed(format!("Failed to get key type: {}", e))
            })?;

        // Get TTL
        let ttl: i64 = conn.ttl(key).await.map_err(|e: RedisError| {
            error!("Failed to get key TTL: {}", e);
            DataError::QueryFailed(format!("Failed to get key TTL: {}", e))
        })?;

        // A string is one response cell, so reject it by logical length before
        // transfer. Collection allocation is deliberately not compared with a
        // per-cell limit: large collections remain safely browsable in pages.
        if key_type == "string" {
            let observed: usize = redis::cmd("STRLEN")
                .arg(key)
                .query_async(&mut conn)
                .await
                .map_err(|e: RedisError| {
                    error!(key_type, error = %e, "failed to inspect Redis string size");
                    DataError::BackendQueryFailed {
                        backend: "Redis",
                        entity: key.to_string(),
                    }
                })?;
            if observed > options.budget.max_cell_bytes {
                return Err(DataError::ResultLimitExceeded {
                    entity: key.to_string(),
                    limit_kind: "cell_bytes",
                    limit: options.budget.max_cell_bytes,
                    observed,
                });
            }
        }

        // Redis SCAN cursors cannot represent an exact item boundary because
        // COUNT is only a hint. Expose a stable logical offset cursor and replay
        // SCAN to that boundary, which avoids dropping the remainder of a batch.
        let offset = match options.cursor.as_deref() {
            Some(cursor) => cursor.parse::<usize>().map_err(|_| {
                DataError::InvalidQuery(format!(
                    "Invalid Redis value cursor '{cursor}': expected a non-negative offset"
                ))
            })?,
            None => options.offset.unwrap_or(0),
        };
        if offset > MAX_REDIS_VALUE_OFFSET {
            return Err(DataError::InvalidQuery(format!(
                "Redis value offset {offset} exceeds maximum {MAX_REDIS_VALUE_OFFSET}"
            )));
        }
        let aggregate_limit = redis_aggregate_limit(options);
        let mut truncated = false;

        // Get value based on type
        let value = match key_type.as_str() {
            "string" => {
                let max_string_bytes = options.budget.max_cell_bytes.saturating_sub(2).max(1);
                let end = i64::try_from(max_string_bytes.saturating_sub(1)).unwrap_or(i64::MAX);
                let v: Vec<u8> = redis::cmd("GETRANGE")
                    .arg(key)
                    .arg(0)
                    .arg(end)
                    .query_async(&mut conn)
                    .await
                    .map_err(|e: RedisError| {
                        error!("Failed to get string value: {}", e);
                        DataError::BackendQueryFailed {
                            backend: "Redis",
                            entity: key.to_string(),
                        }
                    })?;
                serde_json::Value::String(String::from_utf8_lossy(&v).into_owned())
            }
            "list" => {
                let (v, has_more) = Self::list_page(
                    &mut conn,
                    key,
                    offset,
                    aggregate_limit,
                    options.budget.max_cell_bytes,
                )
                .await?;
                truncated = has_more;
                serde_json::json!(v)
            }
            "set" => {
                let (v, has_more) = Self::scan_set_page(
                    &mut conn,
                    key,
                    offset,
                    aggregate_limit,
                    options.budget.max_cell_bytes,
                )
                .await?;
                truncated = has_more;
                serde_json::json!(v)
            }
            "zset" => {
                let (v, has_more) = Self::zset_page(
                    &mut conn,
                    key,
                    offset,
                    aggregate_limit,
                    options.budget.max_cell_bytes,
                )
                .await?;
                truncated = has_more;
                serde_json::json!(v
                    .into_iter()
                    .map(|(member, score)| {
                        serde_json::json!({"member": member, "score": score})
                    })
                    .collect::<Vec<_>>())
            }
            "hash" => {
                let (values, has_more) = Self::scan_hash_page(
                    &mut conn,
                    key,
                    offset,
                    aggregate_limit,
                    options.budget.max_cell_bytes,
                )
                .await?;
                truncated = has_more;
                serde_json::json!(values.into_iter().collect::<HashMap<_, _>>())
            }
            "stream" => {
                // For streams, just indicate it's a stream - full stream reading is complex
                serde_json::Value::String("[Stream data - use XREAD/XRANGE commands]".to_string())
            }
            _ => serde_json::Value::String(format!("[Unknown type: {}]", key_type)),
        };

        let mut row = DataRow::new();
        row.insert(
            "key".to_string(),
            serde_json::Value::String(key.to_string()),
        );
        row.insert("type".to_string(), serde_json::Value::String(key_type));
        row.insert("ttl".to_string(), serde_json::Value::Number(ttl.into()));
        row.insert("value".to_string(), value);

        let next_cursor = truncated.then(|| offset.saturating_add(aggregate_limit).to_string());
        Ok((row, truncated, next_cursor))
    }

    /// Get information about a specific key
    async fn get_key_info(&self, db: i32, key: &str) -> Result<EntityInfo> {
        let mut conn = self.get_db_connection(db).await?;

        debug!("Getting info for key '{}' in database {}", key, db);

        // Check if key exists
        let exists: bool = conn.exists(key).await.map_err(|e: RedisError| {
            error!("Failed to check if key exists: {}", e);
            DataError::QueryFailed(format!("Failed to check key existence: {}", e))
        })?;

        if !exists {
            return Err(DataError::NotFound(format!(
                "Key '{}' not found in database {}",
                key, db
            )));
        }

        // Get key type
        let key_type: String = redis::cmd("TYPE")
            .arg(key)
            .query_async(&mut conn)
            .await
            .map_err(|e: RedisError| {
                error!("Failed to get key type: {}", e);
                DataError::QueryFailed(format!("Failed to get key type: {}", e))
            })?;

        // Get TTL (time to live)
        let ttl: i64 = conn.ttl(key).await.map_err(|e: RedisError| {
            error!("Failed to get key TTL: {}", e);
            DataError::QueryFailed(format!("Failed to get key TTL: {}", e))
        })?;

        // Build schema based on key type
        let schema = DatasetSchema {
            fields: vec![
                FieldDef {
                    name: "key".to_string(),
                    field_type: FieldType::String,
                    nullable: false,
                    description: Some("Redis key name".to_string()),
                },
                FieldDef {
                    name: "type".to_string(),
                    field_type: FieldType::String,
                    nullable: false,
                    description: Some("Redis data type".to_string()),
                },
                FieldDef {
                    name: "ttl".to_string(),
                    field_type: FieldType::Int64,
                    nullable: true,
                    description: Some("Time to live in seconds (-1 = no expiry)".to_string()),
                },
                FieldDef {
                    name: "value".to_string(),
                    field_type: FieldType::String,
                    nullable: true,
                    description: Some("Key value (for simple types)".to_string()),
                },
            ],
            partitions: None,
            primary_key: Some(vec!["key".to_string()]),
        };

        // Build metadata with TTL information
        let mut metadata_map = HashMap::new();
        metadata_map.insert("ttl".to_string(), serde_json::json!(ttl));
        metadata_map.insert("has_expiry".to_string(), serde_json::json!(ttl >= 0));

        Ok(EntityInfo {
            namespace: db.to_string(),
            name: key.to_string(),
            entity_type: key_type,
            row_count: Some(1),
            size_bytes: None,
            schema: Some(schema),
            metadata: Some(serde_json::Value::Object(
                metadata_map.into_iter().collect(),
            )),
        })
    }
}

#[async_trait]
impl DataSource for RedisSource {
    fn source_type(&self) -> &'static str {
        "redis"
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::KeyValue]
    }

    async fn list_containers(&self, path: &ContainerPath) -> Result<Vec<ContainerInfo>> {
        match path.depth() {
            // Depth 0: List databases (0-15 by default)
            0 => {
                debug!("Listing Redis databases");

                // Redis typically supports 16 databases (0-15)
                let databases: Vec<ContainerInfo> = (0..16)
                    .map(|db_num| {
                        let mut metadata = HashMap::new();
                        metadata.insert("database_number".to_string(), serde_json::json!(db_num));

                        ContainerInfo {
                            name: db_num.to_string(),
                            container_type: ContainerType::Database,
                            capabilities: ContainerCapabilities {
                                can_contain_containers: false,
                                can_contain_entities: true,
                                child_container_type: None,
                                entity_type_label: Some("key".to_string()),
                                entity_count_hint: Some(EntityCountHint::Large),
                            },
                            metadata,
                        }
                    })
                    .collect();

                debug!("Found {} databases", databases.len());
                Ok(databases)
            }

            // Depth >= 1: Not supported (Redis is flat - databases contain keys directly)
            _ => Err(DataError::InvalidQuery(format!(
                "Redis hierarchy only supports 1 level (database). Path depth: {}. Use list_entities to list keys.",
                path.depth()
            ))),
        }
    }

    async fn get_container_info(&self, path: &ContainerPath) -> Result<ContainerInfo> {
        if path.depth() != 1 {
            return Err(DataError::InvalidQuery(format!(
                "get_container_info requires path depth 1 (database number), got {}",
                path.depth()
            )));
        }

        let db_str = &path.segments[0];
        let db_num: i32 = db_str.parse().map_err(|_| {
            DataError::InvalidQuery(format!(
                "Invalid database number '{}'. Must be 0-15",
                db_str
            ))
        })?;

        if !(0..=15).contains(&db_num) {
            return Err(DataError::InvalidQuery(format!(
                "Database number {} out of range. Must be 0-15",
                db_num
            )));
        }

        debug!("Getting info for database: {}", db_num);

        let mut metadata = HashMap::new();
        metadata.insert("database_number".to_string(), serde_json::json!(db_num));

        // Try to get database size (number of keys)
        match self.get_db_connection(db_num).await {
            Ok(mut conn) => {
                if let Ok(size) = redis::cmd("DBSIZE").query_async::<usize>(&mut conn).await {
                    metadata.insert("key_count".to_string(), serde_json::json!(size));
                }
            }
            Err(_) => {
                // Connection failed, but we can still return basic info
            }
        }

        Ok(ContainerInfo {
            name: db_num.to_string(),
            container_type: ContainerType::Database,
            capabilities: ContainerCapabilities {
                can_contain_containers: false,
                can_contain_entities: true,
                child_container_type: None,
                entity_type_label: Some("key".to_string()),
                entity_count_hint: Some(EntityCountHint::Large),
            },
            metadata,
        })
    }

    async fn list_entities(&self, container_path: &ContainerPath) -> Result<Vec<EntityInfo>> {
        if container_path.depth() == 0 {
            return Err(DataError::InvalidQuery(
                "Cannot list entities at root level - specify a database path (0-15)".to_string(),
            ));
        }

        let db_str = &container_path.segments[0];
        let db_num: i32 = db_str.parse().map_err(|_| {
            DataError::InvalidQuery(format!(
                "Invalid database number '{}'. Must be 0-15",
                db_str
            ))
        })?;

        if !(0..=15).contains(&db_num) {
            return Err(DataError::InvalidQuery(format!(
                "Database number {} out of range. Must be 0-15",
                db_num
            )));
        }

        // If depth > 1, use remaining segments as key pattern
        let pattern = if container_path.depth() > 1 {
            Some(container_path.segments[1..].join(":"))
        } else {
            None
        };

        self.list_keys_in_db(db_num, pattern.as_deref()).await
    }

    async fn get_entity_info(
        &self,
        container_path: &ContainerPath,
        entity_name: &str,
    ) -> Result<EntityInfo> {
        if container_path.depth() == 0 {
            return Err(DataError::InvalidQuery(
                "Cannot get entity at root level - specify a database path (0-15)".to_string(),
            ));
        }

        let db_str = &container_path.segments[0];
        let db_num: i32 = db_str.parse().map_err(|_| {
            DataError::InvalidQuery(format!(
                "Invalid database number '{}'. Must be 0-15",
                db_str
            ))
        })?;

        if !(0..=15).contains(&db_num) {
            return Err(DataError::InvalidQuery(format!(
                "Database number {} out of range. Must be 0-15",
                db_num
            )));
        }

        self.get_key_info(db_num, entity_name).await
    }

    async fn get_schema(
        &self,
        container_path: &ContainerPath,
        entity_name: &str,
    ) -> Result<DatasetSchema> {
        let entity_info = self.get_entity_info(container_path, entity_name).await?;

        entity_info.schema.ok_or_else(|| {
            DataError::OperationNotSupported("Schema not available for this entity".to_string())
        })
    }

    async fn close(&self) -> Result<()> {
        debug!("Closing Redis source");
        // Connection manager handles cleanup automatically
        Ok(())
    }
}

#[async_trait]
impl Queryable for RedisSource {
    async fn query(
        &self,
        container_path: &ContainerPath,
        entity_name: &str,
        _filters: Option<serde_json::Value>,
        options: QueryOptions,
    ) -> Result<QueryResult> {
        let start = std::time::Instant::now();

        if container_path.depth() == 0 {
            return Err(DataError::InvalidQuery(
                "Cannot query at root level - specify a database path (0-15)".to_string(),
            ));
        }

        let db_str = &container_path.segments[0];
        let db_num: i32 = db_str.parse().map_err(|_| {
            DataError::InvalidQuery(format!(
                "Invalid database number '{}'. Must be 0-15",
                db_str
            ))
        })?;

        if !(0..=15).contains(&db_num) {
            return Err(DataError::InvalidQuery(format!(
                "Database number {} out of range. Must be 0-15",
                db_num
            )));
        }

        // Get the key value
        let (row, value_truncated, next_cursor) =
            self.get_key_value(db_num, entity_name, &options).await?;
        let execution_ms = start.elapsed().as_millis() as u64;

        // Define schema for the result
        let schema = DatasetSchema {
            fields: vec![
                FieldDef {
                    name: "key".to_string(),
                    field_type: FieldType::String,
                    nullable: false,
                    description: Some("Redis key name".to_string()),
                },
                FieldDef {
                    name: "type".to_string(),
                    field_type: FieldType::String,
                    nullable: false,
                    description: Some("Redis data type".to_string()),
                },
                FieldDef {
                    name: "ttl".to_string(),
                    field_type: FieldType::Int64,
                    nullable: true,
                    description: Some("Time to live in seconds (-1 = no expiry)".to_string()),
                },
                FieldDef {
                    name: "value".to_string(),
                    field_type: FieldType::Json,
                    nullable: true,
                    description: Some("Key value".to_string()),
                },
            ],
            partitions: None,
            primary_key: Some(vec!["key".to_string()]),
        };

        let mut bounded = BoundedRows::new(options.budget);
        bounded.push(entity_name, row)?;
        let (rows, budget_truncated) = bounded.into_parts();
        Ok(QueryResult {
            schema,
            stats: QueryStats {
                row_count: rows.len(),
                total_rows: Some(1),
                execution_ms,
                has_more: value_truncated,
                next_cursor,
                truncated: value_truncated || budget_truncated,
            },
            rows,
        })
    }

    async fn count(
        &self,
        container_path: &ContainerPath,
        entity_name: &str,
        _filters: Option<serde_json::Value>,
    ) -> Result<u64> {
        if container_path.depth() == 0 {
            return Err(DataError::InvalidQuery(
                "Cannot count at root level - specify a database path (0-15)".to_string(),
            ));
        }

        let db_str = &container_path.segments[0];
        let db_num: i32 = db_str.parse().map_err(|_| {
            DataError::InvalidQuery(format!(
                "Invalid database number '{}'. Must be 0-15",
                db_str
            ))
        })?;

        if !(0..=15).contains(&db_num) {
            return Err(DataError::InvalidQuery(format!(
                "Database number {} out of range. Must be 0-15",
                db_num
            )));
        }

        let mut conn = self.get_db_connection(db_num).await?;

        // Check if key exists
        let exists: bool = conn.exists(entity_name).await.map_err(|e: RedisError| {
            error!("Failed to check if key exists: {}", e);
            DataError::QueryFailed(format!("Failed to check key existence: {}", e))
        })?;

        Ok(if exists { 1 } else { 0 })
    }

    async fn entity_exists(
        &self,
        container_path: &ContainerPath,
        entity_name: &str,
    ) -> Result<bool> {
        if container_path.depth() == 0 {
            return Err(DataError::InvalidQuery(
                "Cannot check entity at root level - specify a database path (0-15)".to_string(),
            ));
        }

        let db_str = &container_path.segments[0];
        let db_num: i32 = db_str.parse().map_err(|_| {
            DataError::InvalidQuery(format!(
                "Invalid database number '{}'. Must be 0-15",
                db_str
            ))
        })?;

        if !(0..=15).contains(&db_num) {
            return Err(DataError::InvalidQuery(format!(
                "Database number {} out of range. Must be 0-15",
                db_num
            )));
        }

        let mut conn = self.get_db_connection(db_num).await?;

        let exists: bool = conn.exists(entity_name).await.map_err(|e: RedisError| {
            error!("Failed to check if key exists: {}", e);
            DataError::QueryFailed(format!("Failed to check key existence: {}", e))
        })?;

        Ok(exists)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use temps_query::QueryBudget;
    use testcontainers::{core::WaitFor, runners::AsyncRunner, GenericImage};

    #[test]
    fn test_source_type() {
        assert_eq!("redis", "redis");
    }

    #[test]
    fn test_capabilities() {
        let capabilities = [Capability::KeyValue];
        assert!(capabilities.contains(&Capability::KeyValue));
    }

    #[test]
    fn test_database_range() {
        // Redis databases are 0-15
        assert!((0..=15).contains(&0));
        assert!((0..=15).contains(&15));
        assert!(!(0..=15).contains(&16));
    }

    #[test]
    fn redis_aggregate_pages_are_capped_by_structural_budget() {
        let options = QueryOptions {
            limit: Some(10_000),
            budget: QueryBudget {
                max_value_elements_per_row: 40,
                ..QueryBudget::default()
            },
            ..QueryOptions::default()
        };
        assert_eq!(redis_aggregate_limit(&options), 10);
    }

    #[tokio::test]
    async fn large_set_remains_pageable_by_offset() -> anyhow::Result<()> {
        let container = match GenericImage::new("redis", "7-alpine")
            .with_exposed_port(6379_u16.into())
            .with_wait_for(WaitFor::message_on_stdout("Ready to accept connections"))
            .start()
            .await
        {
            Ok(container) => container,
            Err(error) => {
                eprintln!("Skipping Docker-dependent Redis paging test: {error}");
                return Ok(());
            }
        };
        let host = container.get_host().await?;
        let port = container.get_host_port_ipv4(6379).await?;
        let source = RedisSource::new(&format!("redis://{host}:{port}")).await?;
        let mut connection = source.get_db_connection(0).await?;

        for chunk_start in (0..4_000).step_by(200) {
            let mut pipeline = redis::pipe();
            for index in chunk_start..chunk_start + 200 {
                pipeline
                    .cmd("SADD")
                    .arg("large-set")
                    .arg(format!("member-{index:04}-{}", "x".repeat(300)))
                    .ignore();
            }
            pipeline.query_async::<()>(&mut connection).await?;
        }
        let allocated: usize = redis::cmd("MEMORY")
            .arg("USAGE")
            .arg("large-set")
            .query_async(&mut connection)
            .await?;
        assert!(allocated > QueryBudget::default().max_cell_bytes);

        let path = ContainerPath::from_slice(&["0"]);
        let page = |offset| QueryOptions {
            limit: Some(2),
            offset: Some(offset),
            ..QueryOptions::default()
        };
        let first = source.query(&path, "large-set", None, page(0)).await?;
        let second = source.query(&path, "large-set", None, page(2)).await?;
        let cursor_page = source
            .query(
                &path,
                "large-set",
                None,
                QueryOptions {
                    limit: Some(2),
                    cursor: first.stats.next_cursor.clone(),
                    ..QueryOptions::default()
                },
            )
            .await?;

        let members = |result: &QueryResult| -> HashSet<String> {
            result.rows[0]["value"]
                .as_array()
                .expect("set value is an array")
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect()
        };
        let first_members = members(&first);
        let second_members = members(&second);
        let cursor_members = members(&cursor_page);
        assert_eq!(first_members.len(), 2);
        assert_eq!(second_members.len(), 2);
        assert!(first_members.is_disjoint(&second_members));
        assert_eq!(second_members, cursor_members);
        assert!(first.stats.has_more);
        assert_eq!(first.stats.next_cursor.as_deref(), Some("2"));
        assert!(first.stats.truncated);
        assert!(second.stats.truncated);

        let offset_error = source
            .query(
                &path,
                "large-set",
                None,
                QueryOptions {
                    cursor: Some((MAX_REDIS_VALUE_OFFSET + 1).to_string()),
                    ..QueryOptions::default()
                },
            )
            .await
            .expect_err("Redis aggregate script work must be offset-bounded");
        assert!(matches!(offset_error, DataError::InvalidQuery(_)));

        let oversized = "y".repeat(100_000);
        redis::cmd("RPUSH")
            .arg("oversized-list")
            .arg(&oversized)
            .query_async::<()>(&mut connection)
            .await?;
        redis::cmd("SADD")
            .arg("oversized-set")
            .arg(&oversized)
            .query_async::<()>(&mut connection)
            .await?;
        redis::cmd("ZADD")
            .arg("oversized-zset")
            .arg(1)
            .arg(&oversized)
            .query_async::<()>(&mut connection)
            .await?;
        redis::cmd("HSET")
            .arg("oversized-hash")
            .arg("field")
            .arg(&oversized)
            .query_async::<()>(&mut connection)
            .await?;

        for key in [
            "oversized-list",
            "oversized-set",
            "oversized-zset",
            "oversized-hash",
        ] {
            let error = source
                .query(
                    &path,
                    key,
                    None,
                    QueryOptions {
                        limit: Some(2),
                        budget: QueryBudget {
                            max_cell_bytes: 8 * 1024,
                            ..QueryBudget::default()
                        },
                        ..QueryOptions::default()
                    },
                )
                .await
                .expect_err("oversized Redis aggregate element must be rejected in Lua");
            assert!(matches!(
                error,
                DataError::ResultLimitExceeded {
                    limit_kind: "cell_bytes",
                    ..
                }
            ));
        }

        Ok(())
    }
}
