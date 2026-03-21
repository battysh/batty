//! Structured JSONL event stream for external consumers.

use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

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
            load: None,
            working_members: None,
            total_members: None,
            session_running: None,
            reason: None,
            step: None,
            error: None,
            uptime_secs: None,
            ts: Self::now(),
        }
    }

    pub fn daemon_started() -> Self {
        Self::base("daemon_started")
    }

    #[allow(dead_code)]
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

    pub fn task_escalated(role: &str, task: &str) -> Self {
        Self {
            role: Some(role.into()),
            task: Some(task.into()),
            ..Self::base("task_escalated")
        }
    }

    pub fn task_completed(role: &str) -> Self {
        Self {
            role: Some(role.into()),
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

    pub fn member_crashed(role: &str, restart: bool) -> Self {
        Self {
            role: Some(role.into()),
            restart: Some(restart),
            ..Self::base("member_crashed")
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
}

pub struct EventSink {
    writer: Box<dyn Write + Send>,
    path: PathBuf,
}

impl EventSink {
    pub fn new(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed to open event sink: {}", path.display()))?;
        Ok(Self {
            writer: Box::new(BufWriter::new(file)),
            path: path.to_path_buf(),
        })
    }

    #[cfg(test)]
    pub(crate) fn from_writer(path: &Path, writer: impl Write + Send + 'static) -> Self {
        Self {
            writer: Box::new(writer),
            path: path.to_path_buf(),
        }
    }

    pub fn emit(&mut self, event: TeamEvent) -> Result<()> {
        let json = serde_json::to_string(&event)?;
        writeln!(self.writer, "{json}")?;
        self.writer.flush()?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }
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
            ("daemon_stopped", TeamEvent::daemon_stopped()),
            (
                "daemon_stopped",
                TeamEvent::daemon_stopped_with_reason("signal", 3600),
            ),
            ("daemon_heartbeat", TeamEvent::daemon_heartbeat(120)),
            (
                "loop_step_error",
                TeamEvent::loop_step_error("poll_watchers", "tmux error"),
            ),
            (
                "daemon_panic",
                TeamEvent::daemon_panic("index out of bounds"),
            ),
            ("task_assigned", TeamEvent::task_assigned("eng-1", "task")),
            ("task_escalated", TeamEvent::task_escalated("eng-1", "task")),
            ("task_completed", TeamEvent::task_completed("eng-1")),
            ("standup_generated", TeamEvent::standup_generated("manager")),
            ("retro_generated", TeamEvent::retro_generated()),
            ("member_crashed", TeamEvent::member_crashed("eng-1", true)),
            ("message_routed", TeamEvent::message_routed("a", "b")),
            ("agent_spawned", TeamEvent::agent_spawned("eng-1")),
            ("load_snapshot", TeamEvent::load_snapshot(2, 5, true)),
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
        let event = TeamEvent::task_escalated("eng-1-1", "42");
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"event\":\"task_escalated\""));
        assert!(json.contains("\"role\":\"eng-1-1\""));
        assert!(json.contains("\"task\":\"42\""));
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
}
