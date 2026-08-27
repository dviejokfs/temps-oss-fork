use crate::types::NotificationSeverity;
use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, DbErr, EntityTrait, PaginatorTrait,
    QueryFilter, QueryOrder, Set, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use temps_entities::{notification_providers, notification_route_providers, notification_routes};
use utoipa::ToSchema;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct NotificationRoute {
    pub id: i32,
    pub name: String,
    pub enabled: bool,
    pub min_severity: String,
    pub max_severity: String,
    pub provider_ids: Vec<i32>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct CreateNotificationRoute {
    pub name: String,
    pub enabled: bool,
    pub min_severity: String,
    pub max_severity: String,
    pub provider_ids: Vec<i32>,
}

#[derive(Debug, Clone, Default)]
pub struct UpdateNotificationRoute {
    pub name: Option<String>,
    pub enabled: Option<bool>,
    pub min_severity: Option<String>,
    pub max_severity: Option<String>,
    pub provider_ids: Option<Vec<i32>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct NotificationRoutePage {
    pub items: Vec<NotificationRoute>,
    pub total: u64,
    pub page: u64,
    pub page_size: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum NotificationRouteError {
    #[error("Notification route name must not be empty")]
    InvalidName,
    #[error(
        "Minimum severity '{value}' is invalid; expected one of: debug, info, warning, error, critical, emergency"
    )]
    InvalidMinimumSeverity { value: String },
    #[error(
        "Maximum severity '{value}' is invalid; expected one of: debug, info, warning, error, critical, emergency"
    )]
    InvalidMaximumSeverity { value: String },
    #[error("Minimum severity '{min}' must not be higher than maximum severity '{max}'")]
    InvalidSeverityRange { min: String, max: String },
    #[error("A notification route must contain at least one provider")]
    NoProviders,
    #[error("Notification provider {provider_id} assigned to the route was not found")]
    ProviderNotFound { provider_id: i32 },
    #[error("Notification route {route_id} was not found")]
    RouteNotFound { route_id: i32 },
    #[error("A notification route named '{name}' already exists")]
    DuplicateName { name: String },
    #[error("Failed to {operation} notification route {route_id:?}: {source}")]
    Database {
        route_id: Option<i32>,
        operation: &'static str,
        #[source]
        source: DbErr,
    },
}

#[derive(Clone)]
pub struct NotificationRoutingService {
    db: Arc<DatabaseConnection>,
}

impl NotificationRoutingService {
    pub fn new(db: Arc<DatabaseConnection>) -> Self {
        Self { db }
    }

    fn normalize_name(name: String) -> Result<String, NotificationRouteError> {
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err(NotificationRouteError::InvalidName);
        }
        Ok(name)
    }

    fn normalize_min_severity(value: String) -> Result<String, NotificationRouteError> {
        NotificationSeverity::from_str(value.trim())
            .map(|severity| severity.as_str().to_string())
            .ok_or(NotificationRouteError::InvalidMinimumSeverity { value })
    }

    fn normalize_max_severity(value: String) -> Result<String, NotificationRouteError> {
        NotificationSeverity::from_str(value.trim())
            .map(|severity| severity.as_str().to_string())
            .ok_or(NotificationRouteError::InvalidMaximumSeverity { value })
    }

    fn validate_severity_range(
        min_severity: &str,
        max_severity: &str,
    ) -> Result<(), NotificationRouteError> {
        let minimum = NotificationSeverity::from_str(min_severity).ok_or_else(|| {
            NotificationRouteError::InvalidMinimumSeverity {
                value: min_severity.to_string(),
            }
        })?;
        let maximum = NotificationSeverity::from_str(max_severity).ok_or_else(|| {
            NotificationRouteError::InvalidMaximumSeverity {
                value: max_severity.to_string(),
            }
        })?;
        if minimum > maximum {
            return Err(NotificationRouteError::InvalidSeverityRange {
                min: min_severity.to_string(),
                max: max_severity.to_string(),
            });
        }
        Ok(())
    }

    fn normalize_provider_ids(
        mut provider_ids: Vec<i32>,
    ) -> Result<Vec<i32>, NotificationRouteError> {
        provider_ids.sort_unstable();
        provider_ids.dedup();
        if provider_ids.is_empty() {
            return Err(NotificationRouteError::NoProviders);
        }
        Ok(provider_ids)
    }

    async fn validate_providers<C>(
        db: &C,
        provider_ids: &[i32],
        route_id: Option<i32>,
    ) -> Result<(), NotificationRouteError>
    where
        C: sea_orm::ConnectionTrait,
    {
        let found = notification_providers::Entity::find()
            .filter(notification_providers::Column::Id.is_in(provider_ids.iter().copied()))
            .all(db)
            .await
            .map_err(|source| NotificationRouteError::Database {
                route_id,
                operation: "validate providers for",
                source,
            })?;
        let found_ids: HashSet<i32> = found.into_iter().map(|provider| provider.id).collect();
        if let Some(provider_id) = provider_ids
            .iter()
            .find(|provider_id| !found_ids.contains(provider_id))
        {
            return Err(NotificationRouteError::ProviderNotFound {
                provider_id: *provider_id,
            });
        }
        Ok(())
    }

    async fn load_provider_ids<C>(
        db: &C,
        route_ids: &[i32],
        route_id: Option<i32>,
    ) -> Result<HashMap<i32, Vec<i32>>, NotificationRouteError>
    where
        C: sea_orm::ConnectionTrait,
    {
        if route_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let assignments = notification_route_providers::Entity::find()
            .filter(notification_route_providers::Column::RouteId.is_in(route_ids.iter().copied()))
            .order_by_asc(notification_route_providers::Column::ProviderId)
            .all(db)
            .await
            .map_err(|source| NotificationRouteError::Database {
                route_id,
                operation: "load provider assignments for",
                source,
            })?;
        let mut by_route = HashMap::<i32, Vec<i32>>::new();
        for assignment in assignments {
            by_route
                .entry(assignment.route_id)
                .or_default()
                .push(assignment.provider_id);
        }
        Ok(by_route)
    }

    fn map_route(
        route: notification_routes::Model,
        provider_ids: &mut HashMap<i32, Vec<i32>>,
    ) -> NotificationRoute {
        NotificationRoute {
            id: route.id,
            name: route.name,
            enabled: route.enabled,
            min_severity: route.min_severity,
            max_severity: route.max_severity,
            provider_ids: provider_ids.remove(&route.id).unwrap_or_default(),
            created_at: route.created_at.timestamp_millis(),
            updated_at: route.updated_at.timestamp_millis(),
        }
    }

    pub async fn list(
        &self,
        page: u64,
        page_size: u64,
    ) -> Result<NotificationRoutePage, NotificationRouteError> {
        let page = page.max(1);
        let page_size = page_size.clamp(1, 100);
        let paginator = notification_routes::Entity::find()
            .order_by_asc(notification_routes::Column::Name)
            .paginate(self.db.as_ref(), page_size);
        let total =
            paginator
                .num_items()
                .await
                .map_err(|source| NotificationRouteError::Database {
                    route_id: None,
                    operation: "count",
                    source,
                })?;
        let routes = paginator.fetch_page(page - 1).await.map_err(|source| {
            NotificationRouteError::Database {
                route_id: None,
                operation: "list",
                source,
            }
        })?;
        let route_ids: Vec<i32> = routes.iter().map(|route| route.id).collect();
        let mut provider_ids = Self::load_provider_ids(self.db.as_ref(), &route_ids, None).await?;
        Ok(NotificationRoutePage {
            items: routes
                .into_iter()
                .map(|route| Self::map_route(route, &mut provider_ids))
                .collect(),
            total,
            page,
            page_size,
        })
    }

    pub async fn get(&self, route_id: i32) -> Result<NotificationRoute, NotificationRouteError> {
        let route = notification_routes::Entity::find_by_id(route_id)
            .one(self.db.as_ref())
            .await
            .map_err(|source| NotificationRouteError::Database {
                route_id: Some(route_id),
                operation: "load",
                source,
            })?
            .ok_or(NotificationRouteError::RouteNotFound { route_id })?;
        let mut provider_ids =
            Self::load_provider_ids(self.db.as_ref(), &[route_id], Some(route_id)).await?;
        Ok(Self::map_route(route, &mut provider_ids))
    }

    pub async fn create(
        &self,
        input: CreateNotificationRoute,
    ) -> Result<NotificationRoute, NotificationRouteError> {
        let name = Self::normalize_name(input.name)?;
        let min_severity = Self::normalize_min_severity(input.min_severity)?;
        let max_severity = Self::normalize_max_severity(input.max_severity)?;
        Self::validate_severity_range(&min_severity, &max_severity)?;
        let provider_ids = Self::normalize_provider_ids(input.provider_ids)?;
        let transaction =
            self.db
                .begin()
                .await
                .map_err(|source| NotificationRouteError::Database {
                    route_id: None,
                    operation: "begin creating",
                    source,
                })?;
        Self::validate_providers(&transaction, &provider_ids, None).await?;
        let now = Utc::now();
        let route = notification_routes::ActiveModel {
            name: Set(name.clone()),
            enabled: Set(input.enabled),
            min_severity: Set(min_severity),
            max_severity: Set(max_severity),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&transaction)
        .await
        .map_err(|source| {
            if source
                .sql_err()
                .is_some_and(|error| matches!(error, sea_orm::SqlErr::UniqueConstraintViolation(_)))
            {
                NotificationRouteError::DuplicateName { name }
            } else {
                NotificationRouteError::Database {
                    route_id: None,
                    operation: "create",
                    source,
                }
            }
        })?;
        notification_route_providers::Entity::insert_many(provider_ids.iter().map(|provider_id| {
            notification_route_providers::ActiveModel {
                route_id: Set(route.id),
                provider_id: Set(*provider_id),
                created_at: Set(now),
            }
        }))
        .exec(&transaction)
        .await
        .map_err(|source| NotificationRouteError::Database {
            route_id: Some(route.id),
            operation: "assign providers to",
            source,
        })?;
        transaction
            .commit()
            .await
            .map_err(|source| NotificationRouteError::Database {
                route_id: Some(route.id),
                operation: "commit creating",
                source,
            })?;
        Ok(NotificationRoute {
            id: route.id,
            name: route.name,
            enabled: route.enabled,
            min_severity: route.min_severity,
            max_severity: route.max_severity,
            provider_ids,
            created_at: route.created_at.timestamp_millis(),
            updated_at: route.updated_at.timestamp_millis(),
        })
    }

    pub async fn update(
        &self,
        route_id: i32,
        input: UpdateNotificationRoute,
    ) -> Result<NotificationRoute, NotificationRouteError> {
        let name = input.name.map(Self::normalize_name).transpose()?;
        let min_severity = input
            .min_severity
            .map(Self::normalize_min_severity)
            .transpose()?;
        let max_severity = input
            .max_severity
            .map(Self::normalize_max_severity)
            .transpose()?;
        let provider_ids = input
            .provider_ids
            .map(Self::normalize_provider_ids)
            .transpose()?;
        let transaction =
            self.db
                .begin()
                .await
                .map_err(|source| NotificationRouteError::Database {
                    route_id: Some(route_id),
                    operation: "begin updating",
                    source,
                })?;
        let route = notification_routes::Entity::find_by_id(route_id)
            .one(&transaction)
            .await
            .map_err(|source| NotificationRouteError::Database {
                route_id: Some(route_id),
                operation: "load for update",
                source,
            })?
            .ok_or(NotificationRouteError::RouteNotFound { route_id })?;
        Self::validate_severity_range(
            min_severity.as_deref().unwrap_or(&route.min_severity),
            max_severity.as_deref().unwrap_or(&route.max_severity),
        )?;
        if let Some(ref provider_ids) = provider_ids {
            Self::validate_providers(&transaction, provider_ids, Some(route_id)).await?;
        }
        let mut active: notification_routes::ActiveModel = route.into();
        if let Some(name) = name.clone() {
            active.name = Set(name);
        }
        if let Some(enabled) = input.enabled {
            active.enabled = Set(enabled);
        }
        if let Some(min_severity) = min_severity {
            active.min_severity = Set(min_severity);
        }
        if let Some(max_severity) = max_severity {
            active.max_severity = Set(max_severity);
        }
        active.updated_at = Set(Utc::now());
        let route = active.update(&transaction).await.map_err(|source| {
            if source
                .sql_err()
                .is_some_and(|error| matches!(error, sea_orm::SqlErr::UniqueConstraintViolation(_)))
            {
                NotificationRouteError::DuplicateName {
                    name: name.unwrap_or_else(|| route_id.to_string()),
                }
            } else {
                NotificationRouteError::Database {
                    route_id: Some(route_id),
                    operation: "update",
                    source,
                }
            }
        })?;
        let provider_ids = if let Some(provider_ids) = provider_ids {
            notification_route_providers::Entity::delete_many()
                .filter(notification_route_providers::Column::RouteId.eq(route_id))
                .exec(&transaction)
                .await
                .map_err(|source| NotificationRouteError::Database {
                    route_id: Some(route_id),
                    operation: "replace provider assignments for",
                    source,
                })?;
            notification_route_providers::Entity::insert_many(provider_ids.iter().map(
                |provider_id| notification_route_providers::ActiveModel {
                    route_id: Set(route_id),
                    provider_id: Set(*provider_id),
                    created_at: Set(Utc::now()),
                },
            ))
            .exec(&transaction)
            .await
            .map_err(|source| NotificationRouteError::Database {
                route_id: Some(route_id),
                operation: "assign providers to",
                source,
            })?;
            provider_ids
        } else {
            Self::load_provider_ids(&transaction, &[route_id], Some(route_id))
                .await?
                .remove(&route_id)
                .unwrap_or_default()
        };
        transaction
            .commit()
            .await
            .map_err(|source| NotificationRouteError::Database {
                route_id: Some(route_id),
                operation: "commit updating",
                source,
            })?;
        Ok(NotificationRoute {
            id: route.id,
            name: route.name,
            enabled: route.enabled,
            min_severity: route.min_severity,
            max_severity: route.max_severity,
            provider_ids,
            created_at: route.created_at.timestamp_millis(),
            updated_at: route.updated_at.timestamp_millis(),
        })
    }

    pub async fn delete(&self, route_id: i32) -> Result<(), NotificationRouteError> {
        let result = notification_routes::Entity::delete_by_id(route_id)
            .exec(self.db.as_ref())
            .await
            .map_err(|source| NotificationRouteError::Database {
                route_id: Some(route_id),
                operation: "delete",
                source,
            })?;
        if result.rows_affected == 0 {
            return Err(NotificationRouteError::RouteNotFound { route_id });
        }
        Ok(())
    }

    pub async fn resolve_provider_models(
        &self,
        severity: NotificationSeverity,
    ) -> Result<Vec<notification_providers::Model>, NotificationRouteError> {
        let routes = notification_routes::Entity::find()
            .filter(notification_routes::Column::Enabled.eq(true))
            .all(self.db.as_ref())
            .await
            .map_err(|source| NotificationRouteError::Database {
                route_id: None,
                operation: "load enabled routes for",
                source,
            })?;
        let matching_route_ids: Vec<i32> = routes
            .into_iter()
            .filter_map(|route| {
                let minimum = NotificationSeverity::from_str(&route.min_severity);
                let maximum = NotificationSeverity::from_str(&route.max_severity);
                match (minimum, maximum) {
                    (Some(minimum), Some(maximum))
                        if severity >= minimum && severity <= maximum =>
                    {
                        Some(route.id)
                    }
                    (Some(_), Some(_)) => None,
                    _ => {
                        tracing::error!(
                            route_id = route.id,
                            minimum_severity = %route.min_severity,
                            maximum_severity = %route.max_severity,
                            "Notification route has an invalid severity range and was skipped"
                        );
                        None
                    }
                }
            })
            .collect();
        let assignments =
            Self::load_provider_ids(self.db.as_ref(), &matching_route_ids, None).await?;
        let provider_ids: HashSet<i32> = assignments.into_values().flatten().collect();
        if provider_ids.is_empty() {
            return Ok(Vec::new());
        }
        notification_providers::Entity::find()
            .filter(notification_providers::Column::Enabled.eq(true))
            .filter(notification_providers::Column::Id.is_in(provider_ids))
            .order_by_asc(notification_providers::Column::CreatedAt)
            .all(self.db.as_ref())
            .await
            .map_err(|source| NotificationRouteError::Database {
                route_id: None,
                operation: "resolve providers for",
                source,
            })
    }

    pub async fn has_routable_provider(&self) -> Result<bool, NotificationRouteError> {
        let route_ids: Vec<i32> = notification_routes::Entity::find()
            .filter(notification_routes::Column::Enabled.eq(true))
            .all(self.db.as_ref())
            .await
            .map_err(|source| NotificationRouteError::Database {
                route_id: None,
                operation: "load enabled routes for configuration check",
                source,
            })?
            .into_iter()
            .map(|route| route.id)
            .collect();
        let provider_ids: HashSet<i32> =
            Self::load_provider_ids(self.db.as_ref(), &route_ids, None)
                .await?
                .into_values()
                .flatten()
                .collect();
        if provider_ids.is_empty() {
            return Ok(false);
        }
        notification_providers::Entity::find()
            .filter(notification_providers::Column::Enabled.eq(true))
            .filter(notification_providers::Column::Id.is_in(provider_ids))
            .one(self.db.as_ref())
            .await
            .map(|provider| provider.is_some())
            .map_err(|source| NotificationRouteError::Database {
                route_id: None,
                operation: "check enabled routed providers for",
                source,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use temps_database::test_utils::TestDatabase;

    #[test]
    fn severity_range_is_inclusive() {
        let minimum = NotificationSeverity::Warning;
        let maximum = NotificationSeverity::Error;
        assert!(NotificationSeverity::Warning >= minimum);
        assert!(NotificationSeverity::Error <= maximum);
        assert!(NotificationSeverity::Info < minimum);
        assert!(NotificationSeverity::Critical > maximum);
    }

    #[test]
    fn provider_ids_are_deduplicated_and_sorted() {
        let provider_ids = NotificationRoutingService::normalize_provider_ids(vec![3, 1, 3, 2])
            .expect("valid provider IDs should normalize");
        assert_eq!(provider_ids, vec![1, 2, 3]);
    }

    #[test]
    fn route_requires_a_provider() {
        assert!(matches!(
            NotificationRoutingService::normalize_provider_ids(Vec::new()),
            Err(NotificationRouteError::NoProviders)
        ));
    }

    #[test]
    fn severity_alias_is_stored_canonically() {
        assert_eq!(
            NotificationRoutingService::normalize_min_severity("warn".to_string())
                .expect("warn alias should be accepted"),
            "warning"
        );
    }

    #[test]
    fn reversed_severity_range_is_rejected() {
        assert!(matches!(
            NotificationRoutingService::validate_severity_range("critical", "warning"),
            Err(NotificationRouteError::InvalidSeverityRange { .. })
        ));
    }

    #[tokio::test]
    async fn route_crud_filters_severity_ranges_and_providers() {
        let test_db = match TestDatabase::with_migrations().await {
            Ok(test_db) => test_db,
            Err(error) => {
                let message = format!("{error:#}");
                if temps_database::test_utils::is_container_runtime_unavailable(&message)
                    || message.contains("Docker Desktop is unable to start")
                {
                    eprintln!("Skipping Docker-dependent routing test: {message}");
                    return;
                }
                panic!("Failed to create routing test database: {message}");
            }
        };
        let now = Utc::now();
        let primary = notification_providers::ActiveModel {
            name: Set("Slack team alerts".to_string()),
            provider_type: Set("slack".to_string()),
            config: Set("test-config".to_string()),
            enabled: Set(true),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(test_db.db.as_ref())
        .await
        .expect("primary provider should insert");
        let disabled = notification_providers::ActiveModel {
            name: Set("Disabled Slack".to_string()),
            provider_type: Set("slack".to_string()),
            config: Set("test-config".to_string()),
            enabled: Set(false),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(test_db.db.as_ref())
        .await
        .expect("disabled provider should insert");
        let on_call = notification_providers::ActiveModel {
            name: Set("Slack on-call".to_string()),
            provider_type: Set("slack".to_string()),
            config: Set("test-config".to_string()),
            enabled: Set(true),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(test_db.db.as_ref())
        .await
        .expect("on-call provider should insert");
        let service = NotificationRoutingService::new(test_db.connection_arc());

        let missing_provider_error = service
            .create(CreateNotificationRoute {
                name: "Broken route".to_string(),
                enabled: true,
                min_severity: "warning".to_string(),
                max_severity: "error".to_string(),
                provider_ids: vec![i32::MAX],
            })
            .await
            .expect_err("unknown provider must reject the route");
        assert!(matches!(
            missing_provider_error,
            NotificationRouteError::ProviderNotFound { .. }
        ));
        assert_eq!(
            service
                .list(1, 20)
                .await
                .expect("failed route creation must roll back")
                .total,
            0
        );

        let warning_route = service
            .create(CreateNotificationRoute {
                name: "Team alerts".to_string(),
                enabled: true,
                min_severity: "warning".to_string(),
                max_severity: "error".to_string(),
                provider_ids: vec![primary.id, disabled.id, primary.id],
            })
            .await
            .expect("warning route should create");
        assert_eq!(warning_route.min_severity, "warning");
        assert_eq!(warning_route.max_severity, "error");
        assert_eq!(warning_route.provider_ids, vec![primary.id, disabled.id]);

        let critical_route = service
            .create(CreateNotificationRoute {
                name: "On-call".to_string(),
                enabled: true,
                min_severity: "critical".to_string(),
                max_severity: "emergency".to_string(),
                provider_ids: vec![on_call.id],
            })
            .await
            .expect("critical route should create");
        assert!(matches!(
            service
                .create(CreateNotificationRoute {
                    name: "On-call".to_string(),
                    enabled: true,
                    min_severity: "error".to_string(),
                    max_severity: "emergency".to_string(),
                    provider_ids: vec![primary.id],
                })
                .await,
            Err(NotificationRouteError::DuplicateName { .. })
        ));

        assert!(service
            .resolve_provider_models(NotificationSeverity::Info)
            .await
            .expect("info routing should resolve")
            .is_empty());
        let warning = service
            .resolve_provider_models(NotificationSeverity::Warning)
            .await
            .expect("warning routing should resolve");
        assert_eq!(warning.len(), 1, "disabled providers must be excluded");
        assert_eq!(warning[0].id, primary.id);

        let critical = service
            .resolve_provider_models(NotificationSeverity::Critical)
            .await
            .expect("critical routing should resolve");
        assert_eq!(critical.len(), 1, "bounded ranges must not overlap");
        assert_eq!(critical[0].id, on_call.id);
        assert!(service
            .has_routable_provider()
            .await
            .expect("configured routing check should succeed"));

        let updated = service
            .update(
                warning_route.id,
                UpdateNotificationRoute {
                    enabled: Some(false),
                    ..Default::default()
                },
            )
            .await
            .expect("route should update");
        assert!(!updated.enabled);
        assert!(service
            .resolve_provider_models(NotificationSeverity::Warning)
            .await
            .expect("disabled route should resolve")
            .is_empty());

        service
            .delete(critical_route.id)
            .await
            .expect("route should delete");
        assert!(matches!(
            service.get(critical_route.id).await,
            Err(NotificationRouteError::RouteNotFound { .. })
        ));

        test_db
            .cleanup_all_tables()
            .await
            .expect("routing test data should clean up");
    }
}
