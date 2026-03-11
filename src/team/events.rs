//! Structured JSONL event stream for external consumers.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
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
    pub ts: u64,
}

impl TeamEvent {
    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    pub fn daemon_started() -> Self {
        Self {
            event: "daemon_started".into(),
            role: None,
            task: None,
            recipient: None,
            from: None,
            to: None,
            restart: None,
            ts: Self::now(),
        }
    }

    pub fn daemon_stopped() -> Self {
        Self {
            event: "daemon_stopped".into(),
            role: None,
            task: None,
            recipient: None,
            from: None,
            to: None,
            restart: None,
            ts: Self::now(),
        }
    }

    pub fn task_assigned(role: &str, task: &str) -> Self {
        Self {
            event: "task_assigned".into(),
            role: Some(role.into()),
            task: Some(task.into()),
            recipient: None,
            from: None,
            to: None,
            restart: None,
            ts: Self::now(),
        }
    }

    pub fn task_escalated(role: &str, task: &str) -> Self {
        Self {
            event: "task_escalated".into(),
            role: Some(role.into()),
            task: Some(task.into()),
            recipient: None,
            from: None,
            to: None,
            restart: None,
            ts: Self::now(),
        }
    }

    pub fn task_completed(role: &str) -> Self {
        Self {
            event: "task_completed".into(),
            role: Some(role.into()),
            task: None,
            recipient: None,
            from: None,
            to: None,
            restart: None,
            ts: Self::now(),
        }
    }

    pub fn standup_generated(recipient: &str) -> Self {
        Self {
            event: "standup_generated".into(),
            role: None,
            task: None,
            recipient: Some(recipient.into()),
            from: None,
            to: None,
            restart: None,
            ts: Self::now(),
        }
    }

    pub fn member_crashed(role: &str, restart: bool) -> Self {
        Self {
            event: "member_crashed".into(),
            role: Some(role.into()),
            task: None,
            recipient: None,
            from: None,
            to: None,
            restart: Some(restart),
            ts: Self::now(),
        }
    }

    pub fn message_routed(from: &str, to: &str) -> Self {
        Self {
            event: "message_routed".into(),
            role: None,
            task: None,
            recipient: None,
            from: Some(from.into()),
            to: Some(to.into()),
            restart: None,
            ts: Self::now(),
        }
    }

    pub fn agent_spawned(role: &str) -> Self {
        Self {
            event: "agent_spawned".into(),
            role: Some(role.into()),
            task: None,
            recipient: None,
            from: None,
            to: None,
            restart: None,
            ts: Self::now(),
        }
    }
}

pub struct EventSink {
    writer: BufWriter<File>,
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
            writer: BufWriter::new(file),
            path: path.to_path_buf(),
        })
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
            ("task_assigned", TeamEvent::task_assigned("eng-1", "task")),
            ("task_escalated", TeamEvent::task_escalated("eng-1", "task")),
            ("task_completed", TeamEvent::task_completed("eng-1")),
            ("standup_generated", TeamEvent::standup_generated("manager")),
            ("member_crashed", TeamEvent::member_crashed("eng-1", true)),
            ("message_routed", TeamEvent::message_routed("a", "b")),
            ("agent_spawned", TeamEvent::agent_spawned("eng-1")),
        ];
        for (expected_event, event) in &variants {
            let json = serde_json::to_string(event).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed["event"].as_str().unwrap(), *expected_event);
            assert!(parsed["ts"].as_u64().is_some());
        }
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
    fn event_sink_creates_parent_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("deep").join("nested").join("events.jsonl");
        let mut sink = EventSink::new(&path).unwrap();
        sink.emit(TeamEvent::daemon_started()).unwrap();
        assert!(path.exists());
        assert_eq!(sink.path(), path);
    }
}
