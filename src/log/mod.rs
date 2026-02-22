//! Structured execution log — JSON lines per run.
//!
//! Every batty session writes a `.jsonl` log file capturing all events:
//! task reads, agent launches, prompt detections, auto-responses,
//! test executions, and completion status. Each line is a self-contained
//! JSON object with a timestamp, making logs easy to grep, stream, and
//! post-process.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::Serialize;
use tracing::debug;

/// Timestamp as ISO 8601 string.
fn now_iso8601() -> String {
    // Use chrono if available, otherwise fall back to a simple approach.
    // For now, use the system time formatted manually.
    let now = std::time::SystemTime::now();
    let duration = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs();

    // Format as seconds-since-epoch (task #12 can upgrade to chrono if needed)
    format!("{secs}")
}

/// A structured event in the execution log.
#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    /// Unix timestamp (seconds since epoch).
    pub timestamp: String,
    /// The event type and its data.
    #[serde(flatten)]
    pub event: LogEvent,
}

/// All event types that can appear in the execution log.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", content = "data")]
#[serde(rename_all = "snake_case")]
pub enum LogEvent {
    /// A task was read from the kanban board.
    TaskRead {
        task_id: u32,
        title: String,
        status: String,
    },
    /// A git worktree was created for the task.
    WorktreeCreated {
        task_id: u32,
        path: String,
        branch: String,
    },
    /// A phase run worktree was created.
    PhaseWorktreeCreated {
        phase: String,
        path: String,
        branch: String,
        base_branch: String,
    },
    /// A run worktree was retained for later inspection.
    PhaseWorktreeRetained {
        phase: String,
        path: String,
        branch: String,
        reason: String,
    },
    /// A run worktree was cleaned up after merge.
    PhaseWorktreeCleaned {
        phase: String,
        path: String,
        branch: String,
    },
    /// An agent process was launched.
    AgentLaunched {
        agent: String,
        program: String,
        args: Vec<String>,
        work_dir: String,
    },
    /// Launch context was composed and snapshotted before execution.
    LaunchContextSnapshot {
        phase: String,
        agent: String,
        instructions_path: String,
        phase_doc_path: String,
        config_source: String,
        snapshot_path: String,
        snapshot: String,
    },
    /// A prompt was detected in agent output.
    PromptDetected { kind: String, matched_text: String },
    /// An auto-response was sent to the agent.
    AutoResponse { prompt: String, response: String },
    /// User input was forwarded to the agent.
    UserInput { length: usize },
    /// A DoD test command was executed.
    TestExecuted {
        command: String,
        passed: bool,
        exit_code: Option<i32>,
    },
    /// Result of a DoD test (with output summary).
    TestResult {
        attempt: u32,
        passed: bool,
        output_lines: usize,
    },
    /// A git commit was created.
    Commit { hash: String, message: String },
    /// A branch was merged.
    Merge { source: String, target: String },
    /// A policy decision was made.
    PolicyDecision { decision: String, prompt: String },
    /// Agent output line (for verbose logging).
    AgentOutput { line: String },
    /// The run completed successfully.
    RunCompleted { summary: String },
    /// The run failed.
    RunFailed { reason: String },
    /// Completion contract decision (accept/reject + check results).
    CompletionDecision {
        phase: String,
        passed: bool,
        board_all_done: bool,
        milestone_done: bool,
        summary_exists: bool,
        dod_passed: bool,
        executor_stable: bool,
        reasons: Vec<String>,
        summary_path: Option<String>,
        dod_command: String,
        dod_executed: bool,
        dod_exit_code: Option<i32>,
        dod_output_lines: usize,
    },
    /// Session started.
    SessionStarted { phase: String },
    /// Session ended.
    SessionEnded { result: String },
}

/// Writer for JSON lines execution logs.
pub struct ExecutionLog {
    writer: Mutex<BufWriter<File>>,
    #[allow(dead_code)]
    path: PathBuf,
}

impl ExecutionLog {
    /// Create a new execution log, writing to the given path.
    ///
    /// Creates the file (and parent directories) if they don't exist.
    /// Appends to an existing file.
    pub fn new(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create log directory: {}", parent.display()))?;
        }

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed to open log file: {}", path.display()))?;

        Ok(Self {
            writer: Mutex::new(BufWriter::new(file)),
            path: path.to_path_buf(),
        })
    }

    /// Log an event.
    pub fn log(&self, event: LogEvent) -> Result<()> {
        let entry = LogEntry {
            timestamp: now_iso8601(),
            event,
        };

        let json = serde_json::to_string(&entry).context("failed to serialize log entry")?;

        debug!(event = %json, "execution log");

        let mut writer = self.writer.lock().unwrap();
        writeln!(writer, "{json}").context("failed to write log entry")?;
        writer.flush().context("failed to flush log")?;

        Ok(())
    }

    /// Get the path to the log file.
    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Convert a SupervisorEvent to a LogEvent.
///
/// Not all supervisor events have a 1:1 mapping — some are translated
/// with best-effort field extraction.
impl From<&crate::supervisor::SupervisorEvent> for LogEvent {
    fn from(event: &crate::supervisor::SupervisorEvent) -> Self {
        use crate::supervisor::SupervisorEvent;
        match event {
            SupervisorEvent::Output(line) => LogEvent::AgentOutput { line: line.clone() },
            SupervisorEvent::PromptDetected(detected) => LogEvent::PromptDetected {
                kind: format!("{:?}", detected.kind),
                matched_text: detected.matched_text.clone(),
            },
            SupervisorEvent::PolicyDecision(decision) => {
                let (decision_str, prompt_str) = match decision {
                    crate::policy::Decision::Observe { prompt } => {
                        ("observe".to_string(), prompt.clone())
                    }
                    crate::policy::Decision::Suggest { prompt, .. } => {
                        ("suggest".to_string(), prompt.clone())
                    }
                    crate::policy::Decision::Act { prompt, .. } => {
                        ("act".to_string(), prompt.clone())
                    }
                    crate::policy::Decision::Escalate { prompt } => {
                        ("escalate".to_string(), prompt.clone())
                    }
                };
                LogEvent::PolicyDecision {
                    decision: decision_str,
                    prompt: prompt_str,
                }
            }
            SupervisorEvent::AutoResponse { prompt, response } => LogEvent::AutoResponse {
                prompt: prompt.clone(),
                response: response.clone(),
            },
            SupervisorEvent::SessionEnd(result) => LogEvent::SessionEnded {
                result: result.clone(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_entry_serializes_to_json() {
        let entry = LogEntry {
            timestamp: "1234567890".to_string(),
            event: LogEvent::TaskRead {
                task_id: 1,
                title: "scaffolding".to_string(),
                status: "backlog".to_string(),
            },
        };

        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"event\":\"task_read\""));
        assert!(json.contains("\"task_id\":1"));
        assert!(json.contains("\"timestamp\":\"1234567890\""));
    }

    #[test]
    fn all_event_types_serialize() {
        let events = vec![
            LogEvent::TaskRead {
                task_id: 1,
                title: "test".to_string(),
                status: "backlog".to_string(),
            },
            LogEvent::WorktreeCreated {
                task_id: 1,
                path: "/tmp/wt".to_string(),
                branch: "task/1".to_string(),
            },
            LogEvent::PhaseWorktreeCreated {
                phase: "phase-2.5".to_string(),
                path: "/tmp/wt-phase".to_string(),
                branch: "phase-2-5-run-001".to_string(),
                base_branch: "main".to_string(),
            },
            LogEvent::PhaseWorktreeRetained {
                phase: "phase-2.5".to_string(),
                path: "/tmp/wt-phase".to_string(),
                branch: "phase-2-5-run-001".to_string(),
                reason: "run failed".to_string(),
            },
            LogEvent::PhaseWorktreeCleaned {
                phase: "phase-2.5".to_string(),
                path: "/tmp/wt-phase".to_string(),
                branch: "phase-2-5-run-001".to_string(),
            },
            LogEvent::AgentLaunched {
                agent: "claude".to_string(),
                program: "claude".to_string(),
                args: vec!["--prompt".to_string(), "task".to_string()],
                work_dir: "/work".to_string(),
            },
            LogEvent::LaunchContextSnapshot {
                phase: "phase-2.5".to_string(),
                agent: "claude-code".to_string(),
                instructions_path: "CLAUDE.md".to_string(),
                phase_doc_path: "kanban/phase-2.5/PHASE.md".to_string(),
                config_source: ".batty/config.toml".to_string(),
                snapshot_path: ".batty/logs/phase-2.5-ctx.log".to_string(),
                snapshot: "context body".to_string(),
            },
            LogEvent::PromptDetected {
                kind: "Permission".to_string(),
                matched_text: "Allow tool Read?".to_string(),
            },
            LogEvent::AutoResponse {
                prompt: "Continue?".to_string(),
                response: "y".to_string(),
            },
            LogEvent::UserInput { length: 5 },
            LogEvent::TestExecuted {
                command: "cargo test".to_string(),
                passed: true,
                exit_code: Some(0),
            },
            LogEvent::TestResult {
                attempt: 1,
                passed: true,
                output_lines: 42,
            },
            LogEvent::Commit {
                hash: "abc123".to_string(),
                message: "fix bug".to_string(),
            },
            LogEvent::Merge {
                source: "task/1".to_string(),
                target: "main".to_string(),
            },
            LogEvent::PolicyDecision {
                decision: "act".to_string(),
                prompt: "Allow?".to_string(),
            },
            LogEvent::AgentOutput {
                line: "hello".to_string(),
            },
            LogEvent::RunCompleted {
                summary: "all good".to_string(),
            },
            LogEvent::RunFailed {
                reason: "tests failed".to_string(),
            },
            LogEvent::CompletionDecision {
                phase: "phase-2.5".to_string(),
                passed: true,
                board_all_done: true,
                milestone_done: true,
                summary_exists: true,
                dod_passed: true,
                executor_stable: true,
                reasons: vec![],
                summary_path: Some("/work/phase-summary.md".to_string()),
                dod_command: "cargo test".to_string(),
                dod_executed: true,
                dod_exit_code: Some(0),
                dod_output_lines: 120,
            },
            LogEvent::SessionStarted {
                phase: "phase-1".to_string(),
            },
            LogEvent::SessionEnded {
                result: "Completed".to_string(),
            },
        ];

        for event in events {
            let entry = LogEntry {
                timestamp: "0".to_string(),
                event,
            };
            let json = serde_json::to_string(&entry);
            assert!(json.is_ok(), "failed to serialize: {entry:?}");

            // Verify it contains the event tag
            let s = json.unwrap();
            assert!(s.contains("\"event\":"), "missing event tag in: {s}");
        }
    }

    #[test]
    fn write_and_read_log_file() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("test.jsonl");

        let log = ExecutionLog::new(&log_path).unwrap();

        log.log(LogEvent::SessionStarted {
            phase: "phase-1".to_string(),
        })
        .unwrap();

        log.log(LogEvent::TaskRead {
            task_id: 5,
            title: "adapter".to_string(),
            status: "in-progress".to_string(),
        })
        .unwrap();

        log.log(LogEvent::SessionEnded {
            result: "Completed".to_string(),
        })
        .unwrap();

        // Read back and verify
        let content = std::fs::read_to_string(&log_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3);

        // Each line should be valid JSON
        for line in &lines {
            let parsed: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(parsed.get("event").is_some());
            assert!(parsed.get("timestamp").is_some());
        }

        // First line should be session_started
        assert!(lines[0].contains("\"event\":\"session_started\""));
        // Second should be task_read
        assert!(lines[1].contains("\"event\":\"task_read\""));
        // Third should be session_ended
        assert!(lines[2].contains("\"event\":\"session_ended\""));
    }

    #[test]
    fn creates_parent_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("deep").join("nested").join("run.jsonl");

        let log = ExecutionLog::new(&log_path).unwrap();
        log.log(LogEvent::RunCompleted {
            summary: "ok".to_string(),
        })
        .unwrap();

        assert!(log_path.exists());
    }

    #[test]
    fn appends_to_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("append.jsonl");

        // Write first entry
        {
            let log = ExecutionLog::new(&log_path).unwrap();
            log.log(LogEvent::SessionStarted {
                phase: "p1".to_string(),
            })
            .unwrap();
        }

        // Open again and write second entry
        {
            let log = ExecutionLog::new(&log_path).unwrap();
            log.log(LogEvent::SessionEnded {
                result: "ok".to_string(),
            })
            .unwrap();
        }

        let content = std::fs::read_to_string(&log_path).unwrap();
        assert_eq!(content.lines().count(), 2);
    }

    #[test]
    fn log_path_accessor() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("test.jsonl");

        let log = ExecutionLog::new(&log_path).unwrap();
        assert_eq!(log.path(), log_path);
    }

    #[test]
    fn supervisor_event_conversion() {
        use crate::supervisor::SupervisorEvent;

        let events = vec![
            SupervisorEvent::Output("hello".to_string()),
            SupervisorEvent::AutoResponse {
                prompt: "Continue?".to_string(),
                response: "y".to_string(),
            },
            SupervisorEvent::SessionEnd("Completed".to_string()),
        ];

        for event in &events {
            let log_event: LogEvent = event.into();
            let entry = LogEntry {
                timestamp: "0".to_string(),
                event: log_event,
            };
            assert!(serde_json::to_string(&entry).is_ok());
        }
    }

    #[test]
    fn supervisor_prompt_detected_conversion() {
        use crate::prompt::{DetectedPrompt, PromptKind};
        use crate::supervisor::SupervisorEvent;

        let event = SupervisorEvent::PromptDetected(DetectedPrompt {
            kind: PromptKind::Permission {
                detail: "Read".to_string(),
            },
            matched_text: "Allow tool Read?".to_string(),
        });

        let log_event: LogEvent = (&event).into();
        let json = serde_json::to_string(&LogEntry {
            timestamp: "0".to_string(),
            event: log_event,
        })
        .unwrap();

        assert!(json.contains("prompt_detected"));
        assert!(json.contains("Permission"));
        assert!(json.contains("Allow tool Read?"));
    }

    #[test]
    fn supervisor_policy_decision_conversion() {
        use crate::policy::Decision;
        use crate::supervisor::SupervisorEvent;

        let decisions = vec![
            SupervisorEvent::PolicyDecision(Decision::Observe {
                prompt: "test".to_string(),
            }),
            SupervisorEvent::PolicyDecision(Decision::Suggest {
                prompt: "Allow?".to_string(),
                response: "y".to_string(),
            }),
            SupervisorEvent::PolicyDecision(Decision::Act {
                prompt: "Continue?".to_string(),
                response: "y".to_string(),
            }),
            SupervisorEvent::PolicyDecision(Decision::Escalate {
                prompt: "unknown".to_string(),
            }),
        ];

        for event in &decisions {
            let log_event: LogEvent = event.into();
            let json = serde_json::to_string(&LogEntry {
                timestamp: "0".to_string(),
                event: log_event,
            })
            .unwrap();
            assert!(json.contains("policy_decision"));
        }
    }

    #[test]
    fn timestamp_is_numeric() {
        let ts = now_iso8601();
        assert!(
            ts.parse::<u64>().is_ok(),
            "timestamp should be numeric: {ts}"
        );
    }
}
