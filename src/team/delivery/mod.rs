mod routing;
mod telegram;
mod verification;

use std::time::{Duration, Instant};

use crate::tmux;

pub(super) const DELIVERY_VERIFICATION_CAPTURE_LINES: u32 = 50;
/// Increased capture window for agents that recently became ready, to account
/// for startup output pushing the delivery marker further up the scrollback.
#[allow(dead_code)]
pub(super) const DELIVERY_VERIFICATION_CAPTURE_LINES_RECENTLY_READY: u32 = 100;
pub(super) const FAILED_DELIVERY_RETRY_DELAY: Duration = Duration::from_secs(30);
pub(super) const FAILED_DELIVERY_MAX_ATTEMPTS: u32 = 3;

/// Check whether an agent's pane is showing a ready prompt by capturing
/// the last 20 lines and looking for known agent input indicators.
pub(super) fn is_agent_ready(pane_id: &str) -> bool {
    match tmux::capture_pane_recent(pane_id, 20) {
        Ok(capture) => super::watcher::is_at_agent_prompt(&capture),
        Err(_) => false,
    }
}

#[derive(Debug, Clone)]
pub(super) struct PendingMessage {
    pub(super) from: String,
    pub(super) body: String,
    #[allow(dead_code)] // Useful for future queue-age diagnostics.
    pub(super) queued_at: Instant,
}

#[derive(Debug, Clone)]
pub(super) struct FailedDelivery {
    pub(super) recipient: String,
    pub(super) from: String,
    pub(super) body: String,
    pub(super) attempts: u32,
    pub(super) repeated_failures: u32,
    pub(super) last_attempt: Instant,
}

impl FailedDelivery {
    pub(super) fn new(recipient: &str, from: &str, body: &str) -> Self {
        Self {
            recipient: recipient.to_string(),
            from: from.to_string(),
            body: body.to_string(),
            attempts: 1,
            repeated_failures: 1,
            last_attempt: Instant::now(),
        }
    }

    pub(super) fn message_marker(&self) -> String {
        message_delivery_marker(&self.from)
    }

    fn is_ready_for_retry(&self, now: Instant) -> bool {
        now.duration_since(self.last_attempt) >= FAILED_DELIVERY_RETRY_DELAY
    }

    fn has_attempts_remaining(&self) -> bool {
        self.attempts < FAILED_DELIVERY_MAX_ATTEMPTS
    }

    fn mark_retry_attempt(&mut self, now: Instant) {
        self.attempts = self.attempts.saturating_add(1);
        self.last_attempt = now;
    }

    fn record_repeat_failure(&mut self) {
        self.repeated_failures = self.repeated_failures.saturating_add(1);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FailedDeliveryEscalationReason {
    MissingShim,
    NotReady,
    PermanentFailure,
}

impl FailedDeliveryEscalationReason {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::MissingShim => "missing_shim",
            Self::NotReady => "not_ready",
            Self::PermanentFailure => "permanent_failure",
        }
    }

    pub(super) fn operator_summary(self) -> &'static str {
        match self {
            Self::MissingShim => "recipient has no live shim handle",
            Self::NotReady => "recipient never became ready for recovery delivery",
            Self::PermanentFailure => "shim delivery kept failing for the recipient",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MessageDelivery {
    Channel,
    LivePane,
    OrchestratorLogged,
    InboxQueued,
    DeferredPending,
    SkippedUnknownRecipient,
}

pub(super) fn message_delivery_marker(sender: &str) -> String {
    format!("--- Message from {sender} ---")
}

pub(super) fn capture_contains_message_marker(capture: &str, message_marker: &str) -> bool {
    capture.contains(message_marker)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_confirm_marker_detection_matches_captured_text() {
        let marker = message_delivery_marker("manager");
        let capture = format!("prompt\n{marker}\nbody\n");
        assert!(capture_contains_message_marker(&capture, &marker));
        assert!(!capture_contains_message_marker("prompt only", &marker));
    }

    #[test]
    fn delivery_confirm_marker_generation_uses_sender_header() {
        assert_eq!(
            message_delivery_marker("eng-1-4"),
            "--- Message from eng-1-4 ---"
        );
    }

    #[test]
    fn failed_delivery_new_sets_expected_fields() {
        let delivery = FailedDelivery::new("eng-1", "manager", "Please retry this.");
        assert_eq!(delivery.recipient, "eng-1");
        assert_eq!(delivery.from, "manager");
        assert_eq!(delivery.body, "Please retry this.");
        assert_eq!(delivery.attempts, 1);
        assert_eq!(delivery.repeated_failures, 1);
        assert_eq!(delivery.message_marker(), "--- Message from manager ---");
        assert!(delivery.has_attempts_remaining());
    }

    #[test]
    fn message_delivery_variants_are_distinct() {
        assert_ne!(MessageDelivery::Channel, MessageDelivery::LivePane);
        assert_ne!(MessageDelivery::LivePane, MessageDelivery::InboxQueued);
        assert_ne!(
            MessageDelivery::LivePane,
            MessageDelivery::OrchestratorLogged
        );
        assert_ne!(
            MessageDelivery::InboxQueued,
            MessageDelivery::OrchestratorLogged
        );
        assert_ne!(
            MessageDelivery::OrchestratorLogged,
            MessageDelivery::SkippedUnknownRecipient
        );
        assert_eq!(MessageDelivery::Channel, MessageDelivery::Channel);
    }

    #[test]
    fn delivery_verification_constants_are_sane() {
        const {
            assert!(
                DELIVERY_VERIFICATION_CAPTURE_LINES_RECENTLY_READY
                    > DELIVERY_VERIFICATION_CAPTURE_LINES
            );
            assert!(DELIVERY_VERIFICATION_CAPTURE_LINES > 0);
            assert!(FAILED_DELIVERY_MAX_ATTEMPTS >= 2);
        }
        assert!(FAILED_DELIVERY_RETRY_DELAY >= Duration::from_secs(1));
    }

    #[test]
    fn is_agent_ready_returns_false_for_nonexistent_pane() {
        assert!(!is_agent_ready("%99999999"));
    }

    // --- FailedDelivery struct ---

    #[test]
    fn failed_delivery_is_not_ready_for_retry_when_recent() {
        let delivery = FailedDelivery::new("eng-1", "manager", "test");
        // Just created — last_attempt is now, so not ready for retry
        assert!(!delivery.is_ready_for_retry(Instant::now()));
    }

    #[test]
    fn failed_delivery_is_ready_for_retry_after_delay() {
        let mut delivery = FailedDelivery::new("eng-1", "manager", "test");
        delivery.last_attempt =
            Instant::now() - FAILED_DELIVERY_RETRY_DELAY - Duration::from_secs(1);
        assert!(delivery.is_ready_for_retry(Instant::now()));
    }

    #[test]
    fn failed_delivery_has_attempts_remaining_at_boundary() {
        let mut delivery = FailedDelivery::new("eng-1", "manager", "test");
        delivery.attempts = FAILED_DELIVERY_MAX_ATTEMPTS - 1;
        assert!(delivery.has_attempts_remaining());
        delivery.attempts = FAILED_DELIVERY_MAX_ATTEMPTS;
        assert!(!delivery.has_attempts_remaining());
    }

    #[test]
    fn failed_delivery_repeat_failure_counter_is_tracked_without_touching_retry_time() {
        let mut delivery = FailedDelivery::new("eng-1", "manager", "test");
        let first_attempt = delivery.last_attempt;
        delivery.record_repeat_failure();

        assert_eq!(delivery.repeated_failures, 2);
        assert_eq!(delivery.last_attempt, first_attempt);
    }

    #[test]
    fn failed_delivery_message_marker_uses_from_field() {
        let delivery = FailedDelivery::new("eng-1", "architect", "body");
        assert_eq!(delivery.message_marker(), "--- Message from architect ---");
    }

    // --- Capture contains marker ---

    #[test]
    fn capture_contains_marker_empty_capture() {
        assert!(!capture_contains_message_marker(
            "",
            "--- Message from x ---"
        ));
    }

    #[test]
    fn capture_contains_marker_partial_match_fails() {
        let marker = message_delivery_marker("manager");
        assert!(!capture_contains_message_marker(
            "--- Message from",
            &marker
        ));
    }

    #[test]
    fn capture_contains_marker_multiline_capture() {
        let marker = message_delivery_marker("eng-1");
        let capture = "line1\nline2\n--- Message from eng-1 ---\nline4\n";
        assert!(capture_contains_message_marker(capture, &marker));
    }

    // --- Error path and recovery tests (Task #265) ---

    #[test]
    fn failed_delivery_not_ready_for_immediate_retry() {
        let fd = FailedDelivery::new("eng-1", "manager", "test message");
        // Just created — not enough time has passed for retry
        assert!(!fd.is_ready_for_retry(Instant::now()));
    }

    #[test]
    fn failed_delivery_ready_after_delay() {
        let mut fd = FailedDelivery::new("eng-1", "manager", "test message");
        // Simulate past creation
        fd.last_attempt = Instant::now() - FAILED_DELIVERY_RETRY_DELAY - Duration::from_secs(1);
        assert!(fd.is_ready_for_retry(Instant::now()));
    }

    #[test]
    fn failed_delivery_has_attempts_remaining_exhausted() {
        let mut fd = FailedDelivery::new("eng-1", "manager", "test message");
        assert!(fd.has_attempts_remaining()); // attempts=1, max=3
        fd.attempts = FAILED_DELIVERY_MAX_ATTEMPTS;
        assert!(!fd.has_attempts_remaining());
    }

    #[test]
    fn failed_delivery_message_marker_format() {
        let fd = FailedDelivery::new("eng-1", "manager", "test message");
        let marker = fd.message_marker();
        assert!(marker.contains("manager"));
    }

    #[test]
    fn failed_delivery_fields_preserved() {
        let fd = FailedDelivery::new("eng-1", "manager", "hello world");
        assert_eq!(fd.recipient, "eng-1");
        assert_eq!(fd.from, "manager");
        assert_eq!(fd.body, "hello world");
        assert_eq!(fd.attempts, 1);
    }
}
