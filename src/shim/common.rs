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

/// Maximum number of automatic fix-and-retest loops before escalation.
pub const TEST_FAILURE_MAX_ITERATIONS: u8 = 5;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestFailureFollowup {
    pub body: String,
    pub notice: String,
    pub next_iteration_count: u8,
    pub escalate: bool,
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

/// Check if text contains known context exhaustion phrases (hard failure).
pub fn detect_context_exhausted(text: &str) -> bool {
    let lower = text.to_lowercase();
    EXHAUSTION_PATTERNS.iter().any(|p| lower.contains(p))
}

// ---------------------------------------------------------------------------
// Context approaching-limit detection (early warning)
// ---------------------------------------------------------------------------

const CONTEXT_APPROACHING_PATTERNS: &[&str] = &[
    "automatically compress prior messages",
    "context window is approaching",
    "approaching context limit",
    "context is getting large",
    "conversation history has been compressed",
    "messages were compressed",
    "running low on context",
    "nearing the context limit",
];

/// Check if text contains signals that the context window is under pressure
/// but not yet fully exhausted. Returns true for early-warning patterns
/// (e.g. automatic compression notifications from Claude).
pub fn detect_context_approaching_limit(text: &str) -> bool {
    let lower = text.to_lowercase();
    CONTEXT_APPROACHING_PATTERNS
        .iter()
        .any(|p| lower.contains(p))
}

const TEST_COMMAND_PATTERNS: &[&str] = &[
    "cargo test",
    "cargo nextest",
    "pytest",
    "npm test",
    "pnpm test",
    "yarn test",
    "go test",
    "bundle exec rspec",
    "mix test",
    "test result:",
];

const TEST_FAILURE_PATTERNS: &[&str] = &[
    "test result: failed",
    "error: test failed",
    "failing tests:",
    "failures:",
    "failures (",
    "test failed, to rerun pass",
];

/// Check if text looks like a real test runner failure instead of a prose mention.
pub fn detect_test_failure(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let has_test_context = TEST_COMMAND_PATTERNS
        .iter()
        .any(|pattern| lower.contains(pattern));
    if !has_test_context {
        return false;
    }

    TEST_FAILURE_PATTERNS
        .iter()
        .any(|pattern| lower.contains(pattern))
        || lower
            .lines()
            .map(str::trim)
            .any(|line| line.starts_with("test ") && line.ends_with("... failed"))
}

pub fn detect_test_failure_followup(
    text: &str,
    iteration_count: u8,
) -> Option<TestFailureFollowup> {
    if !detect_test_failure(text) {
        return None;
    }

    if iteration_count < TEST_FAILURE_MAX_ITERATIONS {
        let attempt = iteration_count + 1;
        return Some(TestFailureFollowup {
            body: format!(
                "tests failed — fix and retest before reporting completion.\n\
                 Attempt {attempt}/{TEST_FAILURE_MAX_ITERATIONS}.\n\
                 Re-run cargo test after fixing the failures.\n\
                 Do not send a completion packet unless tests_passed=true."
            ),
            notice: format!(
                "tests failed — fix and retest (attempt {attempt}/{TEST_FAILURE_MAX_ITERATIONS})"
            ),
            next_iteration_count: attempt,
            escalate: false,
        });
    }

    Some(TestFailureFollowup {
        body: format!(
            "tests failed — fix and retest loop exhausted after \
             {TEST_FAILURE_MAX_ITERATIONS} attempts.\n\
             Stop reporting completion, send a blocker or escalation with the failing \
             test summary, and wait for direction."
        ),
        notice: format!(
            "tests failed repeatedly — escalation required after \
             {TEST_FAILURE_MAX_ITERATIONS} attempts"
        ),
        next_iteration_count: TEST_FAILURE_MAX_ITERATIONS,
        escalate: true,
    })
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

    #[test]
    fn context_approaching_limit_detected() {
        assert!(detect_context_approaching_limit(
            "The system will automatically compress prior messages in your conversation"
        ));
        assert!(detect_context_approaching_limit(
            "conversation history has been compressed to save space"
        ));
        assert!(detect_context_approaching_limit(
            "Context window is approaching its maximum capacity"
        ));
    }

    #[test]
    fn context_approaching_not_detected_for_normal_text() {
        assert!(!detect_context_approaching_limit(
            "context manager initialized"
        ));
        assert!(!detect_context_approaching_limit(
            "compression algorithm works"
        ));
    }

    #[test]
    fn test_failure_detected_for_cargo_test_output() {
        let output = "running 2 tests\n\
                      test foo::bar ... FAILED\n\
                      failures:\n\
                      test result: FAILED. 1 passed; 1 failed; 0 ignored;";
        assert!(detect_test_failure(output));
    }

    #[test]
    fn test_failure_not_detected_for_prompt_text_only() {
        assert!(!detect_test_failure(
            "tests failed — fix and retest before reporting completion"
        ));
    }

    #[test]
    fn test_failure_followup_retries_before_escalating() {
        let output = "cargo test\nfailures:\ntest result: FAILED.";
        let followup = detect_test_failure_followup(output, 0).expect("followup");
        assert!(!followup.escalate);
        assert_eq!(followup.next_iteration_count, 1);
        assert!(followup.notice.contains("attempt 1/5"));

        let escalation =
            detect_test_failure_followup(output, TEST_FAILURE_MAX_ITERATIONS).expect("escalation");
        assert!(escalation.escalate);
        assert_eq!(escalation.next_iteration_count, TEST_FAILURE_MAX_ITERATIONS);
        assert!(escalation.notice.contains("escalation required"));
    }
}
