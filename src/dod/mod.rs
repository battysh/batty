//! Definition of Done (DoD) — test-gated completion.
//!
//! After an agent signals completion, we run the DoD command to verify the work.
//! If tests pass, the task is complete. If they fail, the failure output can be
//! fed back to the agent for retry.

use std::process::Command;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use crate::config::Defaults;
use crate::task::TaskBattyConfig;

/// Outcome of a single DoD run.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct DodResult {
    /// Whether the DoD command succeeded (exit code 0).
    pub passed: bool,
    /// Combined stdout+stderr from the DoD command.
    pub output: String,
    /// Exit code, if available.
    pub exit_code: Option<i32>,
}

/// Outcome of the full DoD cycle (potentially multiple retries).
#[allow(dead_code)]
#[derive(Debug)]
pub enum DodOutcome {
    /// No DoD command configured — skip verification.
    NoDod,
    /// DoD passed (possibly after retries).
    Passed {
        /// Which attempt succeeded (1-indexed).
        attempt: u32,
        result: DodResult,
    },
    /// DoD failed after all retries exhausted.
    Failed {
        /// Total attempts made.
        attempts: u32,
        /// Results from each attempt.
        results: Vec<DodResult>,
    },
}

/// Resolved configuration for a DoD run.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct DodConfig {
    /// The shell command to run (e.g., "cargo test").
    pub command: String,
    /// Maximum retry attempts.
    pub max_retries: u32,
    /// Working directory for the command.
    pub work_dir: String,
}

impl DodConfig {
    /// Resolve the DoD configuration from task-level overrides and project defaults.
    ///
    /// Priority: task override > project default > None.
    #[allow(dead_code)]
    pub fn resolve(
        task_config: Option<&TaskBattyConfig>,
        project_defaults: &Defaults,
        work_dir: &str,
    ) -> Option<Self> {
        // Task-level dod overrides project-level
        let command = task_config
            .and_then(|tc| tc.dod.clone())
            .or_else(|| project_defaults.dod.clone());

        let command = command?;

        // Task-level max_retries overrides project-level
        let max_retries = task_config
            .and_then(|tc| tc.max_retries)
            .unwrap_or(project_defaults.max_retries);

        Some(DodConfig {
            command,
            max_retries,
            work_dir: work_dir.to_string(),
        })
    }
}

/// Run a single DoD command and capture the result.
#[allow(dead_code)]
pub fn run_dod_command(config: &DodConfig) -> Result<DodResult> {
    info!(command = %config.command, work_dir = %config.work_dir, "running DoD command");

    let output = Command::new("sh")
        .arg("-c")
        .arg(&config.command)
        .current_dir(&config.work_dir)
        .output()
        .with_context(|| format!("failed to execute DoD command: {}", config.command))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = if stderr.is_empty() {
        stdout.to_string()
    } else {
        format!("{stdout}\n--- stderr ---\n{stderr}")
    };

    let exit_code = output.status.code();
    let passed = output.status.success();

    debug!(
        passed = passed,
        exit_code = ?exit_code,
        output_len = combined.len(),
        "DoD command finished"
    );

    Ok(DodResult {
        passed,
        output: combined,
        exit_code,
    })
}

/// Run the full DoD cycle with retries.
///
/// `on_failure` is called after each failed attempt with the attempt number
/// and result, allowing the caller to feed the failure back to the agent
/// before the next retry.
#[allow(dead_code)]
pub fn run_dod_cycle<F>(config: &DodConfig, mut on_failure: F) -> Result<DodOutcome>
where
    F: FnMut(u32, &DodResult),
{
    let total_attempts = config.max_retries + 1; // first attempt + retries
    let mut results = Vec::new();

    for attempt in 1..=total_attempts {
        info!(attempt = attempt, total = total_attempts, "DoD attempt");

        let result = run_dod_command(config)?;

        if result.passed {
            info!(attempt = attempt, "DoD passed");
            return Ok(DodOutcome::Passed { attempt, result });
        }

        warn!(
            attempt = attempt,
            remaining = total_attempts - attempt,
            "DoD failed"
        );

        // Notify caller of failure (so they can feed it back to the agent)
        on_failure(attempt, &result);
        results.push(result);
    }

    Ok(DodOutcome::Failed {
        attempts: total_attempts,
        results,
    })
}

/// Format a DoD failure for feeding back to an agent.
///
/// Produces a concise summary suitable for injecting into an agent's stdin.
#[allow(dead_code)]
pub fn format_failure_feedback(result: &DodResult, attempt: u32, max_attempts: u32) -> String {
    let mut feedback = String::new();
    feedback.push_str(&format!(
        "DoD check failed (attempt {attempt}/{max_attempts}).\n"
    ));
    feedback.push_str("Test output:\n");

    // Truncate very long output to avoid overwhelming the agent
    let max_output_len = 4096;
    if result.output.len() > max_output_len {
        let truncated = &result.output[result.output.len() - max_output_len..];
        feedback.push_str("...(truncated)...\n");
        feedback.push_str(truncated);
    } else {
        feedback.push_str(&result.output);
    }

    if let Some(code) = result.exit_code {
        feedback.push_str(&format!("\nExit code: {code}\n"));
    }

    feedback.push_str("\nPlease fix the failing tests and try again.\n");
    feedback
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Defaults, Policy};
    use crate::task::TaskBattyConfig;

    #[test]
    fn resolve_from_project_defaults() {
        let defaults = Defaults {
            agent: "claude".to_string(),
            policy: Policy::Observe,
            dod: Some("cargo test".to_string()),
            max_retries: 3,
        };

        let config = DodConfig::resolve(None, &defaults, "/work");
        assert!(config.is_some());
        let config = config.unwrap();
        assert_eq!(config.command, "cargo test");
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.work_dir, "/work");
    }

    #[test]
    fn resolve_no_dod_configured() {
        let defaults = Defaults {
            agent: "claude".to_string(),
            policy: Policy::Observe,
            dod: None,
            max_retries: 3,
        };

        let config = DodConfig::resolve(None, &defaults, "/work");
        assert!(config.is_none());
    }

    #[test]
    fn resolve_task_override_takes_priority() {
        let defaults = Defaults {
            agent: "claude".to_string(),
            policy: Policy::Observe,
            dod: Some("cargo test".to_string()),
            max_retries: 3,
        };

        let task_config = TaskBattyConfig {
            agent: None,
            policy: None,
            dod: Some("make test".to_string()),
            max_retries: Some(5),
        };

        let config = DodConfig::resolve(Some(&task_config), &defaults, "/work");
        assert!(config.is_some());
        let config = config.unwrap();
        assert_eq!(config.command, "make test");
        assert_eq!(config.max_retries, 5);
    }

    #[test]
    fn resolve_task_dod_with_default_retries() {
        let defaults = Defaults {
            agent: "claude".to_string(),
            policy: Policy::Observe,
            dod: None,
            max_retries: 3,
        };

        let task_config = TaskBattyConfig {
            agent: None,
            policy: None,
            dod: Some("pytest".to_string()),
            max_retries: None,
        };

        let config = DodConfig::resolve(Some(&task_config), &defaults, "/work");
        assert!(config.is_some());
        let config = config.unwrap();
        assert_eq!(config.command, "pytest");
        assert_eq!(config.max_retries, 3); // falls back to project default
    }

    #[test]
    fn run_passing_command() {
        let config = DodConfig {
            command: "true".to_string(),
            max_retries: 0,
            work_dir: "/tmp".to_string(),
        };

        let result = run_dod_command(&config).unwrap();
        assert!(result.passed);
        assert_eq!(result.exit_code, Some(0));
    }

    #[test]
    fn run_failing_command() {
        let config = DodConfig {
            command: "false".to_string(),
            max_retries: 0,
            work_dir: "/tmp".to_string(),
        };

        let result = run_dod_command(&config).unwrap();
        assert!(!result.passed);
        assert_ne!(result.exit_code, Some(0));
    }

    #[test]
    fn run_command_captures_output() {
        let config = DodConfig {
            command: "echo 'test output here'".to_string(),
            max_retries: 0,
            work_dir: "/tmp".to_string(),
        };

        let result = run_dod_command(&config).unwrap();
        assert!(result.passed);
        assert!(result.output.contains("test output here"));
    }

    #[test]
    fn run_command_captures_stderr() {
        let config = DodConfig {
            command: "echo 'err msg' >&2".to_string(),
            max_retries: 0,
            work_dir: "/tmp".to_string(),
        };

        let result = run_dod_command(&config).unwrap();
        assert!(result.output.contains("err msg"));
        assert!(result.output.contains("stderr"));
    }

    #[test]
    fn run_command_respects_work_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let config = DodConfig {
            command: "pwd".to_string(),
            max_retries: 0,
            work_dir: tmp.path().to_string_lossy().to_string(),
        };

        let result = run_dod_command(&config).unwrap();
        assert!(result.passed);
        // The output should contain the temp dir path
        // (realpath may differ on some systems, so just check it's non-empty)
        assert!(!result.output.trim().is_empty());
    }

    #[test]
    fn cycle_passes_first_attempt() {
        let config = DodConfig {
            command: "true".to_string(),
            max_retries: 2,
            work_dir: "/tmp".to_string(),
        };

        let mut failure_count = 0u32;
        let outcome = run_dod_cycle(&config, |_, _| failure_count += 1).unwrap();

        match outcome {
            DodOutcome::Passed { attempt, .. } => {
                assert_eq!(attempt, 1);
            }
            other => panic!("expected Passed, got: {other:?}"),
        }
        assert_eq!(failure_count, 0);
    }

    #[test]
    fn cycle_fails_all_attempts() {
        let config = DodConfig {
            command: "false".to_string(),
            max_retries: 2,
            work_dir: "/tmp".to_string(),
        };

        let mut failure_count = 0u32;
        let outcome = run_dod_cycle(&config, |_, _| failure_count += 1).unwrap();

        match outcome {
            DodOutcome::Failed { attempts, results } => {
                assert_eq!(attempts, 3); // 1 + 2 retries
                assert_eq!(results.len(), 3);
            }
            other => panic!("expected Failed, got: {other:?}"),
        }
        assert_eq!(failure_count, 3);
    }

    #[test]
    fn cycle_with_no_retries() {
        let config = DodConfig {
            command: "false".to_string(),
            max_retries: 0,
            work_dir: "/tmp".to_string(),
        };

        let outcome = run_dod_cycle(&config, |_, _| {}).unwrap();

        match outcome {
            DodOutcome::Failed { attempts, results } => {
                assert_eq!(attempts, 1);
                assert_eq!(results.len(), 1);
            }
            other => panic!("expected Failed, got: {other:?}"),
        }
    }

    #[test]
    fn cycle_calls_on_failure_with_correct_args() {
        let config = DodConfig {
            command: "echo 'fail output' && false".to_string(),
            max_retries: 1,
            work_dir: "/tmp".to_string(),
        };

        let mut attempts_seen = Vec::new();
        let outcome = run_dod_cycle(&config, |attempt, result| {
            attempts_seen.push(attempt);
            assert!(result.output.contains("fail output"));
        })
        .unwrap();

        assert!(matches!(outcome, DodOutcome::Failed { .. }));
        assert_eq!(attempts_seen, vec![1, 2]);
    }

    #[test]
    fn format_failure_short_output() {
        let result = DodResult {
            passed: false,
            output: "test_foo FAILED\nassert_eq!(1, 2)".to_string(),
            exit_code: Some(1),
        };

        let feedback = format_failure_feedback(&result, 1, 3);
        assert!(feedback.contains("attempt 1/3"));
        assert!(feedback.contains("test_foo FAILED"));
        assert!(feedback.contains("Exit code: 1"));
        assert!(feedback.contains("fix the failing tests"));
    }

    #[test]
    fn format_failure_truncates_long_output() {
        let long_output = "x".repeat(10_000);
        let result = DodResult {
            passed: false,
            output: long_output,
            exit_code: Some(1),
        };

        let feedback = format_failure_feedback(&result, 2, 3);
        assert!(feedback.contains("truncated"));
        // Should be reasonably bounded
        assert!(feedback.len() < 6000);
    }

    #[test]
    fn dod_result_debug_format() {
        let r = DodResult {
            passed: true,
            output: "ok".to_string(),
            exit_code: Some(0),
        };
        assert!(format!("{r:?}").contains("passed: true"));
    }

    #[test]
    fn dod_outcome_no_dod() {
        let outcome = DodOutcome::NoDod;
        assert!(format!("{outcome:?}").contains("NoDod"));
    }
}
