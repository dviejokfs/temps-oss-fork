//! HTTP client for calling the agent's service management API on remote nodes.
//!
//! Used by `ExternalServiceManager` to route service operations (create, start,
//! stop, remove) through a worker node's agent when `node_id` is set.

use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;
use tracing::{error, info};

use crate::externalsvc::ServiceConfig;
use crate::services::ExternalServiceError;

/// Response envelope from the agent API (mirrors `AgentResponse` in temps-agent).
#[derive(Deserialize)]
struct AgentResponse<T> {
    success: bool,
    data: Option<T>,
    error: Option<String>,
}

/// Lightweight client for the agent's service endpoints.
pub struct RemoteServiceClient {
    agent_url: String,
    token: String,
    node_name: String,
    client: reqwest::Client,
}

/// Parameters needed to create a service on a remote node.
#[derive(Debug, serde::Serialize)]
pub struct RemoteServiceCreateParams {
    pub name: String,
    pub service_type: String,
    pub image: String,
    pub environment: HashMap<String, String>,
    pub port_mappings: Vec<RemotePortMapping>,
    pub volumes: HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// Optional cgroup limits applied to the container (memory, swap, CPU).
    /// Skipped from the wire when unset, so older agents that don't know
    /// about the field still parse the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_limits: Option<crate::externalsvc::ServiceResourceLimits>,
}

#[derive(Debug, serde::Serialize)]
pub struct RemotePortMapping {
    /// Host port to bind. `0` means let Docker auto-assign a free port.
    pub host_port: u16,
    pub container_port: u16,
}

/// Response after creating a service on the agent.
#[derive(Debug, Deserialize)]
pub struct RemoteServiceCreateResponse {
    pub container_id: String,
    pub container_name: String,
    pub host_port: u16,
    /// Container's `temps-overlay` IP, when the container is attached to
    /// it. `None` from single-host clusters and from agents that haven't
    /// been upgraded yet (the field is `serde(default)`-able so older
    /// servers' responses still parse). See ADR-011.
    #[serde(default)]
    pub compute_ip: Option<String>,
}

/// Status of a service on a remote node.
#[derive(Debug, Deserialize)]
pub struct RemoteServiceStatus {
    pub container_name: String,
    pub container_id: Option<String>,
    pub running: bool,
    pub health: Option<String>,
}

/// Request body for `exec_in_service`. Mirrors `temps_agent::ServiceExecRequest`
/// but kept local to avoid a cross-crate dep.
#[derive(Debug, serde::Serialize)]
pub struct RemoteExecParams {
    pub container_name: String,
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub environment: HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default)]
    pub detach: bool,
}

/// Response from a remote container exec.
#[derive(Debug, Deserialize)]
pub struct RemoteExecResult {
    pub exit_code: i64,
    pub stdout: String,
    pub stderr: String,
}

/// Request to initialize a service and provision one project's logical
/// resource on the node that owns the service container.
#[derive(Debug, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct RemoteRuntimeEnvRequest {
    pub service_config: ServiceConfig,
    pub project_slug: String,
    pub environment_slug: String,
}

/// Runtime environment returned after the logical resource is provisioned.
#[derive(Debug, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct RemoteRuntimeEnvResponse {
    pub environment: HashMap<String, String>,
}

impl RemoteServiceClient {
    /// Create a new client for the given agent.
    ///
    /// * `agent_url` — base URL, e.g. `https://10.100.0.2:3100`
    /// * `token` — plaintext bearer token for auth
    /// * `node_name` — human-readable name (for error messages)
    pub fn new(
        agent_url: String,
        token: String,
        node_name: String,
    ) -> Result<Self, ExternalServiceError> {
        // Strict TLS by default; operators with self-signed agent certs
        // on a trusted internal network can opt in via the
        // `insecure_tls` toggle in the application settings UI.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .danger_accept_invalid_certs(temps_core::tls::insecure_tls_enabled())
            .build()
            .map_err(|e| ExternalServiceError::InternalError {
                reason: format!("Failed to create HTTP client for node {}: {}", node_name, e),
            })?;

        Ok(Self {
            agent_url,
            token,
            node_name,
            client,
        })
    }

    /// Create and start a service container on the remote node.
    pub async fn create_service(
        &self,
        params: RemoteServiceCreateParams,
    ) -> Result<RemoteServiceCreateResponse, ExternalServiceError> {
        info!(
            "Creating service '{}' on remote node '{}'",
            params.name, self.node_name
        );
        self.agent_post("/agent/services", &params).await
    }

    /// Start an existing service container on the remote node.
    pub async fn start_service(&self, container_name: &str) -> Result<(), ExternalServiceError> {
        info!(
            "Starting service '{}' on remote node '{}'",
            container_name, self.node_name
        );
        let _: serde_json::Value = self
            .agent_post_no_body(&format!("/agent/services/{}/start", container_name))
            .await?;
        Ok(())
    }

    /// Stop a service container on the remote node.
    pub async fn stop_service(&self, container_name: &str) -> Result<(), ExternalServiceError> {
        info!(
            "Stopping service '{}' on remote node '{}'",
            container_name, self.node_name
        );
        let _: serde_json::Value = self
            .agent_post_no_body(&format!("/agent/services/{}/stop", container_name))
            .await?;
        Ok(())
    }

    /// Remove a service container (and its volumes) on the remote node.
    pub async fn remove_service(&self, container_name: &str) -> Result<(), ExternalServiceError> {
        info!(
            "Removing service '{}' on remote node '{}'",
            container_name, self.node_name
        );
        self.agent_delete(&format!("/agent/services/{}", container_name))
            .await
    }

    /// Get the status of a service on the remote node.
    pub async fn service_status(
        &self,
        container_name: &str,
    ) -> Result<RemoteServiceStatus, ExternalServiceError> {
        self.agent_get(&format!("/agent/services/{}/status", container_name))
            .await
    }

    /// Run a command inside a service container on the remote node.
    /// Used for cluster admin operations (e.g. `pg_autoctl perform
    /// promotion`) that have to run from the container's perspective.
    /// Bounded admin commands only — never user input — see the
    /// CLAUDE.md note on docker exec safety.
    pub async fn exec_in_service(
        &self,
        params: RemoteExecParams,
    ) -> Result<RemoteExecResult, ExternalServiceError> {
        info!(
            "Exec in '{}' on remote node '{}': {:?}",
            params.container_name, self.node_name, params.command
        );
        self.agent_post("/agent/services/exec", &params).await
    }

    /// Initialize the provider and provision its per-project resource on the
    /// remote node. This keeps Docker and node-local TCP access inside the
    /// worker agent instead of accidentally using the control plane host.
    pub async fn get_runtime_env_vars(
        &self,
        request: RemoteRuntimeEnvRequest,
    ) -> Result<RemoteRuntimeEnvResponse, ExternalServiceError> {
        info!(
            service = %request.service_config.name,
            node = %self.node_name,
            "Provisioning external-service runtime environment on remote node"
        );
        self.agent_post("/agent/services/runtime-env", &request)
            .await
    }

    // -----------------------------------------------------------------------
    // HTTP helpers
    // -----------------------------------------------------------------------

    async fn agent_get<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
    ) -> Result<T, ExternalServiceError> {
        let url = format!("{}{}", self.agent_url, path);
        let response = self
            .client
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| ExternalServiceError::InternalError {
                reason: format!(
                    "Failed to reach agent on node {} at {}: {}",
                    self.node_name, url, e
                ),
            })?;

        let status = response.status();
        let body: AgentResponse<T> =
            response
                .json()
                .await
                .map_err(|e| ExternalServiceError::InternalError {
                    reason: format!(
                        "Invalid response from node {} at {}: {}",
                        self.node_name, url, e
                    ),
                })?;

        if !body.success {
            let err_msg = body.error.unwrap_or_default();
            error!(
                "Agent on node {} returned error ({}) at {}: {}",
                self.node_name, status, url, err_msg
            );
            return Err(ExternalServiceError::InternalError {
                reason: format!(
                    "Agent on node {} returned error ({}): {}",
                    self.node_name, status, err_msg
                ),
            });
        }

        body.data
            .ok_or_else(|| ExternalServiceError::InternalError {
                reason: format!(
                    "Agent on node {} returned success but no data at {}",
                    self.node_name, url
                ),
            })
    }

    async fn agent_post<B: serde::Serialize, T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, ExternalServiceError> {
        let url = format!("{}{}", self.agent_url, path);
        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .map_err(|e| ExternalServiceError::InternalError {
                reason: format!(
                    "Failed to reach agent on node {} at {}: {}",
                    self.node_name, url, e
                ),
            })?;

        let status = response.status();
        let body: AgentResponse<T> =
            response
                .json()
                .await
                .map_err(|e| ExternalServiceError::InternalError {
                    reason: format!(
                        "Invalid response from node {} at {}: {}",
                        self.node_name, url, e
                    ),
                })?;

        if !body.success {
            let err_msg = body.error.unwrap_or_default();
            error!(
                "Agent on node {} returned error ({}) at {}: {}",
                self.node_name, status, url, err_msg
            );
            return Err(ExternalServiceError::InternalError {
                reason: format!(
                    "Agent on node {} returned error ({}): {}",
                    self.node_name, status, err_msg
                ),
            });
        }

        body.data
            .ok_or_else(|| ExternalServiceError::InternalError {
                reason: format!(
                    "Agent on node {} returned success but no data at {}",
                    self.node_name, url
                ),
            })
    }

    async fn agent_post_no_body<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
    ) -> Result<T, ExternalServiceError> {
        let url = format!("{}{}", self.agent_url, path);
        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| ExternalServiceError::InternalError {
                reason: format!(
                    "Failed to reach agent on node {} at {}: {}",
                    self.node_name, url, e
                ),
            })?;

        let status = response.status();
        let body: AgentResponse<T> =
            response
                .json()
                .await
                .map_err(|e| ExternalServiceError::InternalError {
                    reason: format!(
                        "Invalid response from node {} at {}: {}",
                        self.node_name, url, e
                    ),
                })?;

        if !body.success {
            let err_msg = body.error.unwrap_or_default();
            return Err(ExternalServiceError::InternalError {
                reason: format!(
                    "Agent on node {} returned error ({}): {}",
                    self.node_name, status, err_msg
                ),
            });
        }

        body.data
            .ok_or_else(|| ExternalServiceError::InternalError {
                reason: format!(
                    "Agent on node {} returned success but no data at {}",
                    self.node_name, url
                ),
            })
    }

    async fn agent_delete(&self, path: &str) -> Result<(), ExternalServiceError> {
        let url = format!("{}{}", self.agent_url, path);
        let response = self
            .client
            .delete(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| ExternalServiceError::InternalError {
                reason: format!(
                    "Failed to reach agent on node {} at {}: {}",
                    self.node_name, url, e
                ),
            })?;

        let body: AgentResponse<serde_json::Value> =
            response
                .json()
                .await
                .map_err(|e| ExternalServiceError::InternalError {
                    reason: format!(
                        "Invalid response from node {} at {}: {}",
                        self.node_name, url, e
                    ),
                })?;

        if !body.success {
            let err_msg = body.error.unwrap_or_default();
            return Err(ExternalServiceError::InternalError {
                reason: format!(
                    "Agent on node {} returned error: {}",
                    self.node_name, err_msg
                ),
            });
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::externalsvc::ServiceType;
    use axum::{
        extract::Json,
        http::{header::AUTHORIZATION, HeaderMap},
        routing::post,
        Router,
    };

    #[test]
    fn test_remote_service_create_params_serialization() {
        let params = RemoteServiceCreateParams {
            name: "postgres-main".to_string(),
            service_type: "postgres".to_string(),
            image: "gotempsh/postgres-walg:18-bookworm".to_string(),
            environment: HashMap::from([
                ("POSTGRES_PASSWORD".to_string(), "secret".to_string()),
                ("POSTGRES_DB".to_string(), "mydb".to_string()),
            ]),
            port_mappings: vec![RemotePortMapping {
                host_port: 30001,
                container_port: 5432,
            }],
            volumes: HashMap::from([(
                "postgres-main_data".to_string(),
                "/var/lib/postgresql".to_string(),
            )]),
            network: Some("temps".to_string()),
            command: None,
            resource_limits: None,
        };

        let json = serde_json::to_string(&params).unwrap();
        assert!(json.contains("postgres-main"));
        assert!(json.contains("30001"));
        assert!(!json.contains("command")); // None fields skipped
        assert!(!json.contains("resource_limits")); // None field skipped
    }

    #[test]
    fn runtime_env_request_preserves_provider_config_and_tenant_identity() {
        let request = RemoteRuntimeEnvRequest {
            service_config: ServiceConfig {
                name: "orders-db".to_string(),
                service_type: ServiceType::Postgres,
                version: Some("18".to_string()),
                parameters: serde_json::json!({
                    "host": "localhost",
                    "port": "5432",
                    "password": "secret"
                }),
            },
            project_slug: "storefront".to_string(),
            environment_slug: "production".to_string(),
        };

        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(encoded["service_config"]["name"], "orders-db");
        assert_eq!(encoded["service_config"]["service_type"], "postgres");
        assert_eq!(encoded["service_config"]["parameters"]["port"], "5432");
        assert_eq!(encoded["project_slug"], "storefront");
        assert_eq!(encoded["environment_slug"], "production");
    }

    #[tokio::test]
    async fn runtime_env_posts_to_agent_and_returns_typed_environment() {
        async fn handler(
            headers: HeaderMap,
            Json(request): Json<RemoteRuntimeEnvRequest>,
        ) -> Json<serde_json::Value> {
            assert_eq!(
                headers
                    .get(AUTHORIZATION)
                    .and_then(|value| value.to_str().ok()),
                Some("Bearer node-token")
            );
            assert_eq!(request.service_config.name, "orders-db");
            assert_eq!(request.project_slug, "storefront");
            assert_eq!(request.environment_slug, "production");
            Json(serde_json::json!({
                "success": true,
                "data": {
                    "environment": {
                        "DATABASE_URL": "postgres://app:secret@postgres-orders-db:5432/storefront_production"
                    }
                },
                "error": null
            }))
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/agent/services/runtime-env", post(handler)),
            )
            .await
            .unwrap();
        });

        let client = RemoteServiceClient::new(
            format!("http://{address}"),
            "node-token".to_string(),
            "worker-1".to_string(),
        )
        .unwrap();
        let response = client
            .get_runtime_env_vars(RemoteRuntimeEnvRequest {
                service_config: ServiceConfig {
                    name: "orders-db".to_string(),
                    service_type: ServiceType::Postgres,
                    version: None,
                    parameters: serde_json::json!({"host": "localhost"}),
                },
                project_slug: "storefront".to_string(),
                environment_slug: "production".to_string(),
            })
            .await
            .unwrap();

        assert_eq!(
            response.environment.get("DATABASE_URL").map(String::as_str),
            Some("postgres://app:secret@postgres-orders-db:5432/storefront_production")
        );
        server.abort();
    }
}
