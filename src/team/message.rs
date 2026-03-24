//! Team message types and command queue management.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

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

/// Write a command to the queue file.
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

    // --- Additional serialization edge cases ---

    #[test]
    fn send_command_preserves_multiline_message() {
        let cmd = QueuedCommand::Send {
            from: "manager".into(),
            to: "eng-1".into(),
            message: "line one\nline two\nline three".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: QueuedCommand = serde_json::from_str(&json).unwrap();
        match parsed {
            QueuedCommand::Send { message, .. } => {
                assert_eq!(message, "line one\nline two\nline three");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn send_command_preserves_empty_message() {
        let cmd = QueuedCommand::Send {
            from: "human".into(),
            to: "architect".into(),
            message: "".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: QueuedCommand = serde_json::from_str(&json).unwrap();
        match parsed {
            QueuedCommand::Send { message, .. } => assert!(message.is_empty()),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn send_command_preserves_unicode() {
        let cmd = QueuedCommand::Send {
            from: "人間".into(),
            to: "アーキ".into(),
            message: "日本語テスト 🚀".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: QueuedCommand = serde_json::from_str(&json).unwrap();
        match parsed {
            QueuedCommand::Send { from, to, message } => {
                assert_eq!(from, "人間");
                assert_eq!(to, "アーキ");
                assert_eq!(message, "日本語テスト 🚀");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn assign_command_preserves_special_chars_in_task() {
        let cmd = QueuedCommand::Assign {
            from: "manager".into(),
            engineer: "eng-1-1".into(),
            task: "Task #42: Fix \"auth\" — it's broken!".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: QueuedCommand = serde_json::from_str(&json).unwrap();
        match parsed {
            QueuedCommand::Assign { task, .. } => {
                assert_eq!(task, "Task #42: Fix \"auth\" — it's broken!");
            }
            _ => panic!("wrong variant"),
        }
    }

    // --- enqueue_command edge cases ---

    #[test]
    fn enqueue_creates_parent_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp
            .path()
            .join("deep")
            .join("nested")
            .join("commands.jsonl");

        enqueue_command(
            &queue,
            &QueuedCommand::Send {
                from: "human".into(),
                to: "arch".into(),
                message: "hello".into(),
            },
        )
        .unwrap();

        assert!(queue.exists());
        let commands = read_command_queue(&queue).unwrap();
        assert_eq!(commands.len(), 1);
    }

    #[test]
    fn enqueue_appends_multiple_commands_preserving_order() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("commands.jsonl");

        for i in 0..5 {
            enqueue_command(
                &queue,
                &QueuedCommand::Send {
                    from: "human".into(),
                    to: "arch".into(),
                    message: format!("msg-{i}"),
                },
            )
            .unwrap();
        }

        let commands = read_command_queue(&queue).unwrap();
        assert_eq!(commands.len(), 5);
        for (i, cmd) in commands.iter().enumerate() {
            match cmd {
                QueuedCommand::Send { message, .. } => {
                    assert_eq!(message, &format!("msg-{i}"));
                }
                _ => panic!("wrong variant at index {i}"),
            }
        }
    }

    // --- read_command_queue edge cases ---

    #[test]
    fn read_command_queue_skips_empty_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("commands.jsonl");
        std::fs::write(
            &queue,
            "\n\n{\"type\":\"send\",\"from\":\"a\",\"to\":\"b\",\"message\":\"hi\"}\n\n\n",
        )
        .unwrap();

        let commands = read_command_queue(&queue).unwrap();
        assert_eq!(commands.len(), 1);
    }

    #[test]
    fn read_command_queue_skips_whitespace_only_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("commands.jsonl");
        std::fs::write(
            &queue,
            "   \n\t\n{\"type\":\"send\",\"from\":\"a\",\"to\":\"b\",\"message\":\"ok\"}\n  \n",
        )
        .unwrap();

        let commands = read_command_queue(&queue).unwrap();
        assert_eq!(commands.len(), 1);
    }

    #[test]
    fn read_command_queue_mixed_valid_and_malformed() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("commands.jsonl");
        std::fs::write(
            &queue,
            concat!(
                "{\"type\":\"send\",\"from\":\"a\",\"to\":\"b\",\"message\":\"first\"}\n",
                "garbage line\n",
                "{\"type\":\"assign\",\"from\":\"m\",\"engineer\":\"e1\",\"task\":\"t1\"}\n",
                "{\"invalid json\n",
                "{\"type\":\"send\",\"from\":\"c\",\"to\":\"d\",\"message\":\"last\"}\n",
            ),
        )
        .unwrap();

        let commands = read_command_queue(&queue).unwrap();
        assert_eq!(
            commands.len(),
            3,
            "should parse 3 valid commands, skip 2 malformed"
        );
    }

    // --- write_command_queue edge cases ---

    #[test]
    fn write_command_queue_empty_slice_creates_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("commands.jsonl");

        // First enqueue something
        enqueue_command(
            &queue,
            &QueuedCommand::Send {
                from: "a".into(),
                to: "b".into(),
                message: "hello".into(),
            },
        )
        .unwrap();

        // Overwrite with empty
        write_command_queue(&queue, &[]).unwrap();

        let commands = read_command_queue(&queue).unwrap();
        assert!(commands.is_empty());
    }

    #[test]
    fn write_command_queue_creates_parent_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("sub").join("dir").join("q.jsonl");

        write_command_queue(
            &queue,
            &[QueuedCommand::Assign {
                from: "m".into(),
                engineer: "e1".into(),
                task: "t1".into(),
            }],
        )
        .unwrap();

        let commands = read_command_queue(&queue).unwrap();
        assert_eq!(commands.len(), 1);
    }

    // --- command_queue_path ---

    #[test]
    fn command_queue_path_returns_expected_path() {
        let root = Path::new("/project");
        let path = command_queue_path(root);
        assert_eq!(
            path,
            PathBuf::from("/project/.batty/team_config/commands.jsonl")
        );
    }

    #[test]
    fn command_queue_path_with_trailing_slash() {
        let root = Path::new("/project/");
        let path = command_queue_path(root);
        assert_eq!(
            path,
            PathBuf::from("/project/.batty/team_config/commands.jsonl")
        );
    }

    // --- drain_command_queue ---

    #[test]
    fn drain_empties_queue_file_but_keeps_file() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("commands.jsonl");

        enqueue_command(
            &queue,
            &QueuedCommand::Send {
                from: "a".into(),
                to: "b".into(),
                message: "msg".into(),
            },
        )
        .unwrap();

        let drained = drain_command_queue(&queue).unwrap();
        assert_eq!(drained.len(), 1);

        // File should still exist but be effectively empty
        assert!(queue.exists());
        let commands = read_command_queue(&queue).unwrap();
        assert!(commands.is_empty());
    }

    #[test]
    fn drain_twice_second_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("commands.jsonl");

        enqueue_command(
            &queue,
            &QueuedCommand::Assign {
                from: "m".into(),
                engineer: "e".into(),
                task: "t".into(),
            },
        )
        .unwrap();

        let first = drain_command_queue(&queue).unwrap();
        assert_eq!(first.len(), 1);

        let second = drain_command_queue(&queue).unwrap();
        assert!(second.is_empty());
    }

    // --- QueuedCommand Debug derive ---

    #[test]
    fn queued_command_debug_format() {
        let cmd = QueuedCommand::Send {
            from: "human".into(),
            to: "arch".into(),
            message: "test".into(),
        };
        let debug = format!("{cmd:?}");
        assert!(debug.contains("Send"));
        assert!(debug.contains("human"));
    }

    #[test]
    fn queued_command_clone() {
        let cmd = QueuedCommand::Assign {
            from: "manager".into(),
            engineer: "eng-1".into(),
            task: "build feature".into(),
        };
        let cloned = cmd.clone();
        let json_original = serde_json::to_string(&cmd).unwrap();
        let json_cloned = serde_json::to_string(&cloned).unwrap();
        assert_eq!(json_original, json_cloned);
    }

    // --- JSON tag format verification ---

    #[test]
    fn send_command_json_has_type_send_tag() {
        let cmd = QueuedCommand::Send {
            from: "a".into(),
            to: "b".into(),
            message: "c".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"type\":\"send\""), "got: {json}");
    }

    #[test]
    fn assign_command_json_has_type_assign_tag() {
        let cmd = QueuedCommand::Assign {
            from: "a".into(),
            engineer: "b".into(),
            task: "c".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"type\":\"assign\""), "got: {json}");
    }
}
