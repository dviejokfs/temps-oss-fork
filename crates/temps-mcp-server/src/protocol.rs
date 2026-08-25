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

#[cfg(test)]
mod tests {
    use super::*;

    // ── JsonRpcRequest deserialization ────────────────────────────────────────

    #[test]
    fn valid_request_deserializes() {
        let json = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
        let req: JsonRpcRequest = serde_json::from_str(json).expect("must deserialize");
        assert_eq!(req.jsonrpc, "2.0");
        assert_eq!(req.method, "tools/list");
        assert!(!req.is_notification());
    }

    #[test]
    fn missing_method_fails_deserialization() {
        let json = r#"{"jsonrpc":"2.0","id":1}"#;
        assert!(
            serde_json::from_str::<JsonRpcRequest>(json).is_err(),
            "`method` is required; missing it must fail"
        );
    }

    #[test]
    fn missing_id_is_notification() {
        let json = r#"{"jsonrpc":"2.0","method":"notifications/cancelled"}"#;
        let req: JsonRpcRequest =
            serde_json::from_str(json).expect("notification (no id) must deserialize");
        assert!(req.is_notification());
    }

    #[test]
    fn wrong_typed_method_fails_deserialization() {
        // `method` must be a string; a number must be rejected.
        let json = r#"{"jsonrpc":"2.0","id":1,"method":42}"#;
        assert!(
            serde_json::from_str::<JsonRpcRequest>(json).is_err(),
            "numeric `method` must fail deserialization"
        );
    }

    #[test]
    fn empty_body_fails_deserialization() {
        // `{}` is missing the required `method` field.
        assert!(
            serde_json::from_str::<JsonRpcRequest>("{}").is_err(),
            "empty body must fail because `method` is required"
        );
    }

    #[test]
    fn extra_fields_are_allowed() {
        // MCP clients may send extra protocol fields (e.g. `_meta`).
        let json = r#"{"jsonrpc":"2.0","id":"abc","method":"initialize","_meta":{},"extra":true}"#;
        let req: JsonRpcRequest =
            serde_json::from_str(json).expect("extra fields must be tolerated");
        assert_eq!(req.method, "initialize");
        // id is a string here — serde_json::Value round-trips it.
        assert_eq!(req.id, Some(serde_json::json!("abc")));
    }

    // ── McpQuery::parsed_groups() edge cases ──────────────────────────────────

    #[test]
    fn parsed_groups_absent_returns_empty() {
        let q = McpQuery {
            groups: None,
            write: None,
        };
        assert!(q.parsed_groups().is_empty());
    }

    #[test]
    fn parsed_groups_empty_string_returns_empty() {
        let q = McpQuery {
            groups: Some(String::new()),
            write: None,
        };
        assert!(q.parsed_groups().is_empty());
    }

    #[test]
    fn parsed_groups_whitespace_only_returns_empty() {
        let q = McpQuery {
            groups: Some("  ,  , ".to_string()),
            write: None,
        };
        assert!(
            q.parsed_groups().is_empty(),
            "whitespace-only entries must be filtered out"
        );
    }

    #[test]
    fn parsed_groups_trims_whitespace() {
        let q = McpQuery {
            groups: Some(" platform , deployments ".to_string()),
            write: None,
        };
        let groups = q.parsed_groups();
        assert_eq!(groups, vec!["platform", "deployments"]);
    }

    // ── McpQuery::write_enabled() edge cases ─────────────────────────────────

    #[test]
    fn write_enabled_absent_is_false() {
        let q = McpQuery {
            groups: None,
            write: None,
        };
        assert!(!q.write_enabled());
    }

    #[test]
    fn write_enabled_zero_is_false() {
        let q = McpQuery {
            groups: None,
            write: Some("0".to_string()),
        };
        assert!(!q.write_enabled(), "write=0 must not enable write mode");
    }

    #[test]
    fn write_enabled_one_is_true() {
        let q = McpQuery {
            groups: None,
            write: Some("1".to_string()),
        };
        assert!(q.write_enabled());
    }

    #[test]
    fn write_enabled_arbitrary_value_is_false() {
        // Only the exact string "1" enables write mode.
        let q = McpQuery {
            groups: None,
            write: Some("true".to_string()),
        };
        assert!(!q.write_enabled(), "write=true must not enable write mode");
    }

    // ── MINOR #12 — jsonrpc version validation (exercised in dispatch) ────────
    // The actual runtime check is in dispatch() in handlers.rs.  Here we verify
    // that a request with a non-"2.0" version field still deserializes (the
    // field is present, just wrong), so the version check must happen at the
    // dispatch layer, not at serde time.

    #[test]
    fn wrong_jsonrpc_version_still_deserializes() {
        let json = r#"{"jsonrpc":"1.0","id":1,"method":"initialize"}"#;
        let req: JsonRpcRequest =
            serde_json::from_str(json).expect("wrong version must still deserialize");
        assert_eq!(req.jsonrpc, "1.0");
        // dispatch() will reject this with InvalidJsonRpcVersion.
    }
}
