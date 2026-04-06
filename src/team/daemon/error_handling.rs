//! Error handling infrastructure for daemon subsystem steps.
//!
//! Provides `run_loop_step`, `run_recoverable_step`, and
//! `run_recoverable_step_with_catch_unwind` — the wrappers the daemon poll
//! loop uses to execute each subsystem while keeping the daemon alive when
//! individual subsystems fail.

use anyhow::Result;
use tracing::{error, warn};

use super::*;

impl TeamDaemon {
    /// Run a critical subsystem step. Errors and panics are logged but no
    /// consecutive-failure tracking is applied.
    pub(super) fn run_loop_step<F>(&mut self, step: &str, action: F)
    where
        F: FnOnce(&mut Self) -> Result<()>,
    {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| action(self))) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                error!(subsystem = step, error = %error, "daemon subsystem failed");
                self.record_loop_step_error(step, &error.to_string());
            }
            Err(panic_payload) => {
                let msg = panic_payload_to_string(&panic_payload);
                error!(subsystem = step, panic = %msg, "daemon subsystem panicked");
                self.record_loop_step_error(step, &format!("panic: {msg}"));
            }
        }
    }

    /// Run a recoverable subsystem step. Errors and panics are logged, and
    /// consecutive failures are tracked. After 3+ consecutive failures an
    /// escalation WARN is emitted each cycle.
    pub(super) fn run_recoverable_step<F>(&mut self, step: &str, action: F)
    where
        F: FnOnce(&mut Self) -> Result<()>,
    {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| action(self))) {
            Ok(Ok(())) => {
                self.subsystem_error_counts.remove(step);
            }
            Ok(Err(error)) => {
                error!(subsystem = step, error = %error, "daemon subsystem failed");
                self.record_loop_step_error(step, &error.to_string());
                self.increment_subsystem_error(step);
            }
            Err(panic_payload) => {
                let msg = panic_payload_to_string(&panic_payload);
                error!(subsystem = step, panic = %msg, "daemon subsystem panicked");
                self.record_loop_step_error(step, &format!("panic: {msg}"));
                self.increment_subsystem_error(step);
            }
        }
    }

    /// Run a recoverable subsystem step with panic protection via
    /// `catch_unwind`. Used for subsystems (standup, retro, telegram) where a
    /// panic must not crash the daemon.
    pub(super) fn run_recoverable_step_with_catch_unwind<F>(&mut self, step: &str, action: F)
    where
        F: FnOnce(&mut Self) -> Result<()>,
    {
        self.run_recoverable_step(step, action);
    }

    pub(super) fn increment_subsystem_error(&mut self, step: &str) {
        let count = self
            .subsystem_error_counts
            .entry(step.to_string())
            .or_insert(0);
        *count += 1;
        if *count >= 3 {
            warn!(
                subsystem = step,
                consecutive_failures = *count,
                "subsystem {step} failing repeatedly"
            );
        }
    }
}

fn panic_payload_to_string(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::events::read_events;
    use crate::team::test_helpers::daemon_config_with_roles;

    #[test]
    fn recoverable_subsystem_failure_does_not_crash_daemon() {
        let tmp = tempfile::tempdir().unwrap();
        let config = daemon_config_with_roles(tmp.path(), Vec::new());
        let mut daemon = TeamDaemon::new(config).unwrap();

        // Simulate a recoverable subsystem that always fails
        daemon.run_recoverable_step("maybe_generate_standup", |_daemon| {
            anyhow::bail!("standup generation failed: board file missing")
        });

        // Daemon should still be alive — verify by running another step successfully
        let mut ran = false;
        daemon.run_recoverable_step("poll_watchers", |_daemon| {
            ran = true;
            Ok(())
        });
        assert!(ran, "daemon should continue after recoverable failure");

        // The failed step should have recorded one error
        assert_eq!(
            daemon.subsystem_error_counts.get("maybe_generate_standup"),
            Some(&1)
        );
        // The successful step should have no error count
        assert_eq!(daemon.subsystem_error_counts.get("poll_watchers"), None);
    }

    #[test]
    fn subsystem_consecutive_failures_escalate() {
        let tmp = tempfile::tempdir().unwrap();
        let config = daemon_config_with_roles(tmp.path(), Vec::new());
        let mut daemon = TeamDaemon::new(config).unwrap();

        // Fail the same recoverable subsystem 3 times
        for i in 0..3 {
            daemon.run_recoverable_step("maybe_generate_standup", |_daemon| {
                anyhow::bail!("standup failure #{}", i + 1)
            });
        }

        // Verify escalation: consecutive count should be 3
        assert_eq!(
            daemon.subsystem_error_counts.get("maybe_generate_standup"),
            Some(&3)
        );

        // A 4th failure should keep incrementing
        daemon.run_recoverable_step("maybe_generate_standup", |_daemon| {
            anyhow::bail!("standup failure #4")
        });
        assert_eq!(
            daemon.subsystem_error_counts.get("maybe_generate_standup"),
            Some(&4)
        );

        // A success resets the counter
        daemon.run_recoverable_step("maybe_generate_standup", |_daemon| Ok(()));
        assert_eq!(
            daemon.subsystem_error_counts.get("maybe_generate_standup"),
            None
        );
    }

    #[test]
    fn critical_subsystem_errors_propagate() {
        let tmp = tempfile::tempdir().unwrap();
        let config = daemon_config_with_roles(tmp.path(), Vec::new());
        let mut daemon = TeamDaemon::new(config).unwrap();

        // Critical steps use run_loop_step which logs errors but does NOT track them
        daemon.run_loop_step("deliver_inbox_messages", |_daemon| {
            anyhow::bail!("delivery failed: network error")
        });

        // Critical subsystem errors should NOT appear in subsystem_error_counts
        assert_eq!(
            daemon.subsystem_error_counts.get("deliver_inbox_messages"),
            None
        );

        // But the error should still be logged as a loop step error (via events)
        let events_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        let events = read_events(&events_path).unwrap();
        let step_error = events.iter().find(|event| event.event == "loop_step_error");
        assert!(
            step_error.is_some(),
            "critical subsystem error should still be logged as event"
        );
    }

    #[test]
    fn catch_unwind_recovers_from_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let config = daemon_config_with_roles(tmp.path(), Vec::new());
        let mut daemon = TeamDaemon::new(config).unwrap();

        // A panic inside a catch_unwind-wrapped step should not crash
        daemon.run_recoverable_step_with_catch_unwind("process_telegram_queue", |_daemon| {
            panic!("telegram thread panicked")
        });

        // Daemon survived — verify error was tracked
        assert_eq!(
            daemon.subsystem_error_counts.get("process_telegram_queue"),
            Some(&1)
        );

        // Verify the error event was logged
        let events_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        let events = read_events(&events_path).unwrap();
        let step_error = events
            .iter()
            .find(|event| event.event == "loop_step_error")
            .expect("panic should be logged as loop_step_error");
        assert!(
            step_error.error.as_deref().unwrap_or("").contains("panic"),
            "error field should mention panic"
        );
    }

    #[test]
    fn recoverable_panicking_subsystem_does_not_crash_daemon() {
        let tmp = tempfile::tempdir().unwrap();
        let config = daemon_config_with_roles(tmp.path(), Vec::new());
        let mut daemon = TeamDaemon::new(config).unwrap();

        daemon.run_recoverable_step("poll_watchers", |_daemon| {
            panic!("watcher crashed");
        });

        let mut continued = false;
        daemon.run_recoverable_step("maybe_auto_dispatch", |_daemon| {
            continued = true;
            Ok(())
        });

        assert!(continued, "daemon should continue after recoverable panic");
        assert_eq!(daemon.subsystem_error_counts.get("poll_watchers"), Some(&1));

        let events_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        let events = read_events(&events_path).unwrap();
        let step_error = events
            .iter()
            .find(|event| event.event == "loop_step_error")
            .expect("panic should be logged as loop_step_error");
        assert!(
            step_error
                .error
                .as_deref()
                .unwrap_or("")
                .contains("panic: watcher crashed")
        );
    }

    #[test]
    fn critical_panicking_subsystem_does_not_crash_daemon() {
        let tmp = tempfile::tempdir().unwrap();
        let config = daemon_config_with_roles(tmp.path(), Vec::new());
        let mut daemon = TeamDaemon::new(config).unwrap();

        daemon.run_loop_step("deliver_inbox_messages", |_daemon| {
            panic!("message routing crashed");
        });

        let mut continued = false;
        daemon.run_loop_step("maybe_auto_dispatch", |_daemon| {
            continued = true;
            Ok(())
        });

        assert!(continued, "daemon should continue after critical panic");
        assert_eq!(daemon.subsystem_error_counts.get("deliver_inbox_messages"), None);

        let events_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        let events = read_events(&events_path).unwrap();
        let step_error = events
            .iter()
            .find(|event| event.event == "loop_step_error")
            .expect("panic should be logged as loop_step_error");
        assert!(
            step_error
                .error
                .as_deref()
                .unwrap_or("")
                .contains("panic: message routing crashed")
        );
    }

    #[test]
    fn consecutive_error_counter_resets_on_success() {
        let tmp = tempfile::tempdir().unwrap();
        let config = daemon_config_with_roles(tmp.path(), Vec::new());
        let mut daemon = TeamDaemon::new(config).unwrap();

        // Fail twice
        for _ in 0..2 {
            daemon.run_recoverable_step("maybe_rotate_board", |_daemon| {
                anyhow::bail!("board rotation failed")
            });
        }
        assert_eq!(
            daemon.subsystem_error_counts.get("maybe_rotate_board"),
            Some(&2)
        );

        // One success should reset the counter
        daemon.run_recoverable_step("maybe_rotate_board", |_daemon| Ok(()));
        assert_eq!(
            daemon.subsystem_error_counts.get("maybe_rotate_board"),
            None
        );

        // Next failure starts from 1 again
        daemon.run_recoverable_step("maybe_rotate_board", |_daemon| {
            anyhow::bail!("board rotation failed again")
        });
        assert_eq!(
            daemon.subsystem_error_counts.get("maybe_rotate_board"),
            Some(&1)
        );
    }

    // ── Required task #279 tests ──

    #[test]
    fn subsystem_error_count_increments() {
        let tmp = tempfile::tempdir().unwrap();
        let config = daemon_config_with_roles(tmp.path(), Vec::new());
        let mut daemon = TeamDaemon::new(config).unwrap();

        assert!(daemon.subsystem_error_counts.is_empty());

        // Each failure should increment the count by 1.
        daemon.increment_subsystem_error("poll_watchers");
        assert_eq!(daemon.subsystem_error_counts.get("poll_watchers"), Some(&1));

        daemon.increment_subsystem_error("poll_watchers");
        assert_eq!(daemon.subsystem_error_counts.get("poll_watchers"), Some(&2));

        // Independent subsystems are tracked separately.
        daemon.increment_subsystem_error("maybe_fire_nudges");
        assert_eq!(
            daemon.subsystem_error_counts.get("maybe_fire_nudges"),
            Some(&1)
        );
        assert_eq!(daemon.subsystem_error_counts.get("poll_watchers"), Some(&2));
    }

    #[test]
    fn consecutive_failures_trigger_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let config = daemon_config_with_roles(tmp.path(), Vec::new());
        let mut daemon = TeamDaemon::new(config).unwrap();

        // Below the threshold (3) — no escalation warning yet.
        daemon.increment_subsystem_error("telegram_queue");
        daemon.increment_subsystem_error("telegram_queue");
        assert_eq!(
            daemon.subsystem_error_counts.get("telegram_queue"),
            Some(&2)
        );

        // Hitting the threshold at exactly 3.
        daemon.increment_subsystem_error("telegram_queue");
        assert_eq!(
            daemon.subsystem_error_counts.get("telegram_queue"),
            Some(&3)
        );

        // Continues incrementing past the threshold.
        daemon.increment_subsystem_error("telegram_queue");
        assert_eq!(
            daemon.subsystem_error_counts.get("telegram_queue"),
            Some(&4)
        );
    }

    #[test]
    fn error_count_resets_on_success() {
        let tmp = tempfile::tempdir().unwrap();
        let config = daemon_config_with_roles(tmp.path(), Vec::new());
        let mut daemon = TeamDaemon::new(config).unwrap();

        // Accumulate errors via run_recoverable_step.
        for _ in 0..4 {
            daemon.run_recoverable_step("check_backend_health", |_d| {
                anyhow::bail!("backend unreachable")
            });
        }
        assert_eq!(
            daemon.subsystem_error_counts.get("check_backend_health"),
            Some(&4)
        );

        // A single success resets to zero (entry removed).
        daemon.run_recoverable_step("check_backend_health", |_d| Ok(()));
        assert_eq!(
            daemon.subsystem_error_counts.get("check_backend_health"),
            None
        );
    }

    #[test]
    fn criticality_classification() {
        let tmp = tempfile::tempdir().unwrap();
        let config = daemon_config_with_roles(tmp.path(), Vec::new());
        let mut daemon = TeamDaemon::new(config).unwrap();

        // Critical step (run_loop_step): error is logged but NOT tracked.
        daemon.run_loop_step("deliver_inbox_messages", |_d| {
            anyhow::bail!("delivery timeout")
        });
        assert!(
            !daemon
                .subsystem_error_counts
                .contains_key("deliver_inbox_messages"),
            "critical steps should not track consecutive failures"
        );

        // Recoverable step (run_recoverable_step): error IS tracked.
        daemon.run_recoverable_step("maybe_rotate_board", |_d| {
            anyhow::bail!("board file locked")
        });
        assert_eq!(
            daemon.subsystem_error_counts.get("maybe_rotate_board"),
            Some(&1),
            "recoverable steps should track consecutive failures"
        );

        // Recoverable with catch_unwind: panic IS tracked same as error.
        daemon.run_recoverable_step_with_catch_unwind("process_telegram_queue", |_d| {
            panic!("unexpected panic")
        });
        assert_eq!(
            daemon.subsystem_error_counts.get("process_telegram_queue"),
            Some(&1),
            "catch_unwind steps should track panics as failures"
        );
    }
}
