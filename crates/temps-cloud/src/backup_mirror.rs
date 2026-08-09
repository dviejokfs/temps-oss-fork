use std::{sync::Arc, time::Duration};

use aws_sdk_s3::{Client as S3Client, Config};
use sea_orm::{
    sea_query::Expr, ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection,
    EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set, Statement,
};
use sha2::{Digest, Sha256};
use temps_cloud_client::CloudLink;
use temps_cloud_protocol::{
    BackupCompression, BackupEngine, BackupFormat, NativeSnapshotIdentity,
    NativeSnapshotObjectDeclaration, NativeSnapshotObjectKind, NativeSnapshotRequest,
    WalGObjectCompleted, WalGObjectDeclaration, WalGObjectKind, WalGObjectTargetRequest,
    WalGSnapshotCompleted, WalGSnapshotRequest,
};
use temps_core::EncryptionService;
use temps_entities::{backups, external_service_backups, external_services, s3_sources};
use tokio::{io::AsyncReadExt, sync::watch};
use tokio_util::io::ReaderStream;
use tracing::{info, warn};
use uuid::Uuid;

/// Healthy cadence for discovering completed local backups.
const BASE_SWEEP_INTERVAL: Duration = Duration::from_secs(30);
/// Outage ceiling. Cloud can be unavailable indefinitely without making the
/// self-hosted instance hammer it or affecting local backup completion.
const MAX_SWEEP_INTERVAL: Duration = Duration::from_secs(15 * 60);
const SWEEP_LIMIT: u64 = 50;

enum StageError {
    Unsupported(String),
    Retry(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SweepOutcome {
    NotLinked,
    Idle,
    Progress,
    Retry,
}

fn next_sweep_interval(current: Duration, outcome: SweepOutcome) -> Duration {
    match outcome {
        SweepOutcome::NotLinked => MAX_SWEEP_INTERVAL,
        SweepOutcome::Idle | SweepOutcome::Progress => BASE_SWEEP_INTERVAL,
        SweepOutcome::Retry if current.is_zero() => BASE_SWEEP_INTERVAL,
        SweepOutcome::Retry => (current * 2).min(MAX_SWEEP_INTERVAL),
    }
}

pub async fn run(
    link: Arc<CloudLink>,
    db: Arc<DatabaseConnection>,
    encryption: Arc<EncryptionService>,
    mut cancel: watch::Receiver<bool>,
) {
    info!("Cloud backup mirror started");
    // The first discovery pass is immediate. Subsequent failures back off,
    // while any successful progress resets the healthy cadence.
    let mut retry_in = Duration::ZERO;
    loop {
        tokio::select! {
            changed = cancel.changed() => {
                if changed.is_err() {
                    warn!("Cloud backup mirror stopped because its owner was dropped");
                    return;
                }
                if *cancel.borrow() {
                    info!("Cloud backup mirror stopped after shutdown request");
                    return;
                }
            }
            _ = tokio::time::sleep(retry_in) => {
                let outcome = match sweep(&link, &db, &encryption).await {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        warn!(error = %error, "Cloud backup mirror sweep failed; local backups remain authoritative");
                        SweepOutcome::Retry
                    }
                };
                retry_in = next_sweep_interval(retry_in, outcome);
                if outcome == SweepOutcome::Retry {
                    warn!(
                        retry_in_secs = retry_in.as_secs(),
                        "Cloud backup mirror retained local backup; retrying with exponential backoff"
                    );
                }
            }
        }
    }
}

async fn sweep(
    link: &Arc<CloudLink>,
    db: &Arc<DatabaseConnection>,
    encryption: &Arc<EncryptionService>,
) -> Result<SweepOutcome, sea_orm::DbErr> {
    if !link.is_linked() {
        return Ok(SweepOutcome::NotLinked);
    }
    let (Some(tenant_id), Some(instance_id)) = (link.tenant_id(), link.instance_id()) else {
        return Ok(SweepOutcome::NotLinked);
    };
    let candidates = backups::Entity::find()
        .filter(backups::Column::State.eq("completed"))
        .filter(Expr::col(backups::Column::Metadata).not_like(format!("%\"{tenant_id}\"%")))
        .order_by_asc(backups::Column::FinishedAt)
        .limit(SWEEP_LIMIT)
        .all(db.as_ref())
        .await?;

    tracing::debug!(
        candidate_count = candidates.len(),
        %tenant_id,
        %instance_id,
        "Cloud backup mirror sweep selected local backups"
    );

    if candidates.is_empty() {
        return Ok(SweepOutcome::Idle);
    }

    let mut made_progress = false;
    let mut retry_required = false;

    for backup in candidates {
        info!(
            local_backup_id = %backup.backup_id,
            "Cloud backup mirror staging local backup"
        );
        match mirror_backup(link, db, encryption, &backup, instance_id).await {
            Ok(()) => {
                info!(local_backup_id = %backup.backup_id, "WAL-G repository mirrored to Cloud");
                persist_state(db, backup, tenant_id, "complete", None).await?;
                made_progress = true;
            }
            Err(StageError::Unsupported(reason)) => {
                persist_state(db, backup, tenant_id, "unsupported", Some(&reason)).await?;
                made_progress = true;
            }
            Err(StageError::Retry(error)) => {
                warn!(backup_id = %backup.backup_id, error = %error, "Cloud backup mirror unavailable; will retry without affecting the local backup");
                retry_required = true;
            }
        }
    }
    Ok(if retry_required {
        SweepOutcome::Retry
    } else if made_progress {
        SweepOutcome::Progress
    } else {
        SweepOutcome::Idle
    })
}

async fn mirror_backup(
    link: &CloudLink,
    db: &DatabaseConnection,
    encryption: &EncryptionService,
    backup: &backups::Model,
    instance_id: Uuid,
) -> Result<(), StageError> {
    let external = external_service_backups::Entity::find()
        .filter(external_service_backups::Column::BackupId.eq(backup.id))
        .one(db)
        .await
        .map_err(|error| StageError::Retry(error.to_string()))?;
    let Some(external) = external else {
        return mirror_walg_backup(link, db, encryption, backup, instance_id).await;
    };
    let service = external_services::Entity::find_by_id(external.service_id)
        .one(db)
        .await
        .map_err(|error| StageError::Retry(error.to_string()))?
        .ok_or_else(|| StageError::Retry(format!("service {} is missing", external.service_id)))?;
    match service.service_type.to_ascii_lowercase().as_str() {
        "postgres" | "postgresql" | "timescale" | "timescaledb" => {
            mirror_walg_backup(link, db, encryption, backup, instance_id).await
        }
        engine if supports_native_mirror(engine) => {
            mirror_native_backup(
                link,
                db,
                encryption,
                backup,
                &external,
                &service,
                instance_id,
            )
            .await
        }
        engine => Err(StageError::Unsupported(format!(
            "Cloud backup mirroring does not support engine {engine}"
        ))),
    }
}

fn supports_native_mirror(service_type: &str) -> bool {
    matches!(
        service_type,
        "mongodb" | "mongo" | "redis" | "mariadb" | "rustfs" | "s3" | "minio" | "blob"
    )
}

async fn mirror_native_backup(
    link: &CloudLink,
    db: &DatabaseConnection,
    encryption: &EncryptionService,
    backup: &backups::Model,
    external: &external_service_backups::Model,
    service: &external_services::Model,
    instance_id: Uuid,
) -> Result<(), StageError> {
    let source_config = s3_sources::Entity::find_by_id(backup.s3_source_id)
        .one(db)
        .await
        .map_err(|error| StageError::Retry(error.to_string()))?
        .ok_or_else(|| {
            StageError::Retry(format!("S3 source {} is missing", backup.s3_source_id))
        })?;
    let client = s3_client(encryption, &source_config)?;
    let location = if external.s3_location.trim().is_empty() {
        backup.s3_location.as_str()
    } else {
        external.s3_location.as_str()
    };
    let location_key = s3_key(&source_config.bucket_name, location)?;
    let service_type = service.service_type.to_ascii_lowercase();
    let version = service_engine_version(encryption, service);

    let (root, selected, engine, format, compression, identity) = match service_type.as_str() {
        "mongodb" | "mongo" | "redis" => {
            let root = location_key.trim_end_matches('/').to_string();
            if !root.ends_with("/walg") {
                return Err(StageError::Unsupported(format!(
                    "{service_type} Cloud backups require the WAL-G stream path; got {location}"
                )));
            }
            let all = list_repository_objects(&client, &source_config.bucket_name, &root).await?;
            let (sentinel_key, _) = find_snapshot_sentinel(
                &client,
                &source_config.bucket_name,
                &all,
                &backup.backup_id,
            )
            .await?;
            let backup_name = sentinel_key
                .rsplit('/')
                .next()
                .and_then(|name| name.strip_suffix("_backup_stop_sentinel.json"))
                .ok_or_else(|| {
                    StageError::Retry(format!("invalid WAL-G stream sentinel {sentinel_key}"))
                })?
                .to_string();
            let selected = all
                .into_iter()
                .filter(|object| object.key == sentinel_key || object.key.contains(&backup_name))
                .collect::<Vec<_>>();
            if selected.len() < 2 {
                return Err(StageError::Retry(format!(
                    "WAL-G stream snapshot {backup_name} is incomplete"
                )));
            }
            if service_type == "redis" {
                (
                    root,
                    selected,
                    BackupEngine::Redis,
                    BackupFormat::RedisRdb,
                    BackupCompression::WalGNative,
                    NativeSnapshotIdentity::RedisRdbStream {
                        engine_version: version,
                        backup_name,
                    },
                )
            } else {
                (
                    root,
                    selected,
                    BackupEngine::MongoDb,
                    BackupFormat::MongoDumpArchive,
                    BackupCompression::WalGNative,
                    NativeSnapshotIdentity::MongoDbStream {
                        engine_version: version,
                        backup_name,
                    },
                )
            }
        }
        "mariadb" => {
            let root = location_key.trim_end_matches('/').to_string();
            if !root.ends_with("/walg") {
                return Err(StageError::Unsupported(format!(
                    "MariaDB Cloud backups require the WAL-G repository path; got {location}"
                )));
            }
            let all = list_repository_objects(&client, &source_config.bucket_name, &root).await?;
            let (sentinel_key, _) = find_snapshot_sentinel(
                &client,
                &source_config.bucket_name,
                &all,
                &backup.backup_id,
            )
            .await?;
            let backup_name = sentinel_key
                .rsplit('/')
                .next()
                .and_then(|name| name.strip_suffix("_backup_stop_sentinel.json"))
                .ok_or_else(|| {
                    StageError::Retry(format!("invalid MariaDB WAL-G sentinel {sentinel_key}"))
                })?
                .to_string();
            let metadata_key = format!("{root}/{}.metadata.json", backup.backup_id);
            let selected = all
                .into_iter()
                .filter(|object| {
                    object.key == sentinel_key
                        || object.key == metadata_key
                        || object.key.contains(&backup_name)
                })
                .collect::<Vec<_>>();
            if selected.len() < 3 || !selected.iter().any(|object| object.key == metadata_key) {
                return Err(StageError::Retry(format!(
                    "MariaDB WAL-G snapshot {backup_name} is incomplete or lacks {metadata_key}"
                )));
            }
            let metadata =
                read_json_object(&client, &source_config.bucket_name, &metadata_key).await?;
            let binlog_file = metadata
                .pointer("/extra/binlog_file")
                .or_else(|| metadata.get("binlog_file"))
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
            let binlog_position = metadata
                .pointer("/extra/binlog_position")
                .or_else(|| metadata.get("binlog_position"))
                .and_then(|value| {
                    value
                        .as_u64()
                        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
                });
            (
                root,
                selected,
                BackupEngine::MariaDb,
                BackupFormat::WalGRepository,
                BackupCompression::WalGNative,
                NativeSnapshotIdentity::MariaDbPhysical {
                    engine_version: version,
                    backup_name,
                    binlog_file,
                    binlog_position,
                },
            )
        }
        "rustfs" | "s3" | "minio" | "blob" => {
            let root = location_key.trim_end_matches('/').to_string();
            let selected = list_repository_objects(&client, &source_config.bucket_name, &root)
                .await?
                .into_iter()
                .filter(|object| !object.key.ends_with("/metadata.json"))
                .collect::<Vec<_>>();
            if selected.is_empty() {
                return Err(StageError::Retry(format!(
                    "RustFS snapshot {root} has no objects"
                )));
            }
            (
                root,
                selected,
                BackupEngine::RustFs,
                BackupFormat::ObjectSet,
                BackupCompression::None,
                NativeSnapshotIdentity::ObjectSet {
                    snapshot_name: backup.backup_id.clone(),
                },
            )
        }
        engine => {
            return Err(StageError::Unsupported(format!(
                "native mirror does not support {engine}"
            )))
        }
    };

    let mut declarations = Vec::with_capacity(selected.len());
    for object in &selected {
        let (bytes, checksum_sha256) =
            inspect_source_object(&client, &source_config.bucket_name, &object.key).await?;
        if bytes != object.bytes {
            return Err(StageError::Retry(format!(
                "native snapshot object {} changed while its manifest was built",
                object.key
            )));
        }
        let relative_key = object
            .key
            .strip_prefix(&format!("{root}/"))
            .unwrap_or_else(|| object.key.rsplit('/').next().unwrap_or(&object.key))
            .to_string();
        let kind = if object.key.ends_with("_backup_stop_sentinel.json")
            || object.key.ends_with("metadata.json")
        {
            NativeSnapshotObjectKind::Metadata
        } else if engine == BackupEngine::MariaDb {
            NativeSnapshotObjectKind::BaseBackup
        } else if engine == BackupEngine::RustFs {
            NativeSnapshotObjectKind::Object
        } else {
            NativeSnapshotObjectKind::Data
        };
        declarations.push(NativeSnapshotObjectDeclaration {
            relative_key,
            kind,
            bytes,
            checksum_sha256,
        });
    }
    let cloud_backup_id = Uuid::new_v5(
        &link
            .tenant_id()
            .ok_or_else(|| StageError::Retry("Cloud link lost its tenant identity".into()))?,
        format!("{instance_id}:{}", backup.backup_id).as_bytes(),
    );
    let request = NativeSnapshotRequest {
        backup_id: cloud_backup_id,
        instance_id,
        source: format!("{}/{}", service.service_type, service.name),
        engine,
        format,
        compression,
        identity,
        objects: declarations.clone(),
    };
    let snapshot = link
        .declare_native_snapshot(&request)
        .await
        .map_err(|error| StageError::Retry(error.to_string()))?;
    if snapshot.upload_required {
        for declaration in declarations {
            upload_native_object(
                link,
                &client,
                &source_config.bucket_name,
                &root,
                instance_id,
                cloud_backup_id,
                declaration,
            )
            .await?;
        }
    }
    link.complete_native_snapshot(&WalGSnapshotCompleted {
        backup_id: cloud_backup_id,
    })
    .await
    .map_err(|error| StageError::Retry(error.to_string()))
}

async fn mirror_walg_backup(
    link: &CloudLink,
    db: &DatabaseConnection,
    encryption: &EncryptionService,
    backup: &backups::Model,
    instance_id: Uuid,
) -> Result<(), StageError> {
    let root = walg_root_key(&backup.s3_location).ok_or_else(|| {
        StageError::Unsupported(format!(
            "{} is not a WAL-G repository; PostgreSQL Cloud backups require WAL-G",
            backup.s3_location
        ))
    })?;
    let external = external_service_backups::Entity::find()
        .filter(external_service_backups::Column::BackupId.eq(backup.id))
        .one(db)
        .await
        .map_err(|error| StageError::Retry(error.to_string()))?;
    let (source, engine, postgres_major) = load_postgres_identity(db, external).await?;
    let source_config = s3_sources::Entity::find_by_id(backup.s3_source_id)
        .one(db)
        .await
        .map_err(|error| StageError::Retry(error.to_string()))?
        .ok_or_else(|| {
            StageError::Retry(format!("S3 source {} is missing", backup.s3_source_id))
        })?;
    let client = s3_client(encryption, &source_config)?;
    let objects = list_repository_objects(&client, &source_config.bucket_name, &root).await?;
    let (sentinel_key, sentinel) = find_snapshot_sentinel(
        &client,
        &source_config.bucket_name,
        &objects,
        &backup.backup_id,
    )
    .await?;
    let backup_name = sentinel_key
        .rsplit('/')
        .next()
        .and_then(|name| name.strip_suffix("_backup_stop_sentinel.json"))
        .ok_or_else(|| StageError::Retry(format!("invalid WAL-G sentinel key {sentinel_key}")))?
        .to_string();
    let timeline = sentinel_u32(&sentinel, &["Timeline", "timeline"])
        .or_else(|| timeline_from_backup_name(&backup_name))
        .unwrap_or(1);
    let start_lsn = sentinel_lsn(&sentinel, &["StartLsn", "StartLSN", "start_lsn", "LSN"])
        .ok_or_else(|| {
            StageError::Unsupported("WAL-G sentinel does not report start LSN".into())
        })?;
    let finish_lsn = sentinel_lsn(&sentinel, &["FinishLsn", "FinishLSN", "finish_lsn", "LSN"])
        .ok_or_else(|| {
            StageError::Unsupported("WAL-G sentinel does not report finish LSN".into())
        })?;
    let first_wal = wal_segment_name(&start_lsn, timeline)?;
    let last_wal = wal_segment_name(&finish_lsn, timeline)?;
    let base_prefix = format!("{root}/basebackups_005/{backup_name}/");
    let wal_prefix = format!("{root}/wal_005/");
    let selected = objects
        .into_iter()
        .filter(|object| {
            object.key == sentinel_key
                || object.key.starts_with(&base_prefix)
                || object.key.strip_prefix(&wal_prefix).is_some_and(|name| {
                    let segment = name.get(..24).unwrap_or(name);
                    segment >= first_wal.as_str() && segment <= last_wal.as_str()
                })
        })
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return Err(StageError::Retry(format!(
            "WAL-G snapshot {backup_name} has no repository objects"
        )));
    }

    let mut declarations = Vec::with_capacity(selected.len());
    for object in &selected {
        let (bytes, checksum_sha256) =
            inspect_source_object(&client, &source_config.bucket_name, &object.key).await?;
        if bytes != object.bytes {
            return Err(StageError::Retry(format!(
                "WAL-G object {} changed while its manifest was being built",
                object.key
            )));
        }
        declarations.push(WalGObjectDeclaration {
            relative_key: object
                .key
                .strip_prefix(&format!("{root}/"))
                .ok_or_else(|| {
                    StageError::Retry(format!("object {} escaped repository", object.key))
                })?
                .to_string(),
            kind: if object.key == sentinel_key {
                WalGObjectKind::Sentinel
            } else if object.key.starts_with(&base_prefix) {
                WalGObjectKind::BaseBackup
            } else {
                WalGObjectKind::Wal
            },
            bytes,
            checksum_sha256,
        });
    }
    let cloud_backup_id = Uuid::new_v5(
        &link
            .tenant_id()
            .ok_or_else(|| StageError::Retry("Cloud link lost its tenant identity".into()))?,
        format!("{instance_id}:{}", backup.backup_id).as_bytes(),
    );
    let request = WalGSnapshotRequest {
        backup_id: cloud_backup_id,
        instance_id,
        source,
        engine,
        postgres_major,
        postgres_system_identifier: sentinel_string(
            &sentinel,
            &["SystemIdentifier", "system_identifier"],
        )
        .ok_or_else(|| {
            StageError::Unsupported(
                "WAL-G sentinel does not report PostgreSQL system identifier".into(),
            )
        })?,
        backup_name,
        timeline,
        start_lsn,
        finish_lsn,
        objects: declarations.clone(),
    };
    let snapshot = link
        .declare_walg_snapshot(&request)
        .await
        .map_err(|error| StageError::Retry(error.to_string()))?;
    if snapshot.upload_required {
        for declaration in declarations {
            upload_repository_object(
                link,
                &client,
                &source_config.bucket_name,
                &root,
                instance_id,
                cloud_backup_id,
                declaration,
            )
            .await?;
        }
    }
    link.complete_walg_snapshot(&WalGSnapshotCompleted {
        backup_id: cloud_backup_id,
    })
    .await
    .map_err(|error| StageError::Retry(error.to_string()))
}

struct SourceObject {
    key: String,
    bytes: u64,
}

async fn list_repository_objects(
    client: &S3Client,
    bucket: &str,
    root: &str,
) -> Result<Vec<SourceObject>, StageError> {
    let mut objects = Vec::new();
    let mut continuation = None;
    loop {
        let mut request = client
            .list_objects_v2()
            .bucket(bucket)
            .prefix(format!("{root}/"));
        if let Some(token) = continuation.take() {
            request = request.continuation_token(token);
        }
        let response = request.send().await.map_err(|error| {
            StageError::Retry(format!("could not list WAL-G repository: {error}"))
        })?;
        for object in response.contents() {
            if let (Some(key), Some(bytes)) = (
                object.key(),
                object.size().and_then(|value| u64::try_from(value).ok()),
            ) {
                objects.push(SourceObject {
                    key: key.to_string(),
                    bytes,
                });
            }
        }
        if response.is_truncated().unwrap_or(false) {
            continuation = response.next_continuation_token().map(str::to_string);
            if continuation.is_none() {
                break;
            }
        } else {
            break;
        }
    }
    Ok(objects)
}

async fn find_snapshot_sentinel(
    client: &S3Client,
    bucket: &str,
    objects: &[SourceObject],
    backup_uuid: &str,
) -> Result<(String, serde_json::Value), StageError> {
    for object in objects
        .iter()
        .filter(|object| object.key.ends_with("_backup_stop_sentinel.json"))
    {
        let response = client
            .get_object()
            .bucket(bucket)
            .key(&object.key)
            .send()
            .await
            .map_err(|error| {
                StageError::Retry(format!(
                    "could not read WAL-G sentinel {}: {error}",
                    object.key
                ))
            })?;
        let bytes = response
            .body
            .collect()
            .await
            .map_err(|error| {
                StageError::Retry(format!(
                    "could not collect WAL-G sentinel {}: {error}",
                    object.key
                ))
            })?
            .into_bytes();
        let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
            StageError::Retry(format!(
                "WAL-G sentinel {} is invalid JSON: {error}",
                object.key
            ))
        })?;
        if contains_backup_identity(&value, backup_uuid) {
            return Ok((object.key.clone(), value));
        }
    }
    Err(StageError::Unsupported(format!(
        "WAL-G repository has no sentinel carrying temps_backup_id={backup_uuid}; rerun the backup with the current Temps image"
    )))
}

fn contains_backup_identity(value: &serde_json::Value, backup_uuid: &str) -> bool {
    match value {
        serde_json::Value::Object(values) => values.iter().any(|(key, value)| {
            (key == "temps_backup_id" && value.as_str() == Some(backup_uuid))
                || contains_backup_identity(value, backup_uuid)
        }),
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| contains_backup_identity(value, backup_uuid)),
        _ => false,
    }
}

fn sentinel_string(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    match value {
        serde_json::Value::Object(values) => {
            for key in keys {
                if let Some(value) = values.get(*key) {
                    if let Some(string) = value.as_str() {
                        return Some(string.to_string());
                    }
                    if let Some(number) = value.as_u64() {
                        return Some(number.to_string());
                    }
                }
            }
            values
                .values()
                .find_map(|value| sentinel_string(value, keys))
        }
        serde_json::Value::Array(values) => {
            values.iter().find_map(|value| sentinel_string(value, keys))
        }
        _ => None,
    }
}

fn sentinel_lsn(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    match value {
        serde_json::Value::Object(values) => {
            for key in keys {
                if let Some(value) = values.get(*key) {
                    if let Some(string) = value.as_str() {
                        return Some(string.to_string());
                    }
                    if let Some(number) = value.as_u64() {
                        return Some(format!("{:X}/{:X}", number >> 32, number & 0xffff_ffff));
                    }
                }
            }
            values.values().find_map(|value| sentinel_lsn(value, keys))
        }
        serde_json::Value::Array(values) => {
            values.iter().find_map(|value| sentinel_lsn(value, keys))
        }
        _ => None,
    }
}

fn timeline_from_backup_name(name: &str) -> Option<u32> {
    u32::from_str_radix(name.strip_prefix("base_")?.get(..8)?, 16).ok()
}

fn sentinel_u32(value: &serde_json::Value, keys: &[&str]) -> Option<u32> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
    })
}

/// Convert an LSN into the sortable 24-character WAL segment filename used by
/// Temps-managed PostgreSQL images (default 16 MiB WAL segment size).
fn wal_segment_name(lsn: &str, timeline: u32) -> Result<String, StageError> {
    const SEGMENT_BYTES: u64 = 16 * 1024 * 1024;
    const SEGMENTS_PER_LOG: u64 = 0x1_0000_0000 / SEGMENT_BYTES;
    let (high, low) = lsn.split_once('/').ok_or_else(|| {
        StageError::Unsupported(format!("WAL-G sentinel contains invalid LSN {lsn:?}"))
    })?;
    let high = u64::from_str_radix(high, 16).map_err(|error| {
        StageError::Unsupported(format!(
            "WAL-G sentinel contains invalid LSN {lsn:?}: {error}"
        ))
    })?;
    let low = u64::from_str_radix(low, 16).map_err(|error| {
        StageError::Unsupported(format!(
            "WAL-G sentinel contains invalid LSN {lsn:?}: {error}"
        ))
    })?;
    let segment_number = ((high << 32) | low) / SEGMENT_BYTES;
    Ok(format!(
        "{timeline:08X}{:08X}{:08X}",
        segment_number / SEGMENTS_PER_LOG,
        segment_number % SEGMENTS_PER_LOG
    ))
}

async fn inspect_source_object(
    client: &S3Client,
    bucket: &str,
    key: &str,
) -> Result<(u64, String), StageError> {
    let response = client
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .map_err(|error| {
            StageError::Retry(format!("could not read WAL-G object {key}: {error}"))
        })?;
    let mut reader = response.body.into_async_read();
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut bytes = 0_u64;
    let mut digest = Sha256::new();
    loop {
        let read = reader.read(&mut buffer).await.map_err(|error| {
            StageError::Retry(format!("could not stream WAL-G object {key}: {error}"))
        })?;
        if read == 0 {
            break;
        }
        bytes = bytes
            .checked_add(read as u64)
            .ok_or_else(|| StageError::Retry(format!("WAL-G object {key} is too large")))?;
        digest.update(&buffer[..read]);
    }
    let checksum = digest
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            use std::fmt::Write;
            let _ = write!(output, "{byte:02x}");
            output
        });
    Ok((bytes, checksum))
}

async fn upload_repository_object(
    link: &CloudLink,
    source_client: &S3Client,
    bucket: &str,
    root: &str,
    instance_id: Uuid,
    backup_id: Uuid,
    declaration: WalGObjectDeclaration,
) -> Result<(), StageError> {
    let target = link
        .walg_object_target(&WalGObjectTargetRequest {
            backup_id,
            instance_id,
            relative_key: declaration.relative_key.clone(),
        })
        .await
        .map_err(|error| StageError::Retry(error.to_string()))?;
    if target.upload_required {
        let source_key = format!("{root}/{}", declaration.relative_key);
        let http = reqwest::Client::new();
        let mut last_failure = None;
        for attempt in 0..3 {
            // Reopen the S3 source on every attempt. The stream is not
            // rewindable, but retrying never allocates a local staging file.
            let response = source_client
                .get_object()
                .bucket(bucket)
                .key(&source_key)
                .send()
                .await
                .map_err(|error| {
                    StageError::Retry(format!(
                        "could not reopen WAL-G object {source_key}: {error}"
                    ))
                })?;
            let body =
                reqwest::Body::wrap_stream(ReaderStream::new(response.body.into_async_read()));
            let mut upload = http.put(&target.upload_url).body(body);
            for (name, value) in &target.headers {
                upload = upload.header(name, value);
            }
            match upload.send().await {
                Ok(response) if response.status().is_success() => {
                    last_failure = None;
                    break;
                }
                Ok(response)
                    if matches!(response.status().as_u16(), 408 | 425 | 429 | 500..=599) =>
                {
                    last_failure = Some(format!("object storage returned {}", response.status()));
                }
                Ok(response) => {
                    return Err(StageError::Unsupported(format!(
                        "Cloud object storage rejected {} with {}",
                        declaration.relative_key,
                        response.status()
                    )));
                }
                Err(error) => last_failure = Some(error.without_url().to_string()),
            }
            if attempt < 2 {
                tokio::time::sleep(Duration::from_millis(250 * (1 << attempt))).await;
            }
        }
        if let Some(reason) = last_failure {
            return Err(StageError::Retry(format!(
                "WAL-G object {} did not upload after bounded retries: {reason}",
                declaration.relative_key
            )));
        }
    }
    link.complete_walg_object(&WalGObjectCompleted {
        backup_id,
        relative_key: declaration.relative_key,
        bytes: declaration.bytes,
        checksum_sha256: declaration.checksum_sha256,
    })
    .await
    .map_err(|error| StageError::Retry(error.to_string()))
}

async fn upload_native_object(
    link: &CloudLink,
    source_client: &S3Client,
    bucket: &str,
    root: &str,
    instance_id: Uuid,
    backup_id: Uuid,
    declaration: NativeSnapshotObjectDeclaration,
) -> Result<(), StageError> {
    let target = link
        .native_object_target(&WalGObjectTargetRequest {
            backup_id,
            instance_id,
            relative_key: declaration.relative_key.clone(),
        })
        .await
        .map_err(|error| StageError::Retry(error.to_string()))?;
    if target.upload_required {
        let source_key = format!("{root}/{}", declaration.relative_key);
        let http = reqwest::Client::new();
        let mut last_failure = None;
        for attempt in 0..3 {
            let response = source_client
                .get_object()
                .bucket(bucket)
                .key(&source_key)
                .send()
                .await
                .map_err(|error| {
                    StageError::Retry(format!(
                        "could not reopen native snapshot object {source_key}: {error}"
                    ))
                })?;
            let body =
                reqwest::Body::wrap_stream(ReaderStream::new(response.body.into_async_read()));
            let mut upload = http.put(&target.upload_url).body(body);
            for (name, value) in &target.headers {
                upload = upload.header(name, value);
            }
            match upload.send().await {
                Ok(response) if response.status().is_success() => {
                    last_failure = None;
                    break;
                }
                Ok(response)
                    if matches!(response.status().as_u16(), 408 | 425 | 429 | 500..=599) =>
                {
                    last_failure = Some(format!("object storage returned {}", response.status()));
                }
                Ok(response) => {
                    return Err(StageError::Unsupported(format!(
                        "Cloud object storage rejected {} with {}",
                        declaration.relative_key,
                        response.status()
                    )));
                }
                Err(error) => last_failure = Some(error.without_url().to_string()),
            }
            if attempt < 2 {
                tokio::time::sleep(Duration::from_millis(250 * (1 << attempt))).await;
            }
        }
        if let Some(reason) = last_failure {
            return Err(StageError::Retry(format!(
                "native snapshot object {} did not upload after bounded retries: {reason}",
                declaration.relative_key
            )));
        }
    }
    link.complete_native_object(&WalGObjectCompleted {
        backup_id,
        relative_key: declaration.relative_key,
        bytes: declaration.bytes,
        checksum_sha256: declaration.checksum_sha256,
    })
    .await
    .map_err(|error| StageError::Retry(error.to_string()))
}

fn s3_key(expected_bucket: &str, location: &str) -> Result<String, StageError> {
    if let Some(without_scheme) = location.strip_prefix("s3://") {
        let (bucket, key) = without_scheme.split_once('/').ok_or_else(|| {
            StageError::Unsupported(format!("S3 location {location:?} has no object key"))
        })?;
        if bucket != expected_bucket {
            return Err(StageError::Unsupported(format!(
                "backup location bucket {bucket} does not match configured source bucket {expected_bucket}"
            )));
        }
        return Ok(key.trim_matches('/').to_string());
    }
    let key = location.trim_matches('/');
    if key.is_empty() {
        return Err(StageError::Unsupported(
            "backup location has no object key".into(),
        ));
    }
    Ok(key.to_string())
}

async fn read_json_object(
    client: &S3Client,
    bucket: &str,
    key: &str,
) -> Result<serde_json::Value, StageError> {
    let response = client
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .map_err(|error| StageError::Retry(format!("could not read {key}: {error}")))?;
    let bytes = response
        .body
        .collect()
        .await
        .map_err(|error| StageError::Retry(format!("could not collect {key}: {error}")))?
        .into_bytes();
    serde_json::from_slice(&bytes)
        .map_err(|error| StageError::Retry(format!("object {key} is invalid JSON: {error}")))
}

fn walg_root_key(location: &str) -> Option<String> {
    let without_scheme = location.strip_prefix("s3://")?;
    let (_, key) = without_scheme.split_once('/')?;
    let key = key.trim_end_matches('/');
    (key.ends_with("/walg") || key.contains("/walg/")).then(|| key.to_string())
}

async fn load_postgres_identity(
    db: &DatabaseConnection,
    external: Option<external_service_backups::Model>,
) -> Result<(String, BackupEngine, u16), StageError> {
    if let Some(external) = external {
        let service = external_services::Entity::find_by_id(external.service_id)
            .one(db)
            .await
            .map_err(|error| StageError::Retry(error.to_string()))?
            .ok_or_else(|| {
                StageError::Retry(format!("service {} is missing", external.service_id))
            })?;
        let engine = match service.service_type.to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" => BackupEngine::Postgres,
            "timescale" | "timescaledb" => BackupEngine::TimescaleDb,
            other => {
                return Err(StageError::Unsupported(format!(
                    "engine {other} is not WAL-G compatible"
                )))
            }
        };
        let major = parse_postgres_major(service.version.as_deref()).ok_or_else(|| {
            StageError::Unsupported(format!(
                "service {} has no PostgreSQL major version",
                service.name
            ))
        })?;
        Ok((
            format!("{}/{}", service.service_type, service.name),
            engine,
            major,
        ))
    } else {
        let row = db
            .query_one(Statement::from_string(
                db.get_database_backend(),
                "SELECT current_setting('server_version') AS server_version".to_string(),
            ))
            .await
            .map_err(|error| StageError::Retry(error.to_string()))?
            .ok_or_else(|| StageError::Retry("PostgreSQL did not return server_version".into()))?;
        let version: String = row
            .try_get("", "server_version")
            .map_err(|error| StageError::Retry(error.to_string()))?;
        let major = parse_postgres_major(Some(&version)).ok_or_else(|| {
            StageError::Unsupported(format!("unsupported PostgreSQL version {version}"))
        })?;
        Ok((
            "postgres/control-plane".into(),
            BackupEngine::Postgres,
            major,
        ))
    }
}

fn s3_client(
    encryption: &EncryptionService,
    source: &s3_sources::Model,
) -> Result<S3Client, StageError> {
    let access_key = encryption
        .decrypt_string(&source.access_key_id)
        .map_err(|error| StageError::Retry(format!("could not decrypt S3 access key: {error}")))?;
    let secret_key = encryption
        .decrypt_string(&source.secret_key)
        .map_err(|error| StageError::Retry(format!("could not decrypt S3 secret key: {error}")))?;
    let credentials = aws_sdk_s3::config::Credentials::new(
        access_key,
        secret_key,
        None,
        None,
        "cloud-backup-mirror",
    );
    let mut builder = Config::builder()
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
        .region(aws_sdk_s3::config::Region::new(source.region.clone()))
        .force_path_style(source.force_path_style.unwrap_or(true))
        .credentials_provider(credentials);
    if let Some(endpoint) = &source.endpoint {
        builder = builder.endpoint_url(if endpoint.starts_with("http") {
            endpoint.clone()
        } else {
            format!("http://{endpoint}")
        });
    }
    Ok(S3Client::from_conf(builder.build()))
}

fn parse_postgres_major(version: Option<&str>) -> Option<u16> {
    version?
        .trim_start_matches(|character: char| !character.is_ascii_digit())
        .split(|character: char| !character.is_ascii_digit())
        .next()
        .and_then(|major| major.parse().ok())
        .filter(|major| (10..=99).contains(major))
}

fn service_engine_version(
    encryption: &EncryptionService,
    service: &external_services::Model,
) -> String {
    if let Some(version) = service
        .version
        .as_deref()
        .map(str::trim)
        .filter(|version| !version.is_empty())
    {
        return version.to_owned();
    }

    service
        .config
        .as_deref()
        .and_then(|config| encryption.decrypt_string(config).ok())
        .and_then(|config| serde_json::from_str::<serde_json::Value>(&config).ok())
        .and_then(|config| {
            config
                .get("docker_image")
                .and_then(serde_json::Value::as_str)
                .and_then(image_tag_version)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "unknown".to_string())
}

fn image_tag_version(image: &str) -> Option<&str> {
    let image = image.split('@').next()?.trim();
    let (repository, tag) = image.rsplit_once(':')?;
    (!tag.is_empty() && !tag.contains('/') && !repository.ends_with('/')).then_some(tag)
}

async fn persist_state(
    db: &DatabaseConnection,
    backup: backups::Model,
    tenant_id: Uuid,
    state: &str,
    reason: Option<&str>,
) -> Result<(), sea_orm::DbErr> {
    let mut metadata = serde_json::from_str::<serde_json::Value>(&backup.metadata)
        .unwrap_or_else(|_| serde_json::json!({}));
    if !metadata.is_object() {
        metadata = serde_json::json!({});
    }
    let Some(root) = metadata.as_object_mut() else {
        return Ok(());
    };
    let cloud = root
        .entry("cloud_mirror")
        .or_insert_with(|| serde_json::json!({}));
    if !cloud.is_object() {
        *cloud = serde_json::json!({});
    }
    let Some(cloud) = cloud.as_object_mut() else {
        return Ok(());
    };
    cloud.insert(
        tenant_id.to_string(),
        serde_json::json!({
            "state": state,
            "reason": reason,
            "updated_at": chrono::Utc::now().to_rfc3339(),
        }),
    );
    let mut active: backups::ActiveModel = backup.into();
    active.metadata = Set(metadata.to_string());
    active.update(db).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        contains_backup_identity, image_tag_version, next_sweep_interval, parse_postgres_major,
        s3_key, sentinel_lsn, supports_native_mirror, timeline_from_backup_name, wal_segment_name,
        walg_root_key, SweepOutcome, BASE_SWEEP_INTERVAL, MAX_SWEEP_INTERVAL,
    };
    use std::time::Duration;

    #[test]
    fn cloud_outages_use_bounded_exponential_backoff() {
        let first = next_sweep_interval(Duration::ZERO, SweepOutcome::Retry);
        let second = next_sweep_interval(first, SweepOutcome::Retry);
        let third = next_sweep_interval(second, SweepOutcome::Retry);

        assert_eq!(first, BASE_SWEEP_INTERVAL);
        assert_eq!(second, BASE_SWEEP_INTERVAL * 2);
        assert_eq!(third, BASE_SWEEP_INTERVAL * 4);

        let mut interval = third;
        for _ in 0..20 {
            interval = next_sweep_interval(interval, SweepOutcome::Retry);
        }
        assert_eq!(interval, MAX_SWEEP_INTERVAL);
    }

    #[test]
    fn mirror_progress_resets_backoff_immediately() {
        assert_eq!(
            next_sweep_interval(MAX_SWEEP_INTERVAL, SweepOutcome::Progress),
            BASE_SWEEP_INTERVAL
        );
        assert_eq!(
            next_sweep_interval(MAX_SWEEP_INTERVAL, SweepOutcome::Idle),
            BASE_SWEEP_INTERVAL
        );
    }

    #[test]
    fn parses_supported_postgres_version_shapes() {
        assert_eq!(parse_postgres_major(Some("17.6")), Some(17));
        assert_eq!(parse_postgres_major(Some("pg16")), Some(16));
        assert_eq!(parse_postgres_major(None), None);
    }

    #[test]
    fn derives_engine_version_from_managed_image_tags() {
        assert_eq!(
            image_tag_version("gotempsh/mariadb-walg:11.4"),
            Some("11.4")
        );
        assert_eq!(
            image_tag_version("registry.example.com:5000/mongodb-walg:8.0"),
            Some("8.0")
        );
        assert_eq!(image_tag_version("rustfs/rustfs@sha256:abc"), None);
        assert_eq!(image_tag_version("mariadb"), None);
    }

    #[test]
    fn identifies_only_walg_repository_locations() {
        assert_eq!(
            walg_root_key("s3://bucket/team/postgres/walg"),
            Some("team/postgres/walg".into())
        );
        assert_eq!(walg_root_key("team/postgres/walg"), None);
        assert_eq!(walg_root_key("s3://bucket/backup.sql.gz"), None);
    }

    #[test]
    fn native_locations_are_bound_to_the_configured_bucket() {
        assert_eq!(
            s3_key("backups", "s3://backups/tenant/rustfs/snapshot").ok(),
            Some("tenant/rustfs/snapshot".into())
        );
        assert_eq!(
            s3_key("backups", "/tenant/mariadb/base.mbstream.gz").ok(),
            Some("tenant/mariadb/base.mbstream.gz".into())
        );
        assert!(s3_key("backups", "s3://other/tenant/backup").is_err());
        assert!(s3_key("backups", "").is_err());
    }

    #[test]
    fn object_store_service_aliases_use_native_snapshot_mirroring() {
        for service_type in ["rustfs", "s3", "minio", "blob"] {
            assert!(
                supports_native_mirror(service_type),
                "{service_type} must reach the object-set mirror contract"
            );
        }
    }

    #[test]
    fn sentinel_identity_is_found_inside_user_data() {
        let sentinel = serde_json::json!({
            "UserData": { "temps_backup_id": "backup-42" },
            "LSN": "0/2000000"
        });
        assert!(contains_backup_identity(&sentinel, "backup-42"));
        assert!(!contains_backup_identity(&sentinel, "backup-43"));
    }

    #[test]
    fn lsn_bounds_map_to_sortable_wal_segments() {
        assert_eq!(
            wal_segment_name("0/2000000", 1).ok().as_deref(),
            Some("000000010000000000000002")
        );
        assert_eq!(
            wal_segment_name("1/0", 2).ok().as_deref(),
            Some("000000020000000100000000")
        );
        assert!(wal_segment_name("not-an-lsn", 1).is_err());
    }

    #[test]
    fn real_walg_numeric_lsn_and_backup_timeline_are_supported() {
        let sentinel = serde_json::json!({
            "LSN": 33_554_432_u64,
            "FinishLsn": 50_331_648_u64,
            "SystemIdentifier": 7_420_000_000_000_000_000_u64,
        });
        assert_eq!(
            sentinel_lsn(&sentinel, &["LSN"]).as_deref(),
            Some("0/2000000")
        );
        assert_eq!(
            timeline_from_backup_name("base_000000030000000000000002"),
            Some(3)
        );
    }
}
