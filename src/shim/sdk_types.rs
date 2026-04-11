//! NDJSON message types for the Claude Code stream-json SDK protocol.
//!
//! These types model the messages exchanged over stdin/stdout when Claude Code
//! runs in `-p --input-format=stream-json --output-format=stream-json` mode.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Messages written TO Claude's stdin
// ---------------------------------------------------------------------------

/// A user message sent to Claude Code via stdin.
#[derive(Debug, Serialize)]
pub struct SdkUserMessage {
    #[serde(rename = "type")]
    pub msg_type: &'static str, // always "user"
    pub session_id: String,
    pub message: UserMessageBody,
    pub parent_tool_use_id: Option<String>,
}

impl SdkUserMessage {
    pub fn new(session_id: &str, content: &str) -> Self {
        Self {
            msg_type: "user",
            session_id: session_id.to_string(),
            message: UserMessageBody {
                role: "user".to_string(),
                content: content.to_string(),
            },
            parent_tool_use_id: None,
        }
    }

    /// Serialize to a single NDJSON line (no trailing newline).
    pub fn to_ndjson(&self) -> String {
        serde_json::to_string(self).expect("SdkUserMessage is always serializable")
    }
}

#[derive(Debug, Serialize)]
pub struct UserMessageBody {
    pub role: String,
    pub content: String,
}

/// A control response sent to Claude Code via stdin (e.g. permission approval).
#[derive(Debug, Serialize)]
pub struct SdkControlResponse {
    #[serde(rename = "type")]
    pub msg_type: &'static str, // always "control_response"
    pub response: ControlResponseBody,
}

impl SdkControlResponse {
    /// Build an approval response for a `can_use_tool` control request.
    pub fn approve_tool(request_id: &str, tool_use_id: &str) -> Self {
        Self {
            msg_type: "control_response",
            response: ControlResponseBody {
                subtype: "success".to_string(),
                request_id: request_id.to_string(),
                response: Some(ToolApproval {
                    tool_use_id: tool_use_id.to_string(),
                    approved: true,
                }),
            },
        }
    }

    /// Serialize to a single NDJSON line (no trailing newline).
    pub fn to_ndjson(&self) -> String {
        serde_json::to_string(self).expect("SdkControlResponse is always serializable")
    }
}

#[derive(Debug, Serialize)]
pub struct ControlResponseBody {
    pub subtype: String,
    pub request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<ToolApproval>,
}

#[derive(Debug, Serialize)]
pub struct ToolApproval {
    #[serde(rename = "toolUseID")]
    pub tool_use_id: String,
    pub approved: bool,
}

// ---------------------------------------------------------------------------
// Messages read FROM Claude's stdout
// ---------------------------------------------------------------------------

/// A single NDJSON message received from Claude Code's stdout.
///
/// Uses a flat struct with optional fields rather than a tagged enum so that
/// unknown or new message types are silently tolerated (future-proof).
#[derive(Debug, Deserialize)]
pub struct SdkOutput {
    #[serde(rename = "type")]
    pub msg_type: String,

    #[serde(default)]
    pub subtype: Option<String>,

    #[serde(default)]
    pub session_id: Option<String>,

    #[serde(default)]
    pub uuid: Option<String>,

    /// For `assistant` messages: the full message object with `content` array.
    #[serde(default)]
    pub message: Option<Value>,

    /// For `stream_event` messages: the stream event payload.
    #[serde(default)]
    pub event: Option<Value>,

    /// For `result` messages: the final text result.
    #[serde(default)]
    pub result: Option<String>,

    /// For `result` messages: number of API turns taken.
    #[serde(default)]
    pub num_turns: Option<u32>,

    /// For `result` messages: whether an error occurred.
    #[serde(default)]
    pub is_error: Option<bool>,

    /// For `result` error messages: list of error strings.
    #[serde(default)]
    pub errors: Option<Vec<String>>,

    /// For `result` messages: usage counters for the completed turn.
    #[serde(default)]
    pub usage: Option<Value>,

    /// For `result` messages: model-specific metadata.
    #[serde(rename = "modelUsage", default)]
    pub model_usage: Option<Value>,

    /// For `control_request` messages: the request ID to echo in responses.
    #[serde(default)]
    pub request_id: Option<String>,

    /// For `control_request` messages: the request payload.
    #[serde(default)]
    pub request: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SdkTokenUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
}

impl SdkTokenUsage {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens
            + self.cached_input_tokens
            + self.cache_creation_input_tokens
            + self.cache_read_input_tokens
            + self.output_tokens
            + self.reasoning_output_tokens
    }
}

impl SdkOutput {
    /// Extract the `subtype` from a nested `request` object (for control requests).
    pub fn request_subtype(&self) -> Option<String> {
        self.request
            .as_ref()
            .and_then(|r| r.get("subtype"))
            .and_then(|v| v.as_str())
            .map(String::from)
    }

    /// Extract the `tool_use_id` from a `can_use_tool` control request.
    pub fn request_tool_use_id(&self) -> Option<String> {
        self.request
            .as_ref()
            .and_then(|r| r.get("tool_use_id"))
            .and_then(|v| v.as_str())
            .map(String::from)
    }

    pub fn model_name(&self) -> Option<String> {
        self.message
            .as_ref()
            .and_then(|message| message.get("model"))
            .and_then(|value| value.as_str())
            .map(String::from)
            .or_else(|| {
                self.model_usage
                    .as_ref()
                    .and_then(|value| value.get("model"))
                    .and_then(|value| value.as_str())
                    .map(String::from)
            })
    }

    pub fn token_usage(&self) -> Option<SdkTokenUsage> {
        let usage = self.usage.as_ref()?;
        let cache_creation = usage.get("cache_creation");
        let cache_creation_classified =
            json_u64(cache_creation.and_then(|value| value.get("ephemeral_5m_input_tokens")))
                + json_u64(cache_creation.and_then(|value| value.get("ephemeral_1h_input_tokens")));
        Some(SdkTokenUsage {
            input_tokens: json_u64(usage.get("input_tokens")),
            cached_input_tokens: json_u64(usage.get("cached_input_tokens")),
            cache_creation_input_tokens: json_u64(usage.get("cache_creation_input_tokens"))
                .max(cache_creation_classified),
            cache_read_input_tokens: json_u64(usage.get("cache_read_input_tokens")),
            output_tokens: json_u64(usage.get("output_tokens")),
            reasoning_output_tokens: json_u64(usage.get("reasoning_output_tokens")),
        })
    }

    pub fn usage_total_tokens(&self) -> u64 {
        // Delegate to `token_usage()` so the total uses the same de-duped
        // cache-creation accounting as every other consumer. The prior
        // bespoke implementation summed `cache_creation_input_tokens` AND
        // `ephemeral_5m_input_tokens` AND `ephemeral_1h_input_tokens`, even
        // though the first field is already the sum of the latter two in
        // Claude's stream-json reporting — so cached prompts on the 1M
        // model showed ~2x inflated totals, pushing healthy agents past
        // the context-pressure threshold on every turn. See
        // `token_usage` above for the canonical `.max()` de-dup.
        self.token_usage()
            .map(|usage| usage.total_tokens())
            .unwrap_or(0)
    }
}

fn json_u64(value: Option<&Value>) -> u64 {
    value.and_then(Value::as_u64).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Text extraction helpers
// ---------------------------------------------------------------------------

/// Extract text content from an `assistant` message's `content` array.
///
/// The `message` field contains `{ "role": "assistant", "content": [...] }`
/// where each content block may be `{ "type": "text", "text": "..." }` or
/// a tool_use block. We only extract text blocks.
pub fn extract_assistant_text(message: &Value) -> String {
    let content = match message.get("content") {
        Some(Value::Array(arr)) => arr,
        Some(Value::String(s)) => return s.clone(),
        _ => return String::new(),
    };

    let mut parts = Vec::new();
    for block in content {
        if block.get("type").and_then(|t| t.as_str()) == Some("text") {
            if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                parts.push(text);
            }
        }
    }
    parts.join("")
}

/// Extract incremental text from a `stream_event` payload.
///
/// Stream events contain `{ "type": "content_block_delta", "delta": { "type": "text_delta", "text": "..." } }`
/// or similar structures. We extract the text delta if present.
pub fn extract_stream_text(event: &Value) -> Option<String> {
    // content_block_delta with text_delta
    if let Some(delta) = event.get("delta") {
        if delta.get("type").and_then(|t| t.as_str()) == Some("text_delta") {
            return delta.get("text").and_then(|t| t.as_str()).map(String::from);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- SdkUserMessage ---

    #[test]
    fn user_message_serializes_correctly() {
        let msg = SdkUserMessage::new("sess-1", "Fix the bug");
        let json: Value = serde_json::from_str(&msg.to_ndjson()).unwrap();
        assert_eq!(json["type"], "user");
        assert_eq!(json["session_id"], "sess-1");
        assert_eq!(json["message"]["role"], "user");
        assert_eq!(json["message"]["content"], "Fix the bug");
        assert!(json["parent_tool_use_id"].is_null());
    }

    #[test]
    fn user_message_empty_session_id() {
        let msg = SdkUserMessage::new("", "hello");
        let json: Value = serde_json::from_str(&msg.to_ndjson()).unwrap();
        assert_eq!(json["session_id"], "");
    }

    // --- SdkControlResponse ---

    #[test]
    fn approve_tool_serializes_correctly() {
        let resp = SdkControlResponse::approve_tool("req-42", "tool-99");
        let json: Value = serde_json::from_str(&resp.to_ndjson()).unwrap();
        assert_eq!(json["type"], "control_response");
        assert_eq!(json["response"]["subtype"], "success");
        assert_eq!(json["response"]["request_id"], "req-42");
        assert_eq!(json["response"]["response"]["toolUseID"], "tool-99");
        assert_eq!(json["response"]["response"]["approved"], true);
    }

    // --- SdkOutput deserialization ---

    #[test]
    fn parse_assistant_message() {
        let line = r#"{"type":"assistant","session_id":"abc","uuid":"u1","message":{"role":"assistant","content":[{"type":"text","text":"hello world"}]}}"#;
        let msg: SdkOutput = serde_json::from_str(line).unwrap();
        assert_eq!(msg.msg_type, "assistant");
        assert_eq!(msg.session_id.as_deref(), Some("abc"));
        assert!(msg.message.is_some());
    }

    #[test]
    fn parse_result_token_usage_includes_cache_fields() {
        let line = r#"{"type":"result","usage":{"input_tokens":10,"cached_input_tokens":4,"cache_creation_input_tokens":3,"cache_read_input_tokens":2,"output_tokens":5,"reasoning_output_tokens":1,"cache_creation":{"ephemeral_5m_input_tokens":7,"ephemeral_1h_input_tokens":11}}}"#;
        let msg: SdkOutput = serde_json::from_str(line).unwrap();
        let usage = msg.token_usage().unwrap();
        assert_eq!(
            usage,
            SdkTokenUsage {
                input_tokens: 10,
                cached_input_tokens: 4,
                cache_creation_input_tokens: 18,
                cache_read_input_tokens: 2,
                output_tokens: 5,
                reasoning_output_tokens: 1,
            }
        );
        assert_eq!(usage.total_tokens(), 40);
    }

    #[test]
    fn model_name_prefers_message_then_model_usage() {
        let assistant_line = r#"{"type":"assistant","message":{"model":"claude-sonnet-4-5","content":[{"type":"text","text":"hello"}]}}"#;
        let assistant: SdkOutput = serde_json::from_str(assistant_line).unwrap();
        assert_eq!(assistant.model_name().as_deref(), Some("claude-sonnet-4-5"));

        let result_line = r#"{"type":"result","modelUsage":{"model":"claude-opus-4-1"}}"#;
        let result: SdkOutput = serde_json::from_str(result_line).unwrap();
        assert_eq!(result.model_name().as_deref(), Some("claude-opus-4-1"));
    }

    #[test]
    fn parse_result_success() {
        let line = r#"{"type":"result","subtype":"success","session_id":"abc","uuid":"u2","result":"done","num_turns":3,"is_error":false}"#;
        let msg: SdkOutput = serde_json::from_str(line).unwrap();
        assert_eq!(msg.msg_type, "result");
        assert_eq!(msg.subtype.as_deref(), Some("success"));
        assert_eq!(msg.result.as_deref(), Some("done"));
        assert_eq!(msg.num_turns, Some(3));
        assert_eq!(msg.is_error, Some(false));
    }

    #[test]
    fn parse_result_error() {
        let line = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["context window exceeded"],"session_id":"x"}"#;
        let msg: SdkOutput = serde_json::from_str(line).unwrap();
        assert_eq!(msg.msg_type, "result");
        assert_eq!(msg.is_error, Some(true));
        assert_eq!(
            msg.errors.as_deref(),
            Some(&["context window exceeded".to_string()][..])
        );
    }

    #[test]
    fn parse_control_request() {
        let line = r#"{"type":"control_request","request_id":"req-1","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"tu-1","input":{"command":"ls"}}}"#;
        let msg: SdkOutput = serde_json::from_str(line).unwrap();
        assert_eq!(msg.msg_type, "control_request");
        assert_eq!(msg.request_id.as_deref(), Some("req-1"));
        assert_eq!(msg.request_subtype().as_deref(), Some("can_use_tool"));
        assert_eq!(msg.request_tool_use_id().as_deref(), Some("tu-1"));
    }

    #[test]
    fn parse_result_usage_and_model() {
        let line = r#"{"type":"result","session_id":"x","usage":{"input_tokens":10,"cached_input_tokens":5,"cache_creation_input_tokens":20,"cache_creation":{"ephemeral_5m_input_tokens":5,"ephemeral_1h_input_tokens":15},"cache_read_input_tokens":3,"output_tokens":7,"reasoning_output_tokens":2},"message":{"model":"claude-opus-4-6-1m"}}"#;
        let msg: SdkOutput = serde_json::from_str(line).unwrap();
        assert_eq!(msg.model_name().as_deref(), Some("claude-opus-4-6-1m"));
        assert_eq!(msg.usage_total_tokens(), 67);
    }

    #[test]
    fn parse_stream_event() {
        let line = r#"{"type":"stream_event","session_id":"abc","uuid":"u3","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}}"#;
        let msg: SdkOutput = serde_json::from_str(line).unwrap();
        assert_eq!(msg.msg_type, "stream_event");
        assert!(msg.event.is_some());
    }

    #[test]
    fn unknown_message_type_is_tolerated() {
        let line = r#"{"type":"future_new_type","session_id":"abc","some_field":42}"#;
        let msg: SdkOutput = serde_json::from_str(line).unwrap();
        assert_eq!(msg.msg_type, "future_new_type");
    }

    // --- Text extraction ---

    #[test]
    fn extract_text_from_content_array() {
        let msg = json!({
            "role": "assistant",
            "content": [
                {"type": "text", "text": "Hello "},
                {"type": "tool_use", "id": "t1", "name": "Bash", "input": {}},
                {"type": "text", "text": "world"}
            ]
        });
        assert_eq!(extract_assistant_text(&msg), "Hello world");
    }

    #[test]
    fn extract_text_from_string_content() {
        let msg = json!({"role": "assistant", "content": "plain string"});
        assert_eq!(extract_assistant_text(&msg), "plain string");
    }

    #[test]
    fn extract_text_empty_content() {
        let msg = json!({"role": "assistant", "content": []});
        assert_eq!(extract_assistant_text(&msg), "");
    }

    #[test]
    fn extract_text_no_content_field() {
        let msg = json!({"role": "assistant"});
        assert_eq!(extract_assistant_text(&msg), "");
    }

    #[test]
    fn extract_text_only_tool_use_blocks() {
        let msg = json!({
            "content": [
                {"type": "tool_use", "id": "t1", "name": "Read", "input": {}}
            ]
        });
        assert_eq!(extract_assistant_text(&msg), "");
    }

    #[test]
    fn extract_stream_text_delta() {
        let event = json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "incremental"}
        });
        assert_eq!(extract_stream_text(&event), Some("incremental".to_string()));
    }

    #[test]
    fn extract_stream_text_non_text_delta() {
        let event = json!({
            "type": "content_block_delta",
            "delta": {"type": "input_json_delta", "partial_json": "{}"}
        });
        assert_eq!(extract_stream_text(&event), None);
    }

    #[test]
    fn extract_stream_text_no_delta() {
        let event = json!({"type": "content_block_start"});
        assert_eq!(extract_stream_text(&event), None);
    }
}
