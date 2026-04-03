//! JSON-RPC 2.0 message types for the Kiro CLI Agent Client Protocol (ACP).
//!
//! These types model the messages exchanged over stdin/stdout when Kiro CLI
//! runs in `kiro-cli acp --trust-all-tools` mode. Each line is a complete
//! JSON-RPC 2.0 message (NDJSON transport).
//!
//! Protocol reference: <https://agentclientprotocol.com/protocol/overview>

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 request/response wrappers
// ---------------------------------------------------------------------------

/// A JSON-RPC 2.0 request (sent TO Kiro on stdin).
#[derive(Debug, Serialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl JsonRpcRequest {
    pub fn new(id: u64, method: &str, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method: method.to_string(),
            params,
        }
    }

    /// Serialize to a single NDJSON line (no trailing newline).
    pub fn to_ndjson(&self) -> String {
        serde_json::to_string(self).expect("JsonRpcRequest is always serializable")
    }
}

/// A JSON-RPC 2.0 response (sent TO Kiro on stdin, e.g. permission reply).
#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub result: Value,
}

impl JsonRpcResponse {
    pub fn new(id: u64, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result,
        }
    }

    pub fn to_ndjson(&self) -> String {
        serde_json::to_string(self).expect("JsonRpcResponse is always serializable")
    }
}

/// A single NDJSON message received from Kiro's stdout.
///
/// Can be a response (has `id` + `result`/`error`), a notification (has
/// `method` + `params` but no `id`), or a request from the agent (has
/// `id` + `method` + `params`, e.g. `session/request_permission`).
#[derive(Debug, Deserialize)]
pub struct AcpMessage {
    #[serde(default)]
    pub jsonrpc: Option<String>,

    /// Present on responses and agent-initiated requests.
    #[serde(default)]
    pub id: Option<u64>,

    /// Present on notifications and agent-initiated requests.
    #[serde(default)]
    pub method: Option<String>,

    /// Present on notifications and agent-initiated requests.
    #[serde(default)]
    pub params: Option<Value>,

    /// Present on successful responses.
    #[serde(default)]
    pub result: Option<Value>,

    /// Present on error responses.
    #[serde(default)]
    pub error: Option<Value>,
}

impl AcpMessage {
    /// Is this a response to a request we sent (has id + result/error, no method)?
    pub fn is_response(&self) -> bool {
        self.id.is_some() && self.method.is_none()
    }

    /// Is this a notification from the agent (has method, no id)?
    pub fn is_notification(&self) -> bool {
        self.method.is_some() && self.id.is_none()
    }

    /// Is this a request from the agent to us (has id + method)?
    pub fn is_agent_request(&self) -> bool {
        self.id.is_some() && self.method.is_some()
    }
}

// ---------------------------------------------------------------------------
// ACP-specific request builders
// ---------------------------------------------------------------------------

/// Build the `initialize` request.
pub fn initialize_request(id: u64) -> JsonRpcRequest {
    let params = serde_json::json!({
        "protocolVersion": 1,
        "clientCapabilities": {
            "fs": { "readTextFile": false, "writeTextFile": false },
            "terminal": false
        },
        "clientInfo": {
            "name": "batty",
            "version": env!("CARGO_PKG_VERSION")
        }
    });
    JsonRpcRequest::new(id, "initialize", Some(params))
}

/// Build the `session/new` request.
pub fn session_new_request(id: u64, cwd: &str) -> JsonRpcRequest {
    let params = serde_json::json!({
        "cwd": cwd,
        "mcpServers": []
    });
    JsonRpcRequest::new(id, "session/new", Some(params))
}

/// Build the `session/load` request (resume existing session).
pub fn session_load_request(id: u64, session_id: &str) -> JsonRpcRequest {
    let params = serde_json::json!({
        "sessionId": session_id
    });
    JsonRpcRequest::new(id, "session/load", Some(params))
}

/// Build a `session/prompt` request.
pub fn session_prompt_request(id: u64, session_id: &str, text: &str) -> JsonRpcRequest {
    let params = serde_json::json!({
        "sessionId": session_id,
        "prompt": [{ "type": "text", "text": text }]
    });
    JsonRpcRequest::new(id, "session/prompt", Some(params))
}

/// Build a `session/cancel` request.
pub fn session_cancel_request(id: u64, session_id: &str) -> JsonRpcRequest {
    let params = serde_json::json!({
        "sessionId": session_id
    });
    JsonRpcRequest::new(id, "session/cancel", Some(params))
}

/// Build a permission approval response for `session/request_permission`.
pub fn permission_approve_response(request_id: u64) -> JsonRpcResponse {
    JsonRpcResponse::new(
        request_id,
        serde_json::json!({
            "outcome": {
                "outcome": "selected",
                "optionId": "allow_once"
            }
        }),
    )
}

// ---------------------------------------------------------------------------
// Session update extraction helpers
// ---------------------------------------------------------------------------

/// Extract the `sessionUpdate` discriminator from a `session/update` params.
pub fn extract_update_type(params: &Value) -> Option<&str> {
    params
        .get("update")
        .and_then(|u| u.get("sessionUpdate"))
        .and_then(|v| v.as_str())
}

/// Extract text content from an `agent_message_chunk` update.
pub fn extract_message_chunk_text(params: &Value) -> Option<&str> {
    params
        .get("update")
        .and_then(|u| u.get("content"))
        .and_then(|c| c.get("text"))
        .and_then(|t| t.as_str())
}

/// Extract the session ID from a `session/new` or `session/load` response result.
pub fn extract_session_id(result: &Value) -> Option<&str> {
    result.get("sessionId").and_then(|v| v.as_str())
}

/// Extract the stop reason from a prompt result.
pub fn extract_stop_reason(result: &Value) -> Option<&str> {
    result.get("stopReason").and_then(|v| v.as_str())
}

/// Extract context usage percentage from `_kiro.dev/metadata` params.
pub fn extract_context_usage(params: &Value) -> Option<f64> {
    params
        .get("contextUsagePercentage")
        .and_then(|v| v.as_f64())
}

/// Build the kiro-cli ACP launch command.
///
/// `system_prompt`: optional system prompt passed via --agent config.
/// Returns the shell command to launch kiro-cli in ACP mode.
pub fn kiro_acp_command(program: &str) -> String {
    format!("exec {program} acp --trust-all-tools")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- Request serialization ---

    #[test]
    fn initialize_request_serializes() {
        let req = initialize_request(0);
        let json: Value = serde_json::from_str(&req.to_ndjson()).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], 0);
        assert_eq!(json["method"], "initialize");
        assert_eq!(json["params"]["protocolVersion"], 1);
        assert!(json["params"]["clientInfo"]["name"].as_str().is_some());
    }

    #[test]
    fn session_new_request_serializes() {
        let req = session_new_request(1, "/home/user/project");
        let json: Value = serde_json::from_str(&req.to_ndjson()).unwrap();
        assert_eq!(json["method"], "session/new");
        assert_eq!(json["params"]["cwd"], "/home/user/project");
    }

    #[test]
    fn session_load_request_serializes() {
        let req = session_load_request(1, "sess-abc");
        let json: Value = serde_json::from_str(&req.to_ndjson()).unwrap();
        assert_eq!(json["method"], "session/load");
        assert_eq!(json["params"]["sessionId"], "sess-abc");
    }

    #[test]
    fn session_prompt_request_serializes() {
        let req = session_prompt_request(2, "sess-abc", "Fix the bug");
        let json: Value = serde_json::from_str(&req.to_ndjson()).unwrap();
        assert_eq!(json["method"], "session/prompt");
        assert_eq!(json["params"]["sessionId"], "sess-abc");
        assert_eq!(json["params"]["prompt"][0]["type"], "text");
        assert_eq!(json["params"]["prompt"][0]["text"], "Fix the bug");
    }

    #[test]
    fn session_cancel_request_serializes() {
        let req = session_cancel_request(3, "sess-abc");
        let json: Value = serde_json::from_str(&req.to_ndjson()).unwrap();
        assert_eq!(json["method"], "session/cancel");
        assert_eq!(json["params"]["sessionId"], "sess-abc");
    }

    #[test]
    fn permission_approve_response_serializes() {
        let resp = permission_approve_response(5);
        let json: Value = serde_json::from_str(&resp.to_ndjson()).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], 5);
        assert_eq!(json["result"]["outcome"]["outcome"], "selected");
        assert_eq!(json["result"]["outcome"]["optionId"], "allow_once");
    }

    // --- AcpMessage deserialization ---

    #[test]
    fn parse_response_message() {
        let line =
            r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#;
        let msg: AcpMessage = serde_json::from_str(line).unwrap();
        assert!(msg.is_response());
        assert!(!msg.is_notification());
        assert!(!msg.is_agent_request());
        assert_eq!(msg.id, Some(0));
        assert!(msg.result.is_some());
    }

    #[test]
    fn parse_notification_message() {
        let line = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hello"}}}}"#;
        let msg: AcpMessage = serde_json::from_str(line).unwrap();
        assert!(msg.is_notification());
        assert!(!msg.is_response());
        assert_eq!(msg.method.as_deref(), Some("session/update"));
    }

    #[test]
    fn parse_agent_request_message() {
        let line = r#"{"jsonrpc":"2.0","id":5,"method":"session/request_permission","params":{"sessionId":"s1","toolCall":{"toolCallId":"c1","title":"Running: ls","kind":"execute"},"options":[{"optionId":"allow_once","name":"Yes"},{"optionId":"deny","name":"No"}]}}"#;
        let msg: AcpMessage = serde_json::from_str(line).unwrap();
        assert!(msg.is_agent_request());
        assert_eq!(msg.method.as_deref(), Some("session/request_permission"));
        assert_eq!(msg.id, Some(5));
    }

    #[test]
    fn parse_error_response() {
        let line =
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"invalid request"}}"#;
        let msg: AcpMessage = serde_json::from_str(line).unwrap();
        assert!(msg.is_response());
        assert!(msg.error.is_some());
        assert!(msg.result.is_none());
    }

    #[test]
    fn unknown_fields_tolerated() {
        let line = r#"{"jsonrpc":"2.0","method":"_kiro.dev/metadata","params":{"credits":8.5,"contextUsagePercentage":62.3,"future_field":true}}"#;
        let msg: AcpMessage = serde_json::from_str(line).unwrap();
        assert!(msg.is_notification());
    }

    // --- Extraction helpers ---

    #[test]
    fn extract_update_type_agent_message_chunk() {
        let params = serde_json::json!({
            "sessionId": "s1",
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "hello" }
            }
        });
        assert_eq!(extract_update_type(&params), Some("agent_message_chunk"));
    }

    #[test]
    fn extract_update_type_tool_call() {
        let params = serde_json::json!({
            "sessionId": "s1",
            "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": "c1",
                "title": "Reading file",
                "kind": "read",
                "status": "pending"
            }
        });
        assert_eq!(extract_update_type(&params), Some("tool_call"));
    }

    #[test]
    fn extract_update_type_turn_end() {
        let params = serde_json::json!({
            "sessionId": "s1",
            "update": { "sessionUpdate": "TurnEnd" }
        });
        assert_eq!(extract_update_type(&params), Some("TurnEnd"));
    }

    #[test]
    fn extract_message_chunk_text_present() {
        let params = serde_json::json!({
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "Here is the fix" }
            }
        });
        assert_eq!(extract_message_chunk_text(&params), Some("Here is the fix"));
    }

    #[test]
    fn extract_message_chunk_text_missing() {
        let params = serde_json::json!({
            "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": "c1"
            }
        });
        assert_eq!(extract_message_chunk_text(&params), None);
    }

    #[test]
    fn extract_session_id_from_result() {
        let result = serde_json::json!({"sessionId": "sess-xyz-123"});
        assert_eq!(extract_session_id(&result), Some("sess-xyz-123"));
    }

    #[test]
    fn extract_session_id_missing() {
        let result = serde_json::json!({"other": "field"});
        assert_eq!(extract_session_id(&result), None);
    }

    #[test]
    fn extract_context_usage_present() {
        let params = serde_json::json!({
            "credits": 8.5,
            "contextUsagePercentage": 62.3
        });
        assert_eq!(extract_context_usage(&params), Some(62.3));
    }

    #[test]
    fn extract_context_usage_missing() {
        let params = serde_json::json!({"credits": 8.5});
        assert_eq!(extract_context_usage(&params), None);
    }

    #[test]
    fn kiro_acp_command_format() {
        let cmd = kiro_acp_command("kiro-cli");
        assert_eq!(cmd, "exec kiro-cli acp --trust-all-tools");
    }

    #[test]
    fn kiro_acp_command_custom_binary() {
        let cmd = kiro_acp_command("/opt/kiro-cli");
        assert_eq!(cmd, "exec /opt/kiro-cli acp --trust-all-tools");
    }

    // --- JsonRpcRequest ---

    #[test]
    fn request_with_no_params() {
        let req = JsonRpcRequest::new(99, "ping", None);
        let json: Value = serde_json::from_str(&req.to_ndjson()).unwrap();
        assert_eq!(json["method"], "ping");
        assert!(json.get("params").is_none());
    }

    // --- JsonRpcResponse ---

    #[test]
    fn response_serializes() {
        let resp = JsonRpcResponse::new(42, serde_json::json!({"ok": true}));
        let json: Value = serde_json::from_str(&resp.to_ndjson()).unwrap();
        assert_eq!(json["id"], 42);
        assert_eq!(json["result"]["ok"], true);
    }
}
