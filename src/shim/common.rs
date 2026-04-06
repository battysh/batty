//! Shared utilities used by both PTY runtime and SDK runtime.

use std::collections::VecDeque;

use super::protocol::{Event, ShimState};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum number of messages that can be queued while the agent is working.
pub const MAX_QUEUE_DEPTH: usize = 16;

/// How often to report session stats (secs).
pub const SESSION_STATS_INTERVAL_SECS: u64 = 10;

// ---------------------------------------------------------------------------
// Message formatting
// ---------------------------------------------------------------------------

pub fn reply_target_for(sender: &str) -> &str {
    sender
}

pub fn format_injected_message(sender: &str, body: &str) -> String {
    let reply_to = reply_target_for(sender);
    format!(
        "--- Message from {sender} ---\n\
         Reply-To: {reply_to}\n\
         If you need to reply, use: batty send {reply_to} \"<your reply>\"\n\
         \n\
         {body}"
    )
}

// ---------------------------------------------------------------------------
// Queued message
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct QueuedMessage {
    pub from: String,
    pub body: String,
    pub message_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Queue drain helper
// ---------------------------------------------------------------------------

/// Drain all queued messages, emitting an Error event for each one.
/// Used when the agent enters a terminal state (Dead, ContextExhausted).
pub fn drain_queue_errors(
    queue: &mut VecDeque<QueuedMessage>,
    terminal_state: ShimState,
) -> Vec<Event> {
    let mut events = Vec::new();
    while let Some(msg) = queue.pop_front() {
        events.push(Event::Error {
            command: "SendMessage".into(),
            reason: format!(
                "agent entered {} state, queued message dropped{}",
                terminal_state,
                msg.message_id
                    .map(|id| format!(" (id: {id})"))
                    .unwrap_or_default(),
            ),
        });
    }
    events
}

// ---------------------------------------------------------------------------
// Context exhaustion detection
// ---------------------------------------------------------------------------

const EXHAUSTION_PATTERNS: &[&str] = &[
    "context window exceeded",
    "context window is full",
    "conversation is too long",
    "maximum context length",
    "context limit reached",
    "truncated due to context limit",
    "input exceeds the model",
    "prompt is too long",
];

/// Check if text contains known context exhaustion phrases.
pub fn detect_context_exhausted(text: &str) -> bool {
    let lower = text.to_lowercase();
    EXHAUSTION_PATTERNS.iter().any(|p| lower.contains(p))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_message_includes_sender_and_body() {
        let msg = format_injected_message("manager", "Fix the bug");
        assert!(msg.contains("Message from manager"));
        assert!(msg.contains("Reply-To: manager"));
        assert!(msg.contains("Fix the bug"));
        assert!(msg.contains("batty send manager"));
    }

    #[test]
    fn reply_target_is_identity() {
        assert_eq!(reply_target_for("architect"), "architect");
        assert_eq!(reply_target_for("eng-1-1"), "eng-1-1");
    }

    #[test]
    fn drain_queue_errors_empties_queue() {
        let mut queue = VecDeque::new();
        queue.push_back(QueuedMessage {
            from: "mgr".into(),
            body: "task 1".into(),
            message_id: Some("id-1".into()),
        });
        queue.push_back(QueuedMessage {
            from: "mgr".into(),
            body: "task 2".into(),
            message_id: None,
        });
        let events = drain_queue_errors(&mut queue, ShimState::Dead);
        assert_eq!(events.len(), 2);
        assert!(queue.is_empty());

        match &events[0] {
            Event::Error { reason, .. } => {
                assert!(reason.contains("dead"));
                assert!(reason.contains("id-1"));
            }
            _ => panic!("expected Error event"),
        }
    }

    #[test]
    fn drain_queue_empty_is_noop() {
        let mut queue = VecDeque::new();
        let events = drain_queue_errors(&mut queue, ShimState::Dead);
        assert!(events.is_empty());
    }

    #[test]
    fn context_exhaustion_detected() {
        assert!(detect_context_exhausted("Error: context window exceeded"));
        assert!(detect_context_exhausted(
            "The CONVERSATION IS TOO LONG to continue"
        ));
        assert!(detect_context_exhausted("maximum context length reached"));
    }

    #[test]
    fn context_exhaustion_not_detected_for_normal_text() {
        assert!(!detect_context_exhausted("Writing function to parse YAML"));
        assert!(!detect_context_exhausted("context manager initialized"));
    }
}
