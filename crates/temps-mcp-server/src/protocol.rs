use serde::{Deserialize, Serialize};

// ─── JSON-RPC 2.0 error codes ───────────────────────────────────────────────

pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;

// ─── JSON-RPC wire types ─────────────────────────────────────────────────────

/// An incoming JSON-RPC 2.0 request or notification.
///
/// A notification has no `id` field. Notifications must not be responded to.
#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    /// Absent on notifications.
    #[serde(default)]
    pub id: Option<serde_json::Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<serde_json::Value>,
}

impl JsonRpcRequest {
    /// Returns `true` when this is a notification (no `id`).
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

/// A JSON-RPC 2.0 response sent back to the client.
#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    /// Mirrors the request `id`; absent when `id` was absent (notifications).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl JsonRpcResponse {
    /// Successful response.
    pub fn success(id: Option<serde_json::Value>, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Error response.
    pub fn error(id: Option<serde_json::Value>, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

// ─── MCP protocol types ──────────────────────────────────────────────────────

/// Definition of a single MCP tool as returned by `tools/list`.
#[derive(Debug, Clone, Serialize)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    /// JSON Schema describing the tool's input arguments.
    #[serde(rename = "inputSchema")]
    pub input_schema: serde_json::Value,
}

/// Minimal group descriptor for the `GET /mcp/tools` probe response.
#[derive(Debug, Clone, Serialize)]
pub struct ToolGroupInfo {
    pub key: &'static str,
    pub label: &'static str,
}

/// Response body for `GET /mcp/tools` (unauthenticated probe).
#[derive(Debug, Serialize)]
pub struct ToolsProbeResponse {
    pub groups: Vec<ToolGroupInfo>,
}

// ─── Query parameters ────────────────────────────────────────────────────────

/// Query-string parameters accepted by every MCP endpoint except the probe.
///
/// - `groups`: comma-separated list of group keys to scope tool exposure.
///   Absent or empty → all groups.
/// - `write`: `"1"` enables write tools; any other value (or absent) means
///   read-only.
#[derive(Debug, Default, Deserialize)]
pub struct McpQuery {
    pub groups: Option<String>,
    pub write: Option<String>,
}

impl McpQuery {
    /// Parses `groups` into a `Vec<String>`.  Returns an empty vec when the
    /// param is absent, which callers treat as "all groups".
    pub fn parsed_groups(&self) -> Vec<String> {
        self.groups
            .as_deref()
            .map(|s| {
                s.split(',')
                    .map(str::trim)
                    .filter(|g| !g.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Returns `true` when the caller has opted in to write tools.
    pub fn write_enabled(&self) -> bool {
        self.write.as_deref() == Some("1")
    }
}
