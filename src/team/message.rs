//! Team message types and tmux injection.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::tmux;

/// A message in the command queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum QueuedCommand {
    #[serde(rename = "send")]
    Send {
        from: String,
        to: String,
        message: String,
    },
    #[serde(rename = "assign")]
    Assign {
        from: String,
        engineer: String,
        task: String,
    },
}

/// Inject a text message into a tmux pane.
///
/// Short messages use send-keys. Long messages use load-buffer + paste-buffer.
pub fn inject_message(pane_id: &str, from: &str, message: &str) -> Result<()> {
    let formatted = format!(
        "\n--- Message from {from} ---\n{message}\n--- end message ---\nTo reply, run: batty send {from} \"<your response>\"\n"
    );

    // Use load-buffer + paste-buffer for text, then send Enter to submit
    tmux::load_buffer(&formatted)?;
    tmux::paste_buffer(pane_id)?;
    // paste-buffer needs time to complete before we press Enter —
    // longer messages need more time for the terminal to process the paste
    let delay_ms = 500 + (formatted.len() as u64 / 100) * 50;
    std::thread::sleep(std::time::Duration::from_millis(delay_ms.min(3000)));
    // Send Enter as a non-literal keypress to submit the pasted text
    tmux::send_keys(pane_id, "", true)?;
    // Second Enter after a short pause to ensure submission
    std::thread::sleep(std::time::Duration::from_millis(300));
    tmux::send_keys(pane_id, "", true)?;
    Ok(())
}

/// Write a command to the queue file.
#[allow(dead_code)] // Legacy queue path retained for compatibility and tests.
pub fn enqueue_command(queue_path: &Path, cmd: &QueuedCommand) -> Result<()> {
    if let Some(parent) = queue_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string(cmd)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(queue_path)
        .with_context(|| format!("failed to open command queue: {}", queue_path.display()))?;
    writeln!(file, "{json}")?;
    Ok(())
}

/// Read and drain all pending commands from the queue file.
#[cfg(test)]
pub fn drain_command_queue(queue_path: &Path) -> Result<Vec<QueuedCommand>> {
    let commands = read_command_queue(queue_path)?;
    if !commands.is_empty() {
        write_command_queue(queue_path, &[])?;
    }
    Ok(commands)
}

/// Read all pending commands from the queue file without clearing it.
pub fn read_command_queue(queue_path: &Path) -> Result<Vec<QueuedCommand>> {
    if !queue_path.exists() {
        return Ok(Vec::new());
    }

    let file = File::open(queue_path)
        .with_context(|| format!("failed to open command queue: {}", queue_path.display()))?;
    let reader = BufReader::new(file);

    let mut commands = Vec::new();
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<QueuedCommand>(trimmed) {
            Ok(cmd) => commands.push(cmd),
            Err(e) => tracing::warn!(line = trimmed, error = %e, "skipping malformed command"),
        }
    }

    Ok(commands)
}

/// Atomically rewrite the command queue with the remaining commands.
pub fn write_command_queue(queue_path: &Path, commands: &[QueuedCommand]) -> Result<()> {
    if let Some(parent) = queue_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let tmp_path = queue_path.with_extension("jsonl.tmp");
    {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)
            .with_context(|| {
                format!("failed to open temp command queue: {}", tmp_path.display())
            })?;
        for cmd in commands {
            let json = serde_json::to_string(cmd)?;
            writeln!(file, "{json}")?;
        }
        file.flush()?;
    }

    std::fs::rename(&tmp_path, queue_path).with_context(|| {
        format!(
            "failed to replace command queue {} with {}",
            queue_path.display(),
            tmp_path.display()
        )
    })?;
    Ok(())
}

/// Resolve the command queue path.
pub fn command_queue_path(project_root: &Path) -> PathBuf {
    project_root
        .join(".batty")
        .join("team_config")
        .join("commands.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn send_command_roundtrip() {
        let cmd = QueuedCommand::Send {
            from: "human".into(),
            to: "architect".into(),
            message: "prioritize auth".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: QueuedCommand = serde_json::from_str(&json).unwrap();
        match parsed {
            QueuedCommand::Send { from, to, message } => {
                assert_eq!(from, "human");
                assert_eq!(to, "architect");
                assert_eq!(message, "prioritize auth");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn assign_command_roundtrip() {
        let cmd = QueuedCommand::Assign {
            from: "black-lead".into(),
            engineer: "eng-1-1".into(),
            task: "fix bug".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: QueuedCommand = serde_json::from_str(&json).unwrap();
        match parsed {
            QueuedCommand::Assign {
                from,
                engineer,
                task,
            } => {
                assert_eq!(from, "black-lead");
                assert_eq!(engineer, "eng-1-1");
                assert_eq!(task, "fix bug");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn enqueue_and_drain() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("commands.jsonl");

        enqueue_command(
            &queue,
            &QueuedCommand::Send {
                from: "human".into(),
                to: "arch".into(),
                message: "hello".into(),
            },
        )
        .unwrap();
        enqueue_command(
            &queue,
            &QueuedCommand::Assign {
                from: "black-lead".into(),
                engineer: "eng-1".into(),
                task: "work".into(),
            },
        )
        .unwrap();

        let commands = drain_command_queue(&queue).unwrap();
        assert_eq!(commands.len(), 2);

        // After drain, queue should be empty
        let commands = drain_command_queue(&queue).unwrap();
        assert!(commands.is_empty());
    }

    #[test]
    fn drain_nonexistent_queue_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("nonexistent.jsonl");
        let commands = drain_command_queue(&queue).unwrap();
        assert!(commands.is_empty());
    }

    #[test]
    fn read_command_queue_keeps_file_contents_intact() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("commands.jsonl");
        enqueue_command(
            &queue,
            &QueuedCommand::Send {
                from: "human".into(),
                to: "arch".into(),
                message: "hello".into(),
            },
        )
        .unwrap();

        let commands = read_command_queue(&queue).unwrap();
        assert_eq!(commands.len(), 1);
        let persisted = std::fs::read_to_string(&queue).unwrap();
        assert!(persisted.contains("\"message\":\"hello\""));
    }

    #[test]
    fn write_command_queue_rewrites_remaining_commands_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("commands.jsonl");
        enqueue_command(
            &queue,
            &QueuedCommand::Send {
                from: "human".into(),
                to: "arch".into(),
                message: "hello".into(),
            },
        )
        .unwrap();

        write_command_queue(
            &queue,
            &[QueuedCommand::Assign {
                from: "manager".into(),
                engineer: "eng-1".into(),
                task: "Task #1".into(),
            }],
        )
        .unwrap();

        let commands = read_command_queue(&queue).unwrap();
        assert_eq!(commands.len(), 1);
        match &commands[0] {
            QueuedCommand::Assign { engineer, task, .. } => {
                assert_eq!(engineer, "eng-1");
                assert_eq!(task, "Task #1");
            }
            other => panic!("expected assign command after rewrite, got {other:?}"),
        }
    }

    #[test]
    fn drain_skips_malformed_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("commands.jsonl");
        std::fs::write(
            &queue,
            "not json\n{\"type\":\"assign\",\"from\":\"manager\",\"engineer\":\"e1\",\"task\":\"t1\"}\n",
        )
        .unwrap();
        let commands = drain_command_queue(&queue).unwrap();
        assert_eq!(commands.len(), 1);
    }

    #[test]
    #[serial]
    fn test_inject_message_empty_message_writes_message_wrapper_to_pane() {
        let session = "batty-test-message-empty";
        let _ = crate::tmux::kill_session(session);

        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("message-empty.log");

        crate::tmux::create_session(session, "cat", &[], "/tmp").unwrap();
        crate::tmux::setup_pipe_pane(session, &log_path).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));

        inject_message(session, "manager", "").unwrap();
        let content = (0..30)
            .find_map(|_| {
                let content = std::fs::read_to_string(&log_path).unwrap_or_default();
                let ready = content.contains("--- Message from manager ---")
                    && content.contains("--- end message ---")
                    && content.contains("batty send manager");
                if ready {
                    Some(content)
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    None
                }
            })
            .unwrap_or_else(|| std::fs::read_to_string(&log_path).unwrap_or_default());
        assert!(content.contains("--- Message from manager ---"));
        assert!(content.contains("--- end message ---"));
        assert!(content.contains("batty send manager"));

        crate::tmux::kill_session(session).unwrap();
    }

    #[test]
    #[serial]
    fn test_inject_message_long_special_message_preserves_content() {
        let session = "batty-test-message-special";
        let _ = crate::tmux::kill_session(session);

        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("message-special.log");
        let repeated = "x".repeat(600);
        let long_message = format!(
            "symbols: !@#$%^&*()[]{{}}<>?/\\\\|~`'\" {}\nline-2",
            repeated
        );

        crate::tmux::create_session(session, "cat", &[], "/tmp").unwrap();
        crate::tmux::setup_pipe_pane(session, &log_path).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));

        inject_message(session, "architect", &long_message).unwrap();
        let content = (0..30)
            .find_map(|_| {
                let content = std::fs::read_to_string(&log_path).unwrap_or_default();
                let ready = content.contains("--- Message from architect ---")
                    && content.contains("symbols: !@#$%^&*()[]{}<>?/\\\\|~`'\"")
                    && content.contains("line-2");
                if ready {
                    Some(content)
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    None
                }
            })
            .unwrap_or_else(|| std::fs::read_to_string(&log_path).unwrap_or_default());
        assert!(content.contains("--- Message from architect ---"));
        assert!(content.contains("symbols: !@#$%^&*()[]{}<>?/\\\\|~`'\""));
        assert!(content.contains(&"x".repeat(200)));
        assert!(content.contains("line-2"));
        assert!(content.contains("batty send architect"));

        crate::tmux::kill_session(session).unwrap();
    }

    #[test]
    fn test_drain_command_queue_skips_unknown_and_incomplete_commands() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("commands.jsonl");
        std::fs::write(
            &queue,
            concat!(
                "{\"type\":\"noop\",\"from\":\"manager\"}\n",
                "{\"type\":\"send\",\"from\":\"manager\",\"message\":\"missing recipient\"}\n",
                "{\"type\":\"assign\",\"from\":\"manager\",\"engineer\":\"eng-1\"}\n",
                "{\"type\":\"send\",\"from\":\"manager\",\"to\":\"architect\",\"message\":\"valid\"}\n",
            ),
        )
        .unwrap();

        let commands = drain_command_queue(&queue).unwrap();

        assert_eq!(commands.len(), 1);
        match &commands[0] {
            QueuedCommand::Send { from, to, message } => {
                assert_eq!(from, "manager");
                assert_eq!(to, "architect");
                assert_eq!(message, "valid");
            }
            other => panic!("expected valid send command, got {other:?}"),
        }
    }
}
