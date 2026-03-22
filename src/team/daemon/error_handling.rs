//! Error handling infrastructure for daemon subsystem steps.
//!
//! Provides `run_loop_step`, `run_recoverable_step`, and
//! `run_recoverable_step_with_catch_unwind` — the wrappers the daemon poll
//! loop uses to execute each subsystem while keeping the daemon alive when
//! individual subsystems fail.

use anyhow::Result;
use tracing::warn;

use super::*;

impl TeamDaemon {
    /// Run a critical subsystem step. Errors are logged but no consecutive-failure
    /// tracking is applied — critical subsystems must not be silently degraded.
    pub(super) fn run_loop_step<F>(&mut self, step: &str, action: F)
    where
        F: FnOnce(&mut Self) -> Result<()>,
    {
        if let Err(error) = action(self) {
            self.record_loop_step_error(step, &error.to_string());
        }
    }

    /// Run a recoverable subsystem step. Errors are logged, and consecutive
    /// failures are tracked. After 3+ consecutive failures an escalation WARN
    /// is emitted each cycle.
    pub(super) fn run_recoverable_step<F>(&mut self, step: &str, action: F)
    where
        F: FnOnce(&mut Self) -> Result<()>,
    {
        match action(self) {
            Ok(()) => {
                self.subsystem_error_counts.remove(step);
            }
            Err(error) => {
                self.record_loop_step_error(step, &error.to_string());
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
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| action(self)));
        match result {
            Ok(Ok(())) => {
                self.subsystem_error_counts.remove(step);
            }
            Ok(Err(error)) => {
                self.record_loop_step_error(step, &error.to_string());
                self.increment_subsystem_error(step);
            }
            Err(panic_payload) => {
                let msg = panic_payload_to_string(&panic_payload);
                self.record_loop_step_error(step, &format!("panic: {msg}"));
                self.increment_subsystem_error(step);
            }
        }
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
}
