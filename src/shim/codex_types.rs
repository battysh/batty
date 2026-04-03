//! JSONL event types for Codex CLI `exec --json` mode.
//!
//! These types model the events emitted on stdout when Codex runs in
//! `codex exec --json` mode. Each line is a complete JSON object with
//! a `type` tag discriminating the event kind.

use serde::Deserialize;

// ---------------------------------------------------------------------------
// Top-level thread events (one per JSONL line)
// ---------------------------------------------------------------------------

/// A single JSONL event from `codex exec --json` stdout.
///
/// Uses a flat struct with `event_type` string + optional fields so that
/// unknown/future event types are silently tolerated.
#[derive(Debug, Deserialize)]
pub struct CodexEvent {
    #[serde(rename = "type")]
    pub event_type: String,

    // thread.started
    #[serde(default)]
    pub thread_id: Option<String>,

    // turn.completed
    #[serde(default)]
    pub usage: Option<CodexUsage>,

    // turn.failed / error
    #[serde(default)]
    pub error: Option<CodexError>,

    // item.started / item.updated / item.completed
    #[serde(default)]
    pub item: Option<CodexItem>,
}

#[derive(Debug, Deserialize)]
pub struct CodexUsage {
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub cached_input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
}

#[derive(Debug, Deserialize)]
pub struct CodexError {
    pub message: String,
}

// ---------------------------------------------------------------------------
// Thread items
// ---------------------------------------------------------------------------

/// A thread item carried by item.started / item.updated / item.completed events.
#[derive(Debug, Deserialize)]
pub struct CodexItem {
    pub id: String,
    #[serde(rename = "type")]
    pub item_type: String,

    // agent_message
    #[serde(default)]
    pub text: Option<String>,

    // command_execution
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub aggregated_output: Option<String>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub status: Option<String>,
}

impl CodexItem {
    /// Extract text from an agent_message or reasoning item.
    pub fn agent_text(&self) -> Option<&str> {
        if self.item_type == "agent_message" || self.item_type == "reasoning" {
            self.text.as_deref()
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Launch command builder
// ---------------------------------------------------------------------------

/// Build the Codex exec command for SDK (JSONL) mode.
///
/// `thread_id`: if provided, resumes an existing thread for multi-turn.
/// `prompt`: the message/task text to send.
pub fn codex_sdk_command(program: &str, prompt: &str, thread_id: Option<&str>) -> String {
    let escaped_prompt = prompt.replace('\'', "'\\''");
    match thread_id {
        Some(tid) => {
            let escaped_tid = tid.replace('\'', "'\\''");
            format!(
                "exec {program} exec --json --dangerously-bypass-approvals-and-sandbox resume '{escaped_tid}' '{escaped_prompt}'"
            )
        }
        None => {
            format!(
                "exec {program} exec --json --dangerously-bypass-approvals-and-sandbox '{escaped_prompt}'"
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_thread_started() {
        let line = r#"{"type":"thread.started","thread_id":"abc-123"}"#;
        let evt: CodexEvent = serde_json::from_str(line).unwrap();
        assert_eq!(evt.event_type, "thread.started");
        assert_eq!(evt.thread_id.as_deref(), Some("abc-123"));
    }

    #[test]
    fn parse_turn_started() {
        let line = r#"{"type":"turn.started"}"#;
        let evt: CodexEvent = serde_json::from_str(line).unwrap();
        assert_eq!(evt.event_type, "turn.started");
    }

    #[test]
    fn parse_turn_completed() {
        let line = r#"{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":50,"output_tokens":30}}"#;
        let evt: CodexEvent = serde_json::from_str(line).unwrap();
        assert_eq!(evt.event_type, "turn.completed");
        let usage = evt.usage.unwrap();
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 30);
    }

    #[test]
    fn parse_turn_failed() {
        let line = r#"{"type":"turn.failed","error":{"message":"rate limit"}}"#;
        let evt: CodexEvent = serde_json::from_str(line).unwrap();
        assert_eq!(evt.event_type, "turn.failed");
        assert_eq!(evt.error.unwrap().message, "rate limit");
    }

    #[test]
    fn parse_item_agent_message() {
        let line = r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"Hello world"}}"#;
        let evt: CodexEvent = serde_json::from_str(line).unwrap();
        assert_eq!(evt.event_type, "item.completed");
        let item = evt.item.unwrap();
        assert_eq!(item.item_type, "agent_message");
        assert_eq!(item.agent_text(), Some("Hello world"));
    }

    #[test]
    fn parse_item_command_execution() {
        let line = r#"{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"ls","aggregated_output":"file.txt\n","exit_code":0,"status":"completed"}}"#;
        let evt: CodexEvent = serde_json::from_str(line).unwrap();
        let item = evt.item.unwrap();
        assert_eq!(item.item_type, "command_execution");
        assert_eq!(item.command.as_deref(), Some("ls"));
        assert_eq!(item.exit_code, Some(0));
        assert!(item.agent_text().is_none());
    }

    #[test]
    fn parse_error_event() {
        let line = r#"{"type":"error","message":"fatal"}"#;
        // The flat struct puts message at top level for the error event type;
        // but the Codex format uses an error object. Let's handle both.
        let evt: CodexEvent = serde_json::from_str(line).unwrap();
        assert_eq!(evt.event_type, "error");
    }

    #[test]
    fn unknown_event_type_tolerated() {
        let line = r#"{"type":"future.event","some_field":42}"#;
        let evt: CodexEvent = serde_json::from_str(line).unwrap();
        assert_eq!(evt.event_type, "future.event");
    }

    #[test]
    fn codex_sdk_command_new_session() {
        let cmd = codex_sdk_command("codex", "fix the bug", None);
        assert!(cmd.contains("exec codex exec --json"));
        assert!(cmd.contains("--dangerously-bypass-approvals-and-sandbox"));
        assert!(cmd.contains("'fix the bug'"));
        assert!(!cmd.contains("resume"));
    }

    #[test]
    fn codex_sdk_command_resume() {
        let cmd = codex_sdk_command("codex", "next step", Some("tid-123"));
        assert!(cmd.contains("resume 'tid-123'"));
        assert!(cmd.contains("'next step'"));
    }

    #[test]
    fn codex_sdk_command_escapes_quotes() {
        let cmd = codex_sdk_command("codex", "fix user's bug", None);
        assert!(cmd.contains("user'\\''s"));
    }
}
