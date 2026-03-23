//! Claude session tracking — discovers and polls Claude Code JSONL session files.
//!
//! Tracks session state by tailing the on-disk JSONL log, detecting tool use,
//! end-of-turn, interruptions, and session rebinding when a newer file appears.

use anyhow::Result;
use serde_json::Value;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use super::{TrackerState, current_file_len, read_dir_paths, session_file_id};

pub(super) struct ClaudeSessionTracker {
    pub(super) projects_root: PathBuf,
    pub(super) cwd: PathBuf,
    pub(super) session_id: Option<String>,
    pub(super) session_file: Option<PathBuf>,
    pub(super) offset: u64,
    pub(super) last_state: TrackerState,
}

pub(super) fn discover_claude_session_file(
    projects_root: &Path,
    cwd: &Path,
    session_id: Option<&str>,
) -> Result<Option<PathBuf>> {
    if !projects_root.exists() {
        return Ok(None);
    }

    let preferred_dir = projects_root.join(cwd.to_string_lossy().replace('/', "-"));
    if let Some(session_id) = session_id {
        let exact = preferred_dir.join(format!("{session_id}.jsonl"));
        if exact.is_file() {
            return Ok(Some(exact));
        }
        return Ok(None);
    }

    if preferred_dir.is_dir() {
        let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
        for entry in read_dir_paths(&preferred_dir)? {
            if entry.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let modified = fs::metadata(&entry)
                .and_then(|meta| meta.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            match &newest {
                Some((current, _)) if modified <= *current => {}
                _ => newest = Some((modified, entry)),
            }
        }
        if newest.is_some() {
            return Ok(newest.map(|(_, path)| path));
        }
    }

    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for project_dir in read_dir_paths(projects_root)? {
        if !project_dir.is_dir() {
            continue;
        }
        for entry in read_dir_paths(&project_dir)? {
            if entry.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            if session_file_cwd(&entry)?.as_deref() != Some(cwd.as_os_str()) {
                continue;
            }
            let modified = fs::metadata(&entry)
                .and_then(|meta| meta.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            match &newest {
                Some((current, _)) if modified <= *current => {}
                _ => newest = Some((modified, entry)),
            }
        }
    }

    Ok(newest.map(|(_, path)| path))
}

fn session_file_cwd(path: &Path) -> Result<Option<std::ffi::OsString>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(cwd) = entry.get("cwd").and_then(Value::as_str) {
            return Ok(Some(std::ffi::OsString::from(cwd)));
        }
    }
    Ok(None)
}

pub(super) fn poll_claude_session(tracker: &mut ClaudeSessionTracker) -> Result<TrackerState> {
    if tracker.session_file.is_none() {
        tracker.session_file = discover_claude_session_file(
            &tracker.projects_root,
            &tracker.cwd,
            tracker.session_id.as_deref(),
        )?;
        if let Some(session_file) = tracker.session_file.clone() {
            // Bind at EOF so historical entries from an older or unrelated
            // Claude session do not override the live pane state on first poll.
            tracker.offset = current_file_len(&session_file)?;
            tracker.last_state = TrackerState::Unknown;
        }
        return Ok(tracker.last_state);
    }

    maybe_rebind_claude_session_file(tracker)?;

    let Some(session_file) = tracker.session_file.clone() else {
        return Ok(TrackerState::Unknown);
    };

    if !session_file.exists() {
        tracker.session_file = None;
        tracker.offset = 0;
        tracker.last_state = TrackerState::Unknown;
        return Ok(TrackerState::Unknown);
    }

    let (state, offset) = parse_claude_session_file(&session_file, tracker.offset)?;
    tracker.offset = offset;
    if state != TrackerState::Unknown {
        tracker.last_state = state;
    }
    Ok(tracker.last_state)
}

fn maybe_rebind_claude_session_file(tracker: &mut ClaudeSessionTracker) -> Result<()> {
    let Some(current_file) = tracker.session_file.clone() else {
        return Ok(());
    };

    let Some(newest_file) =
        discover_claude_session_file(&tracker.projects_root, &tracker.cwd, None)?
    else {
        return Ok(());
    };

    if newest_file == current_file {
        return Ok(());
    }

    let current_modified = fs::metadata(&current_file)
        .and_then(|meta| meta.modified())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    let newest_modified = fs::metadata(&newest_file)
        .and_then(|meta| meta.modified())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);

    if newest_modified <= current_modified {
        return Ok(());
    }

    tracker.session_file = Some(newest_file.clone());
    tracker.session_id = session_file_id(Some(&newest_file));
    tracker.offset = current_file_len(&newest_file)?;
    tracker.last_state = TrackerState::Unknown;
    Ok(())
}

fn parse_claude_session_file(path: &Path, start_offset: u64) -> Result<(TrackerState, u64)> {
    let file_len = fs::metadata(path)?.len();
    let mut offset = if file_len < start_offset {
        0
    } else {
        start_offset
    };

    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(offset))?;

    let mut state = TrackerState::Unknown;
    loop {
        let line_start = reader.stream_position()?;
        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            break;
        }
        if !line.ends_with('\n') {
            reader.seek(SeekFrom::Start(line_start))?;
            break;
        }

        if let Ok(entry) = serde_json::from_str::<Value>(&line) {
            let line_state = classify_claude_log_entry(&entry);
            if line_state != TrackerState::Unknown {
                state = line_state;
            }
        }

        offset = reader.stream_position()?;
    }

    Ok((state, offset))
}

fn classify_claude_log_entry(entry: &Value) -> TrackerState {
    match entry.get("type").and_then(Value::as_str) {
        Some("assistant") => {
            let stop_reason = entry
                .get("message")
                .and_then(|message| message.get("stop_reason"))
                .and_then(Value::as_str);
            match stop_reason {
                Some("tool_use") => TrackerState::Active,
                Some("end_turn") => TrackerState::Idle,
                _ => TrackerState::Unknown,
            }
        }
        Some("progress") => TrackerState::Active,
        Some("user") => {
            if entry
                .get("toolUseResult")
                .and_then(Value::as_object)
                .is_some()
            {
                return TrackerState::Active;
            }
            if let Some(content) = entry
                .get("message")
                .and_then(|message| message.get("content"))
            {
                if let Some(text) = content.as_str() {
                    if text == "[Request interrupted by user]" {
                        return TrackerState::Idle;
                    }
                    return TrackerState::Active;
                }
                if let Some(items) = content.as_array() {
                    for item in items {
                        if item.get("tool_use_id").is_some() {
                            return TrackerState::Active;
                        }
                        if item.get("type").and_then(Value::as_str) == Some("text")
                            && item.get("text").and_then(Value::as_str)
                                == Some("[Request interrupted by user]")
                        {
                            return TrackerState::Idle;
                        }
                    }
                    return TrackerState::Active;
                }
            }
            TrackerState::Unknown
        }
        _ => TrackerState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_claude_session_by_exact_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let projects_root = tmp.path().join("projects");
        let cwd = PathBuf::from("/Users/zedmor/chess_test");
        let project_dir = projects_root.join("-Users-zedmor-chess-test");
        std::fs::create_dir_all(&project_dir).unwrap();

        let session_file = project_dir.join("latest.jsonl");
        std::fs::write(
            &session_file,
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":{{\"content\":\"hello\"}}}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let discovered = discover_claude_session_file(&projects_root, &cwd, None).unwrap();
        assert_eq!(discovered.as_deref(), Some(session_file.as_path()));
    }

    #[test]
    fn discover_claude_session_file_requires_exact_session_id_when_configured() {
        let tmp = tempfile::tempdir().unwrap();
        let projects_root = tmp.path().join("projects");
        let cwd = PathBuf::from("/Users/zedmor/chess_test");
        let project_dir = projects_root.join("-Users-zedmor-chess-test");
        std::fs::create_dir_all(&project_dir).unwrap();

        let other_session = project_dir.join("11111111-1111-4111-8111-111111111111.jsonl");
        std::fs::write(
            &other_session,
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"sessionId\":\"11111111-1111-4111-8111-111111111111\"}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let discovered = discover_claude_session_file(
            &projects_root,
            &cwd,
            Some("22222222-2222-4222-8222-222222222222"),
        )
        .unwrap();
        assert!(discovered.is_none());
    }

    #[test]
    fn classify_claude_log_entry_tracks_tool_and_end_turn() {
        let tool_use: Value =
            serde_json::from_str(r#"{"type":"assistant","message":{"stop_reason":"tool_use"}}"#)
                .unwrap();
        let end_turn: Value =
            serde_json::from_str(r#"{"type":"assistant","message":{"stop_reason":"end_turn"}}"#)
                .unwrap();
        let tool_result: Value =
            serde_json::from_str(r#"{"type":"user","toolUseResult":{"stdout":"ok"}}"#).unwrap();

        assert_eq!(classify_claude_log_entry(&tool_use), TrackerState::Active);
        assert_eq!(
            classify_claude_log_entry(&tool_result),
            TrackerState::Active
        );
        assert_eq!(classify_claude_log_entry(&end_turn), TrackerState::Idle);
    }

    #[test]
    fn parse_claude_session_file_reports_latest_state() {
        let tmp = tempfile::tempdir().unwrap();
        let session_file = tmp.path().join("session.jsonl");
        std::fs::write(
            &session_file,
            concat!(
                "{\"type\":\"user\",\"message\":{\"content\":\"hello\"}}\n",
                "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"tool_use\"}}\n",
                "{\"type\":\"user\",\"toolUseResult\":{\"stdout\":\"ok\"}}\n",
                "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"end_turn\"}}\n",
            ),
        )
        .unwrap();

        let (state, offset) = parse_claude_session_file(&session_file, 0).unwrap();
        assert_eq!(state, TrackerState::Idle);
        assert!(offset > 0);
    }

    #[test]
    fn claude_tracker_binding_ignores_historical_state() {
        let tmp = tempfile::tempdir().unwrap();
        let projects_root = tmp.path().join("projects");
        let cwd = tmp
            .path()
            .join("repo")
            .join(".batty")
            .join("worktrees")
            .join("eng-1");
        let project_dir = projects_root.join(cwd.to_string_lossy().replace('/', "-"));
        std::fs::create_dir_all(&project_dir).unwrap();

        let session_file = project_dir.join("latest.jsonl");
        std::fs::write(
            &session_file,
            concat!(
                "{\"type\":\"user\",\"message\":{\"content\":\"hello\"}}\n",
                "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"tool_use\"}}\n",
            ),
        )
        .unwrap();

        let mut tracker = ClaudeSessionTracker {
            projects_root,
            cwd,
            session_id: None,
            session_file: None,
            offset: 0,
            last_state: TrackerState::Unknown,
        };

        assert_eq!(
            poll_claude_session(&mut tracker).unwrap(),
            TrackerState::Unknown
        );
        assert_eq!(tracker.offset, current_file_len(&session_file).unwrap());
        assert_eq!(tracker.last_state, TrackerState::Unknown);
    }

    #[test]
    fn claude_tracker_rebinds_to_newer_session_file_after_manual_resume() {
        let tmp = tempfile::tempdir().unwrap();
        let projects_root = tmp.path().join("projects");
        let cwd = tmp
            .path()
            .join("repo")
            .join(".batty")
            .join("worktrees")
            .join("eng-1");
        let project_dir = projects_root.join(cwd.to_string_lossy().replace('/', "-"));
        std::fs::create_dir_all(&project_dir).unwrap();

        let old_session = project_dir.join("11111111-1111-4111-8111-111111111111.jsonl");
        std::fs::write(
            &old_session,
            "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"end_turn\"}}\n",
        )
        .unwrap();

        let mut tracker = ClaudeSessionTracker {
            projects_root: projects_root.clone(),
            cwd: cwd.clone(),
            session_id: Some("11111111-1111-4111-8111-111111111111".to_string()),
            session_file: Some(old_session.clone()),
            offset: current_file_len(&old_session).unwrap(),
            last_state: TrackerState::Idle,
        };

        std::thread::sleep(std::time::Duration::from_millis(20));

        let new_session = project_dir.join("22222222-2222-4222-8222-222222222222.jsonl");
        std::fs::write(
            &new_session,
            "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"end_turn\"}}\n",
        )
        .unwrap();

        assert_eq!(
            poll_claude_session(&mut tracker).unwrap(),
            TrackerState::Unknown
        );
        assert_eq!(tracker.session_file.as_deref(), Some(new_session.as_path()));
        assert_eq!(
            tracker.session_id.as_deref(),
            Some("22222222-2222-4222-8222-222222222222")
        );
        assert_eq!(tracker.offset, current_file_len(&new_session).unwrap());
    }
}
