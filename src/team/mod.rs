//! Team mode — hierarchical agent org chart with daemon-managed communication.
//!
//! A YAML-defined team (architect ↔ manager ↔ N engineers) runs in a tmux
//! session. The daemon monitors panes, routes messages between roles, and
//! manages agent lifecycles.

pub mod artifact;
pub mod auto_merge;
#[cfg(test)]
mod behavioral_tests;
pub mod board;
#[cfg(test)]
mod smoke_tests;
// -- Decomposed submodules --
mod init;
pub use init::*;
mod load;
pub use load::*;
mod messaging;
pub use messaging::*;
pub mod board_cmd;
pub mod board_health;
pub mod capability;
pub mod checkpoint;
pub mod comms;
pub mod completion;
pub mod config;
pub mod config_diff;
pub mod cost;
pub mod daemon;
mod daemon_mgmt;
pub mod delivery;
pub mod deps;
pub mod doctor;
pub mod equivalence;
pub mod errors;
pub mod estimation;
pub mod events;
pub mod failure_patterns;
pub mod git_cmd;
pub mod grafana;
pub mod harness;
pub mod hierarchy;
pub mod inbox;
pub mod layout;
pub use daemon_mgmt::*;
mod session;
pub use session::*;
pub mod merge;
pub mod message;
pub mod metrics;
pub mod metrics_cmd;
pub mod nudge;
pub mod parity;
pub mod policy;
pub mod resolver;
pub mod retrospective;
pub mod retry;
pub mod review;
pub mod scale;
pub mod spec_gen;
pub mod standup;
pub mod status;
pub mod stress;
pub mod tact;
pub mod task_cmd;
pub mod task_loop;
pub mod telegram;
pub mod telemetry_db;
#[cfg(test)]
pub mod test_helpers;
#[cfg(test)]
pub mod test_support;
pub mod validation;
pub mod verification;
pub mod watcher;
pub mod workflow;
pub mod worktree_health;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Team config directory name inside `.batty/`.
pub const TEAM_CONFIG_DIR: &str = "team_config";
/// Team config filename.
pub const TEAM_CONFIG_FILE: &str = "team.yaml";

const TRIAGE_RESULT_FRESHNESS_SECONDS: u64 = 300;
pub(crate) const DEFAULT_EVENT_LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentResultStatus {
    Delivered,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AssignmentDeliveryResult {
    pub message_id: String,
    pub status: AssignmentResultStatus,
    pub engineer: String,
    pub task_summary: String,
    pub branch: Option<String>,
    pub work_dir: Option<String>,
    pub detail: String,
    pub ts: u64,
}

/// Resolve the team config directory for a project root.
pub fn team_config_dir(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join(TEAM_CONFIG_DIR)
}

/// Resolve the path to team.yaml.
pub fn team_config_path(project_root: &Path) -> PathBuf {
    team_config_dir(project_root).join(TEAM_CONFIG_FILE)
}

pub fn team_events_path(project_root: &Path) -> PathBuf {
    team_config_dir(project_root).join("events.jsonl")
}

pub(crate) fn orchestrator_log_path(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("orchestrator.log")
}

pub(crate) fn orchestrator_ansi_log_path(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("orchestrator.ansi.log")
}

/// Directory containing per-agent PTY log files written by the shim.
#[allow(dead_code)] // Public API for future shim-mode daemon integration
pub(crate) fn shim_logs_dir(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("shim-logs")
}

/// Path to an individual agent's PTY log file.
#[allow(dead_code)] // Public API for future shim-mode daemon integration
pub(crate) fn shim_log_path(project_root: &Path, agent_id: &str) -> PathBuf {
    shim_logs_dir(project_root).join(format!("{agent_id}.pty.log"))
}

pub(crate) fn shim_events_log_path(project_root: &Path, agent_id: &str) -> PathBuf {
    shim_logs_dir(project_root).join(format!("{agent_id}.events.log"))
}

pub(crate) fn append_shim_event_log(project_root: &Path, agent_id: &str, line: &str) -> Result<()> {
    let path = shim_events_log_path(project_root, agent_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open shim event log {}", path.display()))?;
    use std::io::Write;
    writeln!(file, "[{}] {}", now_unix(), line)
        .with_context(|| format!("failed to write shim event log {}", path.display()))?;
    Ok(())
}

fn assignment_results_dir(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("assignment_results")
}

fn assignment_result_path(project_root: &Path, message_id: &str) -> PathBuf {
    assignment_results_dir(project_root).join(format!("{message_id}.json"))
}

pub(crate) fn store_assignment_result(
    project_root: &Path,
    result: &AssignmentDeliveryResult,
) -> Result<()> {
    let path = assignment_result_path(project_root, &result.message_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_vec_pretty(result)?;
    std::fs::write(&path, content)
        .with_context(|| format!("failed to write assignment result {}", path.display()))?;
    Ok(())
}

pub fn load_assignment_result(
    project_root: &Path,
    message_id: &str,
) -> Result<Option<AssignmentDeliveryResult>> {
    let path = assignment_result_path(project_root, message_id);
    if !path.exists() {
        return Ok(None);
    }
    let data = std::fs::read(&path)
        .with_context(|| format!("failed to read assignment result {}", path.display()))?;
    let result = serde_json::from_slice(&data)
        .with_context(|| format!("failed to parse assignment result {}", path.display()))?;
    Ok(Some(result))
}

pub fn wait_for_assignment_result(
    project_root: &Path,
    message_id: &str,
    timeout: Duration,
) -> Result<Option<AssignmentDeliveryResult>> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(result) = load_assignment_result(project_root, message_id)? {
            return Ok(Some(result));
        }
        if std::time::Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

pub fn format_assignment_result(result: &AssignmentDeliveryResult) -> String {
    let mut text = match result.status {
        AssignmentResultStatus::Delivered => {
            format!(
                "Assignment delivered: {} -> {}",
                result.message_id, result.engineer
            )
        }
        AssignmentResultStatus::Failed => {
            format!(
                "Assignment failed: {} -> {}",
                result.message_id, result.engineer
            )
        }
    };

    text.push_str(&format!("\nTask: {}", result.task_summary));
    if let Some(branch) = result.branch.as_deref() {
        text.push_str(&format!("\nBranch: {branch}"));
    }
    if let Some(work_dir) = result.work_dir.as_deref() {
        text.push_str(&format!("\nWorktree: {work_dir}"));
    }
    if !result.detail.is_empty() {
        text.push_str(&format!("\nDetail: {}", result.detail));
    }
    text
}

pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn team_config_dir_is_under_batty() {
        let root = Path::new("/tmp/project");
        assert_eq!(
            team_config_dir(root),
            PathBuf::from("/tmp/project/.batty/team_config")
        );
    }

    #[test]
    fn team_config_path_points_to_yaml() {
        let root = Path::new("/tmp/project");
        assert_eq!(
            team_config_path(root),
            PathBuf::from("/tmp/project/.batty/team_config/team.yaml")
        );
    }

    #[test]
    fn assignment_result_round_trip_and_format() {
        let tmp = tempfile::tempdir().unwrap();
        let result = AssignmentDeliveryResult {
            message_id: "msg-1".to_string(),
            status: AssignmentResultStatus::Delivered,
            engineer: "eng-1-1".to_string(),
            task_summary: "Say Hello".to_string(),
            branch: Some("eng-1-1/task-1".to_string()),
            work_dir: Some("/tmp/worktree".to_string()),
            detail: "assignment launched".to_string(),
            ts: now_unix(),
        };

        store_assignment_result(tmp.path(), &result).unwrap();
        let loaded = load_assignment_result(tmp.path(), "msg-1")
            .unwrap()
            .unwrap();
        assert_eq!(loaded, result);

        let formatted = format_assignment_result(&loaded);
        assert!(formatted.contains("Assignment delivered: msg-1 -> eng-1-1"));
        assert!(formatted.contains("Branch: eng-1-1/task-1"));
        assert!(formatted.contains("Worktree: /tmp/worktree"));
    }

    #[test]
    fn wait_for_assignment_result_returns_none_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let result =
            wait_for_assignment_result(tmp.path(), "missing", Duration::from_millis(10)).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn shim_logs_dir_path() {
        let root = Path::new("/tmp/project");
        assert_eq!(
            shim_logs_dir(root),
            PathBuf::from("/tmp/project/.batty/shim-logs")
        );
    }

    #[test]
    fn shim_log_path_includes_agent_id() {
        let root = Path::new("/tmp/project");
        assert_eq!(
            shim_log_path(root, "eng-1-1"),
            PathBuf::from("/tmp/project/.batty/shim-logs/eng-1-1.pty.log")
        );
    }

    #[test]
    fn shim_events_log_path_includes_agent_id() {
        let root = Path::new("/tmp/project");
        assert_eq!(
            shim_events_log_path(root, "eng-1-1"),
            PathBuf::from("/tmp/project/.batty/shim-logs/eng-1-1.events.log")
        );
    }

    /// Count unwrap()/expect() calls in production code (before `#[cfg(test)] mod tests`).
    fn production_unwrap_expect_count(source: &str) -> usize {
        // Split at the test module boundary, not individual #[cfg(test)] items
        let prod = if let Some(pos) = source.find("\n#[cfg(test)]\nmod tests") {
            &source[..pos]
        } else {
            source
        };
        prod.lines()
            .filter(|line| {
                let trimmed = line.trim();
                // Skip lines that are themselves cfg(test)-gated items
                !trimmed.starts_with("#[cfg(test)]")
                    && (trimmed.contains(".unwrap(") || trimmed.contains(".expect("))
            })
            .count()
    }

    #[test]
    fn production_mod_has_no_unwrap_or_expect_calls() {
        let src = include_str!("mod.rs");
        assert_eq!(
            production_unwrap_expect_count(src),
            0,
            "production mod.rs should avoid unwrap/expect"
        );
    }
}
