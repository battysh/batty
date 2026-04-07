//! Error handling infrastructure for daemon subsystem steps.
//!
//! Provides `run_loop_step`, `run_recoverable_step`, and
//! `run_recoverable_step_with_catch_unwind` — the wrappers the daemon poll
//! loop uses to execute each subsystem while keeping the daemon alive when
//! individual subsystems fail.

use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{error, warn};

use super::*;

const OPTIONAL_SUBSYSTEM_ERROR_WINDOW_SECS: u64 = 600;
const OPTIONAL_SUBSYSTEM_ERROR_BUDGET: usize = 5;
const OPTIONAL_SUBSYSTEM_BACKOFF_SECS: [u64; 3] = [60, 300, 1_800];
const OPTIONAL_SUBSYSTEM_BACKOFF_KEY_PREFIX: &str = "__optional_subsystem_backoff:";
const OPTIONAL_SUBSYSTEM_DISABLE_KEY_PREFIX: &str = "__optional_subsystem_disabled:";

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

    pub(super) fn run_optional_subsystem_step<F>(&mut self, step: &str, subsystem: &str, action: F)
    where
        F: FnOnce(&mut Self) -> Result<()>,
    {
        if !self.optional_subsystem_ready(subsystem) {
            return;
        }

        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| action(self))) {
            Ok(Ok(())) => {
                self.subsystem_error_counts.remove(step);
                self.record_optional_subsystem_success(subsystem);
            }
            Ok(Err(error)) => {
                error!(subsystem = step, error = %error, "daemon subsystem failed");
                self.record_loop_step_error(step, &error.to_string());
                self.increment_subsystem_error(step);
                self.record_optional_subsystem_failure(subsystem, &error.to_string());
            }
            Err(panic_payload) => {
                let msg = panic_payload_to_string(&panic_payload);
                error!(subsystem = step, panic = %msg, "daemon subsystem panicked");
                self.record_loop_step_error(step, &format!("panic: {msg}"));
                self.increment_subsystem_error(step);
                self.record_optional_subsystem_failure(subsystem, &format!("panic: {msg}"));
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

    pub(super) fn optional_subsystem_ready(&mut self, subsystem: &str) -> bool {
        let disable_key = optional_subsystem_disable_key(subsystem);
        let Some(disabled_until) = self.intervention_cooldowns.get(&disable_key).copied() else {
            return true;
        };

        if Instant::now() < disabled_until {
            return false;
        }

        self.intervention_cooldowns.remove(&disable_key);
        tracing::info!(subsystem, "optional subsystem re-enabled after backoff");
        self.record_orchestrator_action(format!(
            "health: re-enabled optional subsystem {subsystem} after backoff"
        ));
        true
    }

    pub(super) fn record_optional_subsystem_success(&mut self, subsystem: &str) {
        let backoff_key = optional_subsystem_backoff_key(subsystem);
        if self.recent_optional_subsystem_error_count(subsystem) == 0 {
            self.subsystem_error_counts.remove(&backoff_key);
        }
    }

    pub(super) fn record_optional_subsystem_failure(&mut self, subsystem: &str, error: &str) {
        let recent_errors = self.recent_optional_subsystem_error_count(subsystem);
        if recent_errors <= OPTIONAL_SUBSYSTEM_ERROR_BUDGET {
            return;
        }

        let disable_key = optional_subsystem_disable_key(subsystem);
        if self.intervention_cooldowns.contains_key(&disable_key) {
            return;
        }

        let backoff_key = optional_subsystem_backoff_key(subsystem);
        let backoff_index = self
            .subsystem_error_counts
            .get(&backoff_key)
            .copied()
            .unwrap_or(0)
            .min((OPTIONAL_SUBSYSTEM_BACKOFF_SECS.len() - 1) as u32);
        let backoff_secs = OPTIONAL_SUBSYSTEM_BACKOFF_SECS[backoff_index as usize];
        self.intervention_cooldowns.insert(
            disable_key,
            Instant::now() + Duration::from_secs(backoff_secs),
        );
        self.subsystem_error_counts.insert(
            backoff_key,
            (backoff_index + 1).min((OPTIONAL_SUBSYSTEM_BACKOFF_SECS.len() - 1) as u32),
        );
        warn!(
            subsystem,
            recent_errors,
            backoff_secs,
            error,
            "optional subsystem disabled after exceeding error budget"
        );
        self.record_orchestrator_action(format!(
            "health: disabled optional subsystem {subsystem} after {recent_errors} errors in 10m; retry in {backoff_secs}s ({error})"
        ));
    }

    pub(super) fn snapshot_optional_subsystem_backoff(
        &self,
    ) -> std::collections::HashMap<String, u32> {
        optional_subsystem_names()
            .iter()
            .filter_map(|subsystem| {
                self.subsystem_error_counts
                    .get(&optional_subsystem_backoff_key(subsystem))
                    .copied()
                    .map(|value| ((*subsystem).to_string(), value))
            })
            .collect()
    }

    pub(super) fn snapshot_optional_subsystem_disabled_remaining_secs(
        &self,
    ) -> std::collections::HashMap<String, u64> {
        optional_subsystem_names()
            .iter()
            .filter_map(|subsystem| {
                self.intervention_cooldowns
                    .get(&optional_subsystem_disable_key(subsystem))
                    .and_then(|deadline| deadline.checked_duration_since(Instant::now()))
                    .map(|remaining| ((*subsystem).to_string(), remaining.as_secs()))
            })
            .collect()
    }

    pub(super) fn restore_optional_subsystem_budget_state(
        &mut self,
        backoff: &std::collections::HashMap<String, u32>,
        disabled_remaining_secs: &std::collections::HashMap<String, u64>,
    ) {
        for (subsystem, value) in backoff {
            self.subsystem_error_counts
                .insert(optional_subsystem_backoff_key(subsystem), *value);
        }
        for (subsystem, remaining_secs) in disabled_remaining_secs {
            self.intervention_cooldowns.insert(
                optional_subsystem_disable_key(subsystem),
                Instant::now() + Duration::from_secs(*remaining_secs),
            );
        }
    }

    fn recent_optional_subsystem_error_count(&self, subsystem: &str) -> usize {
        let cutoff = now_unix().saturating_sub(OPTIONAL_SUBSYSTEM_ERROR_WINDOW_SECS);
        crate::team::events::read_events(&crate::team::team_events_path(&self.config.project_root))
            .map(|events| {
                events
                    .into_iter()
                    .filter(|event| {
                        event.event == "loop_step_error"
                            && event.ts >= cutoff
                            && event.step.as_deref().and_then(optional_subsystem_for_step)
                                == Some(subsystem)
                    })
                    .count()
            })
            .unwrap_or(0)
    }
}

pub(crate) fn optional_subsystem_for_step(step: &str) -> Option<&'static str> {
    match step {
        "telemetry_emit_event" => Some("telemetry"),
        "process_discord_queue" => Some("discord"),
        "process_telegram_queue" => Some("telegram"),
        "maybe_generate_standup" => Some("standup"),
        _ => None,
    }
}

pub(crate) fn optional_subsystem_names() -> [&'static str; 5] {
    ["telemetry", "discord", "telegram", "grafana", "standup"]
}

fn optional_subsystem_backoff_key(subsystem: &str) -> String {
    format!("{OPTIONAL_SUBSYSTEM_BACKOFF_KEY_PREFIX}{subsystem}")
}

fn optional_subsystem_disable_key(subsystem: &str) -> String {
    format!("{OPTIONAL_SUBSYSTEM_DISABLE_KEY_PREFIX}{subsystem}")
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
        assert_eq!(
            daemon.subsystem_error_counts.get("deliver_inbox_messages"),
            None
        );

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

    #[test]
    fn optional_subsystem_disables_after_budget_and_recovers_after_backoff() {
        let tmp = tempfile::tempdir().unwrap();
        let config = daemon_config_with_roles(tmp.path(), Vec::new());
        let mut daemon = TeamDaemon::new(config).unwrap();

        for i in 0..=OPTIONAL_SUBSYSTEM_ERROR_BUDGET {
            daemon.run_optional_subsystem_step("process_telegram_queue", "telegram", |_daemon| {
                anyhow::bail!("telegram failure #{i}")
            });
        }

        let disable_key = optional_subsystem_disable_key("telegram");
        assert!(
            daemon.intervention_cooldowns.contains_key(&disable_key),
            "telegram should be disabled after exceeding the error budget"
        );

        let mut ran_while_disabled = false;
        daemon.run_optional_subsystem_step("process_telegram_queue", "telegram", |_daemon| {
            ran_while_disabled = true;
            Ok(())
        });
        assert!(!ran_while_disabled, "disabled subsystem should be skipped");

        daemon
            .intervention_cooldowns
            .insert(disable_key, Instant::now() - Duration::from_secs(1));

        let mut ran_after_backoff = false;
        daemon.run_optional_subsystem_step("process_telegram_queue", "telegram", |_daemon| {
            ran_after_backoff = true;
            Ok(())
        });
        assert!(
            ran_after_backoff,
            "subsystem should run after backoff expires"
        );
    }
}
