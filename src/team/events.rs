//! Structured JSONL event stream for external consumers.

use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::DEFAULT_EVENT_LOG_MAX_BYTES;

/// Bundled parameters for merge-confidence scoring events.
pub struct MergeConfidenceInfo<'a> {
    pub engineer: &'a str,
    pub task: &'a str,
    pub confidence: f64,
    pub files_changed: usize,
    pub lines_changed: usize,
    pub has_migrations: bool,
    pub has_config_changes: bool,
    pub rename_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]

pub struct TeamEvent {
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recipient: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_members: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_members: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_running: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    pub ts: u64,
}

impl TeamEvent {
    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    fn base(event: &str) -> Self {
        Self {
            event: event.into(),
            role: None,
            task: None,
            recipient: None,
            from: None,
            to: None,
            restart: None,
            restart_count: None,
            load: None,
            working_members: None,
            total_members: None,
            session_running: None,
            reason: None,
            step: None,
            error: None,
            uptime_secs: None,
            session_size_bytes: None,
            output_bytes: None,
            filename: None,
            content_hash: None,
            ts: Self::now(),
        }
    }

    pub fn daemon_started() -> Self {
        Self::base("daemon_started")
    }

    pub fn daemon_reloading() -> Self {
        Self::base("daemon_reloading")
    }

    pub fn daemon_reloaded() -> Self {
        Self::base("daemon_reloaded")
    }

    pub fn daemon_stopped() -> Self {
        Self::base("daemon_stopped")
    }

    pub fn daemon_stopped_with_reason(reason: &str, uptime_secs: u64) -> Self {
        Self {
            reason: Some(reason.into()),
            uptime_secs: Some(uptime_secs),
            ..Self::base("daemon_stopped")
        }
    }

    pub fn daemon_heartbeat(uptime_secs: u64) -> Self {
        Self {
            uptime_secs: Some(uptime_secs),
            ..Self::base("daemon_heartbeat")
        }
    }

    pub fn context_exhausted(
        role: &str,
        task: Option<u32>,
        session_size_bytes: Option<u64>,
    ) -> Self {
        Self {
            role: Some(role.into()),
            task: task.map(|task_id| task_id.to_string()),
            session_size_bytes,
            ..Self::base("context_exhausted")
        }
    }

    pub fn loop_step_error(step: &str, error: &str) -> Self {
        Self {
            step: Some(step.into()),
            error: Some(error.into()),
            ..Self::base("loop_step_error")
        }
    }

    pub fn daemon_panic(reason: &str) -> Self {
        Self {
            reason: Some(reason.into()),
            ..Self::base("daemon_panic")
        }
    }

    pub fn task_assigned(role: &str, task: &str) -> Self {
        Self {
            role: Some(role.into()),
            task: Some(task.into()),
            ..Self::base("task_assigned")
        }
    }

    pub fn cwd_corrected(role: &str, path: &str) -> Self {
        Self {
            role: Some(role.into()),
            reason: Some(path.into()),
            ..Self::base("cwd_corrected")
        }
    }

    pub fn review_nudge_sent(role: &str, task: &str) -> Self {
        Self {
            role: Some(role.into()),
            task: Some(task.into()),
            ..Self::base("review_nudge_sent")
        }
    }

    pub fn review_escalated(task: &str, reason: &str) -> Self {
        Self {
            task: Some(task.into()),
            reason: Some(reason.into()),
            ..Self::base("review_escalated")
        }
    }

    pub fn state_reconciliation(role: Option<&str>, task: Option<&str>, correction: &str) -> Self {
        Self {
            role: role.map(str::to_string),
            task: task.map(str::to_string),
            reason: Some(correction.into()),
            ..Self::base("state_reconciliation")
        }
    }

    pub fn task_escalated(role: &str, task: &str, reason: Option<&str>) -> Self {
        Self {
            role: Some(role.into()),
            task: Some(task.into()),
            reason: reason.map(|r| r.into()),
            ..Self::base("task_escalated")
        }
    }

    pub fn task_unblocked(role: &str, task: &str) -> Self {
        Self {
            role: Some(role.into()),
            task: Some(task.into()),
            ..Self::base("task_unblocked")
        }
    }

    pub fn board_task_archived(task: &str, role: Option<&str>) -> Self {
        Self {
            role: role.map(str::to_string),
            task: Some(task.into()),
            ..Self::base("board_task_archived")
        }
    }

    pub fn performance_regression(task: &str, reason: &str) -> Self {
        Self {
            task: Some(task.into()),
            reason: Some(reason.into()),
            ..Self::base("performance_regression")
        }
    }

    pub fn task_completed(role: &str, task: Option<&str>) -> Self {
        Self {
            role: Some(role.into()),
            task: task.map(|t| t.into()),
            ..Self::base("task_completed")
        }
    }

    pub fn standup_generated(recipient: &str) -> Self {
        Self {
            recipient: Some(recipient.into()),
            ..Self::base("standup_generated")
        }
    }

    pub fn retro_generated() -> Self {
        Self::base("retro_generated")
    }

    pub fn pattern_detected(pattern_type: &str, frequency: u32) -> Self {
        Self {
            reason: Some(format!("{pattern_type}:{frequency}")),
            ..Self::base("pattern_detected")
        }
    }

    pub fn member_crashed(role: &str, restart: bool) -> Self {
        Self {
            role: Some(role.into()),
            restart: Some(restart),
            ..Self::base("member_crashed")
        }
    }

    pub fn pane_death(role: &str) -> Self {
        Self {
            role: Some(role.into()),
            ..Self::base("pane_death")
        }
    }

    pub fn pane_respawned(role: &str) -> Self {
        Self {
            role: Some(role.into()),
            ..Self::base("pane_respawned")
        }
    }

    pub fn narration_detected(role: &str, task: Option<u32>) -> Self {
        Self {
            role: Some(role.into()),
            task: task.map(|id| id.to_string()),
            ..Self::base("narration_detected")
        }
    }

    pub fn narration_rejection(role: &str, task_id: u32, rejection_count: u32) -> Self {
        Self {
            role: Some(role.into()),
            task: Some(task_id.to_string()),
            reason: Some(format!("rejection_count={rejection_count}")),
            ..Self::base("narration_rejection")
        }
    }

    pub fn planning_cycle_triggered(role: &str, idle_engineers: u32, board_summary: &str) -> Self {
        Self {
            role: Some(role.into()),
            working_members: Some(idle_engineers),
            reason: Some(board_summary.into()),
            ..Self::base("planning_cycle_triggered")
        }
    }

    pub fn planning_cycle_completed(
        role: &str,
        tasks_created: u32,
        latency_secs: u64,
        success: bool,
        error: Option<&str>,
    ) -> Self {
        Self {
            role: Some(role.into()),
            restart_count: Some(tasks_created),
            uptime_secs: Some(latency_secs),
            reason: Some(if success { "success" } else { "failure" }.into()),
            error: error.map(str::to_string),
            ..Self::base("planning_cycle_completed")
        }
    }

    pub fn context_pressure_warning(
        role: &str,
        task: Option<u32>,
        output_bytes: u64,
        threshold_bytes: u64,
    ) -> Self {
        Self {
            role: Some(role.into()),
            task: task.map(|id| id.to_string()),
            output_bytes: Some(output_bytes),
            reason: Some(format!("threshold_bytes={threshold_bytes}")),
            ..Self::base("context_pressure_warning")
        }
    }

    pub fn stall_detected(role: &str, task: Option<u32>, stall_duration_secs: u64) -> Self {
        Self {
            role: Some(role.into()),
            task: task.map(|id| id.to_string()),
            uptime_secs: Some(stall_duration_secs),
            ..Self::base("stall_detected")
        }
    }

    /// Record a backend health state change for an agent.
    ///
    /// `reason` encodes the transition, e.g. "healthy→unreachable".
    pub fn health_changed(role: &str, reason: &str) -> Self {
        Self {
            role: Some(role.into()),
            reason: Some(reason.into()),
            ..Self::base("health_changed")
        }
    }

    pub fn message_routed(from: &str, to: &str) -> Self {
        Self {
            from: Some(from.into()),
            to: Some(to.into()),
            ..Self::base("message_routed")
        }
    }

    pub fn agent_spawned(role: &str) -> Self {
        Self {
            role: Some(role.into()),
            ..Self::base("agent_spawned")
        }
    }

    pub fn agent_restarted(role: &str, task: &str, reason: &str, restart_count: u32) -> Self {
        Self {
            role: Some(role.into()),
            task: Some(task.into()),
            reason: Some(reason.into()),
            restart_count: Some(restart_count),
            ..Self::base("agent_restarted")
        }
    }

    pub fn task_resumed(role: &str, task: &str, reason: &str, restart_count: u32) -> Self {
        Self {
            role: Some(role.into()),
            task: Some(task.into()),
            reason: Some(reason.into()),
            restart_count: Some(restart_count),
            ..Self::base("task_resumed")
        }
    }

    pub fn delivery_failed(role: &str, from: &str, reason: &str) -> Self {
        Self {
            role: Some(role.into()),
            from: Some(from.into()),
            reason: Some(reason.into()),
            ..Self::base("delivery_failed")
        }
    }

    pub fn task_auto_merged(
        engineer: &str,
        task: &str,
        confidence: f64,
        files_changed: usize,
        lines_changed: usize,
    ) -> Self {
        Self {
            role: Some(engineer.into()),
            task: Some(task.into()),
            load: Some(confidence),
            reason: Some(format!("files={} lines={}", files_changed, lines_changed)),
            ..Self::base("task_auto_merged")
        }
    }

    pub fn task_manual_merged(task: &str) -> Self {
        Self {
            task: Some(task.into()),
            ..Self::base("task_manual_merged")
        }
    }

    /// Emitted for every completed task to record its merge confidence score.
    pub fn merge_confidence_scored(info: &MergeConfidenceInfo<'_>) -> Self {
        let detail = format!(
            "files={} lines={} migrations={} config={} renames={}",
            info.files_changed,
            info.lines_changed,
            info.has_migrations,
            info.has_config_changes,
            info.rename_count
        );
        Self {
            role: Some(info.engineer.into()),
            task: Some(info.task.into()),
            load: Some(info.confidence),
            reason: Some(detail),
            ..Self::base("merge_confidence_scored")
        }
    }

    pub fn review_escalated_by_role(role: &str, task: &str) -> Self {
        Self {
            role: Some(role.into()),
            task: Some(task.into()),
            ..Self::base("review_escalated")
        }
    }

    pub fn pipeline_starvation_detected(idle_engineers: usize, todo_tasks: usize) -> Self {
        Self {
            reason: Some(format!(
                "idle_engineers={idle_engineers} todo_tasks={todo_tasks}"
            )),
            ..Self::base("pipeline_starvation_detected")
        }
    }

    pub fn task_reworked(role: &str, task: &str) -> Self {
        Self {
            role: Some(role.into()),
            task: Some(task.into()),
            ..Self::base("task_reworked")
        }
    }

    pub fn task_recycled(task_id: u32, cron_expr: &str) -> Self {
        Self {
            task: Some(format!("#{task_id}")),
            reason: Some(cron_expr.into()),
            ..Self::base("task_recycled")
        }
    }

    pub fn barrier_artifact_created(role: &str, filename: &str, content_hash: &str) -> Self {
        Self {
            role: Some(role.into()),
            filename: Some(filename.into()),
            content_hash: Some(content_hash.into()),
            ..Self::base("barrier_artifact_created")
        }
    }

    pub fn barrier_artifact_read(role: &str, filename: &str, content_hash: &str) -> Self {
        Self {
            role: Some(role.into()),
            filename: Some(filename.into()),
            content_hash: Some(content_hash.into()),
            ..Self::base("barrier_artifact_read")
        }
    }

    pub fn barrier_violation_attempt(role: &str, filename: &str, reason: &str) -> Self {
        Self {
            role: Some(role.into()),
            filename: Some(filename.into()),
            reason: Some(reason.into()),
            ..Self::base("barrier_violation_attempt")
        }
    }

    pub fn load_snapshot(working_members: u32, total_members: u32, session_running: bool) -> Self {
        let load = if total_members == 0 {
            0.0
        } else {
            working_members as f64 / total_members as f64
        };
        Self {
            load: Some(load),
            working_members: Some(working_members),
            total_members: Some(total_members),
            session_running: Some(session_running),
            ..Self::base("load_snapshot")
        }
    }

    pub fn parity_updated(summary: &crate::team::parity::ParitySummary) -> Self {
        Self {
            load: Some(summary.overall_parity_pct as f64 / 100.0),
            reason: Some(format!(
                "total={} spec={} tests={} implementation={} verified_pass={} verified_fail={}",
                summary.total_behaviors,
                summary.spec_complete,
                summary.tests_complete,
                summary.implementation_complete,
                summary.verified_pass,
                summary.verified_fail
            )),
            ..Self::base("parity_updated")
        }
    }

    pub fn worktree_reconciled(role: &str, branch: &str) -> Self {
        Self {
            role: Some(role.into()),
            reason: Some(format!("branch '{branch}' merged into main")),
            ..Self::base("worktree_reconciled")
        }
    }

    /// Emitted when the daemon reconciles a topology change.
    ///
    /// `reason` contains a human-readable summary (e.g. "+2 added, -1 removed").
    pub fn topology_changed(added: u32, removed: u32, reason: &str) -> Self {
        Self {
            working_members: Some(added),
            total_members: Some(removed),
            reason: Some(reason.into()),
            ..Self::base("topology_changed")
        }
    }

    /// Emitted when an agent is removed during a scale-down.
    pub fn agent_removed(role: &str, reason: &str) -> Self {
        Self {
            role: Some(role.into()),
            reason: Some(reason.into()),
            ..Self::base("agent_removed")
        }
    }
}

pub struct EventSink {
    writer: Box<dyn Write + Send>,
    path: PathBuf,
    max_bytes: Option<u64>,
}

impl EventSink {
    pub fn new(path: &Path) -> Result<Self> {
        Self::new_with_max_bytes(path, DEFAULT_EVENT_LOG_MAX_BYTES)
    }

    pub fn new_with_max_bytes(path: &Path, max_bytes: u64) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        rotate_event_log_if_needed(path, max_bytes, 0)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed to open event sink: {}", path.display()))?;
        Ok(Self {
            writer: Box::new(BufWriter::new(file)),
            path: path.to_path_buf(),
            max_bytes: Some(max_bytes),
        })
    }

    #[cfg(test)]
    pub(crate) fn from_writer(path: &Path, writer: impl Write + Send + 'static) -> Self {
        Self {
            writer: Box::new(writer),
            path: path.to_path_buf(),
            max_bytes: None,
        }
    }

    pub fn emit(&mut self, event: TeamEvent) -> Result<()> {
        let json = serde_json::to_string(&event)?;
        self.rotate_if_needed((json.len() + 1) as u64)?;
        writeln!(self.writer, "{json}")?;
        self.writer.flush()?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn rotate_if_needed(&mut self, next_entry_bytes: u64) -> Result<()> {
        let Some(max_bytes) = self.max_bytes else {
            return Ok(());
        };
        self.writer.flush()?;
        if rotate_event_log_if_needed(&self.path, max_bytes, next_entry_bytes)? {
            self.writer = Box::new(BufWriter::new(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)
                    .with_context(|| {
                        format!("failed to reopen event sink: {}", self.path.display())
                    })?,
            ));
        }
        Ok(())
    }
}

fn rotated_event_log_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.1", path.display()))
}

fn rotate_event_log_if_needed(path: &Path, max_bytes: u64, next_entry_bytes: u64) -> Result<bool> {
    let len = match fs::metadata(path) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to stat {}", path.display()));
        }
    };

    if len == 0 {
        return Ok(false);
    }

    if len.saturating_add(next_entry_bytes) <= max_bytes {
        return Ok(false);
    }

    let rotated = rotated_event_log_path(path);
    if rotated.exists() {
        fs::remove_file(&rotated)
            .with_context(|| format!("failed to remove {}", rotated.display()))?;
    }
    fs::rename(path, &rotated).with_context(|| {
        format!(
            "failed to rotate event log {} to {}",
            path.display(),
            rotated.display()
        )
    })?;
    Ok(true)
}

pub fn read_events(path: &Path) -> Result<Vec<TeamEvent>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(path).context("failed to read event log")?;
    let mut events = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(event) = serde_json::from_str::<TeamEvent>(line) {
            events.push(event);
        }
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::thread;

    use super::*;

    #[test]
    fn event_serializes_to_json() {
        let event = TeamEvent::task_assigned("eng-1-1", "fix auth bug");
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"task_assigned\""));
        assert!(json.contains("\"role\":\"eng-1-1\""));
        assert!(json.contains("\"task\":\"fix auth bug\""));
        assert!(json.contains("\"ts\":"));
    }

    #[test]
    fn optional_fields_omitted() {
        let event = TeamEvent::daemon_started();
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("\"role\""));
        assert!(!json.contains("\"task\""));
    }

    #[test]
    fn event_sink_writes_jsonl() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        let mut sink = EventSink::new(&path).unwrap();
        sink.emit(TeamEvent::daemon_started()).unwrap();
        sink.emit(TeamEvent::task_assigned("eng-1", "fix bug"))
            .unwrap();
        sink.emit(TeamEvent::daemon_stopped()).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("daemon_started"));
        assert!(lines[1].contains("task_assigned"));
        assert!(lines[2].contains("daemon_stopped"));
    }

    #[test]
    fn all_event_variants_serialize_with_correct_event_field() {
        let variants: Vec<(&str, TeamEvent)> = vec![
            ("daemon_started", TeamEvent::daemon_started()),
            ("daemon_reloading", TeamEvent::daemon_reloading()),
            ("daemon_reloaded", TeamEvent::daemon_reloaded()),
            ("daemon_stopped", TeamEvent::daemon_stopped()),
            (
                "daemon_stopped",
                TeamEvent::daemon_stopped_with_reason("signal", 3600),
            ),
            ("daemon_heartbeat", TeamEvent::daemon_heartbeat(120)),
            (
                "context_exhausted",
                TeamEvent::context_exhausted("eng-1", Some(42), Some(1_024)),
            ),
            (
                "loop_step_error",
                TeamEvent::loop_step_error("poll_watchers", "tmux error"),
            ),
            (
                "daemon_panic",
                TeamEvent::daemon_panic("index out of bounds"),
            ),
            ("task_assigned", TeamEvent::task_assigned("eng-1", "task")),
            (
                "cwd_corrected",
                TeamEvent::cwd_corrected("eng-1", "/tmp/worktree"),
            ),
            (
                "task_escalated",
                TeamEvent::task_escalated("eng-1", "task", None),
            ),
            ("task_unblocked", TeamEvent::task_unblocked("eng-1", "task")),
            (
                "performance_regression",
                TeamEvent::performance_regression("42", "runtime_ms=1300 avg_ms=1000 pct=30"),
            ),
            (
                "task_completed",
                TeamEvent::task_completed("eng-1", Some("42")),
            ),
            ("standup_generated", TeamEvent::standup_generated("manager")),
            ("retro_generated", TeamEvent::retro_generated()),
            (
                "pattern_detected",
                TeamEvent::pattern_detected("merge_conflict_recurrence", 5),
            ),
            ("member_crashed", TeamEvent::member_crashed("eng-1", true)),
            ("pane_death", TeamEvent::pane_death("eng-1")),
            ("pane_respawned", TeamEvent::pane_respawned("eng-1")),
            (
                "context_pressure_warning",
                TeamEvent::context_pressure_warning("eng-1", Some(42), 400_000, 512_000),
            ),
            (
                "planning_cycle_triggered",
                TeamEvent::planning_cycle_triggered("architect", 3, "todo=0, backlog=1"),
            ),
            (
                "planning_cycle_completed",
                TeamEvent::planning_cycle_completed("architect", 2, 14, true, None),
            ),
            (
                "board_task_archived",
                TeamEvent::board_task_archived("42", Some("eng-1")),
            ),
            ("message_routed", TeamEvent::message_routed("a", "b")),
            ("agent_spawned", TeamEvent::agent_spawned("eng-1")),
            (
                "agent_restarted",
                TeamEvent::agent_restarted("eng-1", "42", "context_exhausted", 1),
            ),
            (
                "delivery_failed",
                TeamEvent::delivery_failed("eng-1", "manager", "message marker missing"),
            ),
            (
                "task_auto_merged",
                TeamEvent::task_auto_merged("eng-1", "42", 0.95, 2, 30),
            ),
            ("task_manual_merged", TeamEvent::task_manual_merged("42")),
            (
                "merge_confidence_scored",
                TeamEvent::merge_confidence_scored(&MergeConfidenceInfo {
                    engineer: "eng-1",
                    task: "42",
                    confidence: 0.85,
                    files_changed: 3,
                    lines_changed: 50,
                    has_migrations: false,
                    has_config_changes: false,
                    rename_count: 0,
                }),
            ),
            (
                "review_nudge_sent",
                TeamEvent::review_nudge_sent("manager", "42"),
            ),
            (
                "review_escalated",
                TeamEvent::review_escalated("42", "stale review"),
            ),
            (
                "pipeline_starvation_detected",
                TeamEvent::pipeline_starvation_detected(3, 0),
            ),
            (
                "state_reconciliation",
                TeamEvent::state_reconciliation(Some("eng-1"), Some("42"), "adopt"),
            ),
            ("task_reworked", TeamEvent::task_reworked("eng-1", "42")),
            ("load_snapshot", TeamEvent::load_snapshot(2, 5, true)),
            (
                "parity_updated",
                TeamEvent::parity_updated(&crate::team::parity::ParitySummary {
                    total_behaviors: 10,
                    spec_complete: 8,
                    tests_complete: 6,
                    implementation_complete: 5,
                    verified_pass: 4,
                    verified_fail: 1,
                    overall_parity_pct: 40,
                }),
            ),
            (
                "worktree_reconciled",
                TeamEvent::worktree_reconciled("eng-1", "eng-1/42"),
            ),
        ];
        for (expected_event, event) in &variants {
            let json = serde_json::to_string(event).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed["event"].as_str().unwrap(), *expected_event);
            assert!(parsed["ts"].as_u64().is_some());
        }
    }

    #[test]
    fn load_snapshot_serializes_optional_metrics() {
        let event = TeamEvent::load_snapshot(3, 7, false);
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"load_snapshot\""));
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["load"].as_f64().unwrap(), 3.0 / 7.0);
        assert_eq!(parsed["working_members"].as_u64().unwrap(), 3);
        assert_eq!(parsed["total_members"].as_u64().unwrap(), 7);
        assert!(!parsed["session_running"].as_bool().unwrap());
    }

    #[test]
    fn parity_updated_serializes_summary_metrics() {
        let event = TeamEvent::parity_updated(&crate::team::parity::ParitySummary {
            total_behaviors: 10,
            spec_complete: 8,
            tests_complete: 6,
            implementation_complete: 5,
            verified_pass: 4,
            verified_fail: 1,
            overall_parity_pct: 40,
        });
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["event"].as_str().unwrap(), "parity_updated");
        assert_eq!(parsed["load"].as_f64().unwrap(), 0.4);
        let reason = parsed["reason"].as_str().unwrap();
        assert!(reason.contains("total=10"));
        assert!(reason.contains("spec=8"));
        assert!(reason.contains("verified_pass=4"));
    }

    #[test]
    fn read_events_parses_all_known_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        let mut sink = EventSink::new(&path).unwrap();
        sink.emit(TeamEvent::daemon_started()).unwrap();
        sink.emit(TeamEvent::load_snapshot(1, 4, true)).unwrap();
        sink.emit(TeamEvent::load_snapshot(2, 4, true)).unwrap();

        let events = read_events(&path).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[1].event, "load_snapshot");
        assert_eq!(events[1].working_members, Some(1));
        assert_eq!(events[2].total_members, Some(4));
    }

    #[test]
    fn event_sink_appends_to_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");

        // Write one event and close the sink
        {
            let mut sink = EventSink::new(&path).unwrap();
            sink.emit(TeamEvent::daemon_started()).unwrap();
        }

        // Open again and append another
        {
            let mut sink = EventSink::new(&path).unwrap();
            sink.emit(TeamEvent::daemon_stopped()).unwrap();
        }

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("daemon_started"));
        assert!(lines[1].contains("daemon_stopped"));
    }

    #[test]
    fn event_with_special_characters_in_task() {
        let event = TeamEvent::task_assigned("eng-1", "fix: \"quotes\" & <angles> / \\ newline\n");
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let task_val = parsed["task"].as_str().unwrap();
        assert!(task_val.contains("\"quotes\""));
        assert!(task_val.contains("<angles>"));
    }

    #[test]
    fn task_escalated_serializes_role_and_task() {
        let event = TeamEvent::task_escalated("eng-1-1", "42", Some("tests_failed"));
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"task_escalated\""));
        assert!(json.contains("\"role\":\"eng-1-1\""));
        assert!(json.contains("\"task\":\"42\""));
        assert!(json.contains("\"reason\":\"tests_failed\""));
    }

    #[test]
    fn cwd_corrected_serializes_role_and_reason() {
        let event = TeamEvent::cwd_corrected("eng-1-1", "/tmp/worktree");
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["event"].as_str().unwrap(), "cwd_corrected");
        assert_eq!(parsed["role"].as_str().unwrap(), "eng-1-1");
        assert_eq!(parsed["reason"].as_str().unwrap(), "/tmp/worktree");
    }

    #[test]
    fn pane_death_serializes_role() {
        let event = TeamEvent::pane_death("eng-1-1");
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["event"].as_str().unwrap(), "pane_death");
        assert_eq!(parsed["role"].as_str().unwrap(), "eng-1-1");
    }

    #[test]
    fn pane_respawned_serializes_role() {
        let event = TeamEvent::pane_respawned("eng-1-1");
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["event"].as_str().unwrap(), "pane_respawned");
        assert_eq!(parsed["role"].as_str().unwrap(), "eng-1-1");
    }

    #[test]
    fn agent_restarted_includes_reason_task_and_count() {
        let event = TeamEvent::agent_restarted("eng-1-2", "67", "context_exhausted", 2);
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["event"].as_str().unwrap(), "agent_restarted");
        assert_eq!(parsed["role"].as_str().unwrap(), "eng-1-2");
        assert_eq!(parsed["task"].as_str().unwrap(), "67");
        assert_eq!(parsed["reason"].as_str().unwrap(), "context_exhausted");
        assert_eq!(parsed["restart_count"].as_u64().unwrap(), 2);
    }

    #[test]
    fn context_pressure_warning_includes_threshold_and_output_bytes() {
        let event = TeamEvent::context_pressure_warning("eng-1-2", Some(67), 420_000, 512_000);
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(
            parsed["event"].as_str().unwrap(),
            "context_pressure_warning"
        );
        assert_eq!(parsed["role"].as_str().unwrap(), "eng-1-2");
        assert_eq!(parsed["task"].as_str().unwrap(), "67");
        assert_eq!(parsed["output_bytes"].as_u64().unwrap(), 420_000);
        assert_eq!(parsed["reason"].as_str().unwrap(), "threshold_bytes=512000");
    }

    #[test]
    fn planning_cycle_completed_includes_latency_status_and_created_count() {
        let event =
            TeamEvent::planning_cycle_completed("architect", 4, 19, false, Some("parse failed"));
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(
            parsed["event"].as_str().unwrap(),
            "planning_cycle_completed"
        );
        assert_eq!(parsed["role"].as_str().unwrap(), "architect");
        assert_eq!(parsed["restart_count"].as_u64().unwrap(), 4);
        assert_eq!(parsed["uptime_secs"].as_u64().unwrap(), 19);
        assert_eq!(parsed["reason"].as_str().unwrap(), "failure");
        assert_eq!(parsed["error"].as_str().unwrap(), "parse failed");
    }

    #[test]
    fn board_task_archived_includes_task_and_role() {
        let event = TeamEvent::board_task_archived("88", Some("eng-1-2"));
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["event"].as_str().unwrap(), "board_task_archived");
        assert_eq!(parsed["task"].as_str().unwrap(), "88");
        assert_eq!(parsed["role"].as_str().unwrap(), "eng-1-2");
    }

    #[test]
    fn delivery_failed_includes_role_sender_and_reason() {
        let event = TeamEvent::delivery_failed("eng-1-2", "manager", "message marker missing");
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["event"].as_str().unwrap(), "delivery_failed");
        assert_eq!(parsed["role"].as_str().unwrap(), "eng-1-2");
        assert_eq!(parsed["from"].as_str().unwrap(), "manager");
        assert_eq!(parsed["reason"].as_str().unwrap(), "message marker missing");
    }

    #[test]
    fn task_unblocked_serializes_role_and_task() {
        let event = TeamEvent::task_unblocked("eng-1-1", "42");
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"task_unblocked\""));
        assert!(json.contains("\"role\":\"eng-1-1\""));
        assert!(json.contains("\"task\":\"42\""));
    }

    #[test]
    fn merge_confidence_scored_includes_all_fields() {
        let event = TeamEvent::merge_confidence_scored(&MergeConfidenceInfo {
            engineer: "eng-1-1",
            task: "42",
            confidence: 0.85,
            files_changed: 3,
            lines_changed: 50,
            has_migrations: true,
            has_config_changes: false,
            rename_count: 1,
        });
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["event"].as_str().unwrap(), "merge_confidence_scored");
        assert_eq!(parsed["role"].as_str().unwrap(), "eng-1-1");
        assert_eq!(parsed["task"].as_str().unwrap(), "42");
        assert!((parsed["load"].as_f64().unwrap() - 0.85).abs() < 0.001);
        let reason = parsed["reason"].as_str().unwrap();
        assert!(reason.contains("files=3"));
        assert!(reason.contains("lines=50"));
        assert!(reason.contains("migrations=true"));
        assert!(reason.contains("config=false"));
        assert!(reason.contains("renames=1"));
    }

    #[test]
    fn performance_regression_serializes_task_and_reason() {
        let event = TeamEvent::performance_regression("42", "runtime_ms=1300 avg_ms=1000 pct=30");
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["event"].as_str().unwrap(), "performance_regression");
        assert_eq!(parsed["task"].as_str().unwrap(), "42");
        assert_eq!(
            parsed["reason"].as_str().unwrap(),
            "runtime_ms=1300 avg_ms=1000 pct=30"
        );
    }

    #[test]
    fn daemon_stopped_with_reason_includes_fields() {
        let event = TeamEvent::daemon_stopped_with_reason("signal", 7200);
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["reason"].as_str().unwrap(), "signal");
        assert_eq!(parsed["uptime_secs"].as_u64().unwrap(), 7200);
    }

    #[test]
    fn heartbeat_includes_uptime() {
        let event = TeamEvent::daemon_heartbeat(600);
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["event"].as_str().unwrap(), "daemon_heartbeat");
        assert_eq!(parsed["uptime_secs"].as_u64().unwrap(), 600);
        // No reason/step/error fields
        assert!(parsed.get("reason").is_none());
        assert!(parsed.get("step").is_none());
    }

    #[test]
    fn context_exhausted_includes_role_task_and_session_size() {
        let event = TeamEvent::context_exhausted("eng-1", Some(77), Some(4096));
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["event"].as_str().unwrap(), "context_exhausted");
        assert_eq!(parsed["role"].as_str().unwrap(), "eng-1");
        assert_eq!(parsed["task"].as_str().unwrap(), "77");
        assert_eq!(parsed["session_size_bytes"].as_u64().unwrap(), 4096);
    }

    #[test]
    fn loop_step_error_includes_step_and_error() {
        let event = TeamEvent::loop_step_error("poll_watchers", "connection refused");
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["step"].as_str().unwrap(), "poll_watchers");
        assert_eq!(parsed["error"].as_str().unwrap(), "connection refused");
    }

    #[test]
    fn daemon_panic_includes_reason() {
        let event = TeamEvent::daemon_panic("index out of bounds");
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["event"].as_str().unwrap(), "daemon_panic");
        assert_eq!(parsed["reason"].as_str().unwrap(), "index out of bounds");
    }

    #[test]
    fn new_fields_omitted_from_basic_events() {
        let event = TeamEvent::daemon_started();
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("\"reason\""));
        assert!(!json.contains("\"step\""));
        assert!(!json.contains("\"error\""));
        assert!(!json.contains("\"uptime_secs\""));
        assert!(!json.contains("\"restart_count\""));
    }

    #[test]
    fn pattern_detected_includes_reason_payload() {
        let event = TeamEvent::pattern_detected("escalation_cluster", 6);
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["event"].as_str().unwrap(), "pattern_detected");
        assert_eq!(parsed["reason"].as_str().unwrap(), "escalation_cluster:6");
    }

    #[test]
    fn event_sink_creates_parent_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("deep").join("nested").join("events.jsonl");
        let mut sink = EventSink::new(&path).unwrap();
        sink.emit(TeamEvent::daemon_started()).unwrap();
        assert!(path.exists());
        assert_eq!(sink.path(), path);
    }

    #[test]
    fn event_sink_rotates_oversized_log_on_open() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        fs::write(&path, "0123456789").unwrap();

        let mut sink = EventSink::new_with_max_bytes(&path, 5).unwrap();
        sink.emit(TeamEvent::daemon_started()).unwrap();

        let rotated = rotated_event_log_path(&path);
        assert_eq!(fs::read_to_string(&rotated).unwrap(), "0123456789");
        let current = fs::read_to_string(&path).unwrap();
        assert!(current.contains("daemon_started"));
    }

    #[test]
    fn event_sink_rotates_before_write_that_would_exceed_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        let first_line = "{\"event\":\"first\"}\n";
        fs::write(&path, first_line).unwrap();

        let mut sink = EventSink::new_with_max_bytes(&path, first_line.len() as u64 + 10).unwrap();
        sink.emit(TeamEvent::task_assigned(
            "eng-1",
            "this assignment is long enough to rotate",
        ))
        .unwrap();

        let rotated = rotated_event_log_path(&path);
        assert_eq!(fs::read_to_string(&rotated).unwrap(), first_line);
        let current = fs::read_to_string(&path).unwrap();
        assert!(current.contains("task_assigned"));
        assert!(!current.contains("\"event\":\"first\""));
    }

    #[test]
    fn event_round_trip_preserves_fields_for_agent_restarted() {
        let original = TeamEvent::agent_restarted("eng-1", "42", "context_exhausted", 3);

        let json = serde_json::to_string(&original).unwrap();
        let parsed: TeamEvent = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.event, "agent_restarted");
        assert_eq!(parsed.role.as_deref(), Some("eng-1"));
        assert_eq!(parsed.task.as_deref(), Some("42"));
        assert_eq!(parsed.reason.as_deref(), Some("context_exhausted"));
        assert_eq!(parsed.restart_count, Some(3));
        assert_eq!(parsed.ts, original.ts);
    }

    #[test]
    fn event_round_trip_preserves_fields_for_load_snapshot() {
        let original = TeamEvent::load_snapshot(4, 8, true);

        let json = serde_json::to_string(&original).unwrap();
        let parsed: TeamEvent = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.event, "load_snapshot");
        assert_eq!(parsed.working_members, Some(4));
        assert_eq!(parsed.total_members, Some(8));
        assert_eq!(parsed.session_running, Some(true));
        assert_eq!(parsed.load, Some(0.5));
        assert_eq!(parsed.ts, original.ts);
    }

    #[test]
    fn event_round_trip_preserves_fields_for_delivery_failed() {
        let original = TeamEvent::delivery_failed("eng-2", "manager", "marker missing");

        let json = serde_json::to_string(&original).unwrap();
        let parsed: TeamEvent = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.event, "delivery_failed");
        assert_eq!(parsed.role.as_deref(), Some("eng-2"));
        assert_eq!(parsed.from.as_deref(), Some("manager"));
        assert_eq!(parsed.reason.as_deref(), Some("marker missing"));
        assert_eq!(parsed.ts, original.ts);
    }

    #[test]
    fn read_events_skips_blank_and_malformed_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        fs::write(
            &path,
            [
                "",
                "{\"event\":\"daemon_started\",\"ts\":1}",
                "not-json",
                "   ",
                "{\"event\":\"daemon_stopped\",\"ts\":2}",
            ]
            .join("\n"),
        )
        .unwrap();

        let events = read_events(&path).unwrap();

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event, "daemon_started");
        assert_eq!(events[1].event, "daemon_stopped");
    }

    #[test]
    fn rotate_event_log_if_needed_returns_false_for_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");

        let rotated = rotate_event_log_if_needed(&path, 128, 0).unwrap();

        assert!(!rotated);
        assert!(!rotated_event_log_path(&path).exists());
    }

    #[test]
    fn rotate_event_log_if_needed_returns_false_for_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        fs::write(&path, "").unwrap();

        let rotated = rotate_event_log_if_needed(&path, 1, 1).unwrap();

        assert!(!rotated);
        assert!(path.exists());
        assert!(!rotated_event_log_path(&path).exists());
    }

    #[test]
    fn rotate_event_log_if_needed_replaces_existing_rotated_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        let rotated_path = rotated_event_log_path(&path);
        fs::write(&path, "current-events").unwrap();
        fs::write(&rotated_path, "old-rotated-events").unwrap();

        let rotated = rotate_event_log_if_needed(&path, 5, 0).unwrap();

        assert!(rotated);
        assert_eq!(fs::read_to_string(&rotated_path).unwrap(), "current-events");
    }

    #[test]
    fn concurrent_event_sinks_append_without_losing_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let path = Arc::new(tmp.path().join("events.jsonl"));
        let ready = Arc::new(std::sync::Barrier::new(5));
        let errors = Arc::new(Mutex::new(Vec::<String>::new()));
        let mut handles = Vec::new();

        for idx in 0..4 {
            let path = Arc::clone(&path);
            let ready = Arc::clone(&ready);
            let errors = Arc::clone(&errors);
            handles.push(thread::spawn(move || {
                ready.wait();
                let result = (|| -> Result<()> {
                    let mut sink = EventSink::new(&path)?;
                    sink.emit(TeamEvent::task_assigned(
                        &format!("eng-{idx}"),
                        &format!("task-{idx}"),
                    ))?;
                    Ok(())
                })();
                if let Err(error) = result {
                    errors.lock().unwrap().push(error.to_string());
                }
            }));
        }

        ready.wait();
        for handle in handles {
            handle.join().unwrap();
        }

        assert!(errors.lock().unwrap().is_empty());
        let events = read_events(&path).unwrap();
        assert_eq!(events.len(), 4);
        for idx in 0..4 {
            assert!(events
                .iter()
                .any(|event| event.role.as_deref() == Some(&format!("eng-{idx}"))));
        }
    }

    #[test]
    fn read_events_handles_large_log_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        let mut sink = EventSink::new(&path).unwrap();

        for idx in 0..512 {
            sink.emit(TeamEvent::task_assigned(
                &format!("eng-{idx}"),
                &"x".repeat(128),
            ))
            .unwrap();
        }

        let events = read_events(&path).unwrap();

        assert_eq!(events.len(), 512);
        assert_eq!(events.first().unwrap().event, "task_assigned");
        assert_eq!(events.last().unwrap().event, "task_assigned");
    }

    fn production_unwrap_expect_count(source: &str) -> usize {
        let prod = if let Some(pos) = source.find("\n#[cfg(test)]\nmod tests") {
            &source[..pos]
        } else {
            source
        };
        prod.lines()
            .filter(|line| {
                let trimmed = line.trim();
                !trimmed.starts_with("#[cfg(test)]")
                    && (trimmed.contains(".unwrap(") || trimmed.contains(".expect("))
            })
            .count()
    }

    #[test]
    fn task_completed_includes_task_id() {
        let event = TeamEvent::task_completed("eng-1", Some("42"));
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["event"].as_str().unwrap(), "task_completed");
        assert_eq!(parsed["role"].as_str().unwrap(), "eng-1");
        assert_eq!(parsed["task"].as_str().unwrap(), "42");
    }

    #[test]
    fn task_completed_without_task_id_omits_task_field() {
        let event = TeamEvent::task_completed("eng-1", None);
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"task_completed\""));
        assert!(json.contains("\"role\":\"eng-1\""));
        assert!(!json.contains("\"task\""));
    }

    #[test]
    fn task_escalated_without_reason_omits_reason_field() {
        let event = TeamEvent::task_escalated("eng-1", "42", None);
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["event"].as_str().unwrap(), "task_escalated");
        assert_eq!(parsed["task"].as_str().unwrap(), "42");
        assert!(parsed.get("reason").is_none());
    }

    #[test]
    fn task_escalated_with_reason_includes_reason_field() {
        let event = TeamEvent::task_escalated("eng-1", "42", Some("merge_conflict"));
        let json = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["event"].as_str().unwrap(), "task_escalated");
        assert_eq!(parsed["task"].as_str().unwrap(), "42");
        assert_eq!(parsed["reason"].as_str().unwrap(), "merge_conflict");
    }

    #[test]
    fn task_completed_round_trip_preserves_task_id() {
        let original = TeamEvent::task_completed("eng-1", Some("99"));
        let json = serde_json::to_string(&original).unwrap();
        let parsed: TeamEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event, "task_completed");
        assert_eq!(parsed.role.as_deref(), Some("eng-1"));
        assert_eq!(parsed.task.as_deref(), Some("99"));
    }

    #[test]
    fn task_escalated_round_trip_preserves_reason() {
        let original = TeamEvent::task_escalated("eng-1", "42", Some("context_exhausted"));
        let json = serde_json::to_string(&original).unwrap();
        let parsed: TeamEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.event, "task_escalated");
        assert_eq!(parsed.role.as_deref(), Some("eng-1"));
        assert_eq!(parsed.task.as_deref(), Some("42"));
        assert_eq!(parsed.reason.as_deref(), Some("context_exhausted"));
    }

    #[test]
    fn production_events_has_no_unwrap_or_expect_calls() {
        let src = include_str!("events.rs");
        assert_eq!(
            production_unwrap_expect_count(src),
            0,
            "production events.rs should avoid unwrap/expect"
        );
    }

    #[test]
    fn stall_detected_event_fields() {
        let event = TeamEvent::stall_detected("eng-1-1", Some(42), 300);
        assert_eq!(event.event, "stall_detected");
        assert_eq!(event.role.as_deref(), Some("eng-1-1"));
        assert_eq!(event.task.as_deref(), Some("42"));
        assert_eq!(event.uptime_secs, Some(300));
    }

    #[test]
    fn stall_detected_event_without_task() {
        let event = TeamEvent::stall_detected("eng-1-1", None, 600);
        assert_eq!(event.event, "stall_detected");
        assert_eq!(event.role.as_deref(), Some("eng-1-1"));
        assert!(event.task.is_none());
        assert_eq!(event.uptime_secs, Some(600));
    }

    #[test]
    fn stall_detected_event_serializes_to_jsonl() {
        let event = TeamEvent::stall_detected("eng-1-1", Some(42), 300);
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"stall_detected\""));
        assert!(json.contains("\"eng-1-1\""));
        assert!(json.contains("\"42\""));
    }

    #[test]
    fn health_changed_event_fields() {
        let event = TeamEvent::health_changed("eng-1-1", "healthy→unreachable");
        assert_eq!(event.event, "health_changed");
        assert_eq!(event.role.as_deref(), Some("eng-1-1"));
        assert_eq!(event.reason.as_deref(), Some("healthy→unreachable"));
    }

    #[test]
    fn health_changed_event_serializes_to_jsonl() {
        let event = TeamEvent::health_changed("eng-1-2", "unreachable→healthy");
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"health_changed\""));
        assert!(json.contains("\"eng-1-2\""));
    }

    // --- Error path and recovery tests (Task #265) ---

    #[test]
    fn event_sink_on_readonly_dir_returns_error() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let tmp = tempfile::tempdir().unwrap();
            let readonly_dir = tmp.path().join("readonly");
            fs::create_dir(&readonly_dir).unwrap();
            fs::set_permissions(&readonly_dir, fs::Permissions::from_mode(0o444)).unwrap();

            let path = readonly_dir.join("subdir").join("events.jsonl");
            let result = EventSink::new(&path);
            assert!(result.is_err());

            // Restore permissions for cleanup
            fs::set_permissions(&readonly_dir, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn read_events_from_nonexistent_file_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("does_not_exist.jsonl");
        let events = read_events(&path).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn read_events_all_malformed_lines_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        fs::write(&path, "not json\nalso not json\n{invalid}\n").unwrap();
        let events = read_events(&path).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn event_sink_emit_with_failing_writer() {
        struct FailWriter;
        impl Write for FailWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "simulated write failure",
                ))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "simulated flush failure",
                ))
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        let mut sink = EventSink::from_writer(&path, FailWriter);
        let result = sink.emit(TeamEvent::daemon_started());
        assert!(result.is_err());
    }

    #[test]
    fn rotate_event_log_replaces_stale_rotated_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        let rotated = rotated_event_log_path(&path);

        fs::write(&path, "current-data-that-is-large").unwrap();
        fs::write(&rotated, "old-rotated-data").unwrap();

        let did_rotate = rotate_event_log_if_needed(&path, 5, 0).unwrap();
        assert!(did_rotate);
        // Old rotated was replaced with current data
        assert_eq!(
            fs::read_to_string(&rotated).unwrap(),
            "current-data-that-is-large"
        );
        // Current file is now gone (rotated away)
        assert!(!path.exists());
    }

    #[test]
    fn event_sink_handles_zero_max_bytes_rotation() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");

        // With max_bytes=0, any existing content triggers rotation, but empty file doesn't
        fs::write(&path, "").unwrap();
        let did_rotate = rotate_event_log_if_needed(&path, 0, 0).unwrap();
        assert!(!did_rotate); // empty file → no rotation

        fs::write(&path, "x").unwrap();
        let did_rotate = rotate_event_log_if_needed(&path, 0, 0).unwrap();
        assert!(did_rotate); // non-empty file at 0-byte limit → rotation
    }

    #[test]
    fn narration_rejection_event_has_correct_fields() {
        let event = TeamEvent::narration_rejection("eng-1-1", 42, 2);
        assert_eq!(event.event, "narration_rejection");
        assert_eq!(event.role.as_deref(), Some("eng-1-1"));
        assert_eq!(event.task.as_deref(), Some("42"));
        assert_eq!(event.reason.as_deref(), Some("rejection_count=2"));
    }

    #[test]
    fn read_events_partial_json_with_valid_lines_mixed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        // Simulate a truncated write: valid JSON, then partial, then valid
        let content = format!(
            "{}\n{{\"event\":\"trunca\n{}\n",
            r#"{"event":"daemon_started","ts":1}"#, r#"{"event":"daemon_stopped","ts":3}"#
        );
        fs::write(&path, content).unwrap();

        let events = read_events(&path).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event, "daemon_started");
        assert_eq!(events[1].event, "daemon_stopped");
    }
}
