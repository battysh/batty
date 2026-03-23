//! Codex session tracking — discovers and polls Codex JSONL session files.
//!
//! Detects task completion events and tracks quality signals (shrinking
//! responses, repeated outputs, tool failures) to flag degraded agents.

use anyhow::Result;
use serde_json::Value;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use super::{CodexQualitySignals, TrackerState, read_dir_paths, simple_hash};

pub(super) struct CodexSessionTracker {
    pub(super) sessions_root: PathBuf,
    pub(super) cwd: PathBuf,
    pub(super) session_id: Option<String>,
    pub(super) session_file: Option<PathBuf>,
    pub(super) offset: u64,
    pub(super) quality: CodexQualitySignals,
    pub(super) last_response_hash: Option<u64>,
}

pub(super) fn discover_codex_session_file(
    sessions_root: &Path,
    cwd: &Path,
    session_id: Option<&str>,
) -> Result<Option<PathBuf>> {
    if !sessions_root.exists() {
        return Ok(None);
    }

    if let Some(session_id) = session_id {
        for year in read_dir_paths(sessions_root)? {
            for month in read_dir_paths(&year)? {
                for day in read_dir_paths(&month)? {
                    let entry = day.join(format!("{session_id}.jsonl"));
                    if entry.is_file()
                        && session_meta_cwd(&entry)?.as_deref() == Some(cwd.as_os_str())
                    {
                        return Ok(Some(entry));
                    }
                }
            }
        }
        return Ok(None);
    }

    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for year in read_dir_paths(sessions_root)? {
        for month in read_dir_paths(&year)? {
            for day in read_dir_paths(&month)? {
                for entry in read_dir_paths(&day)? {
                    if entry.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                        continue;
                    }
                    if session_meta_cwd(&entry)?.as_deref() != Some(cwd.as_os_str()) {
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
        }
    }

    Ok(newest.map(|(_, path)| path))
}

fn session_meta_cwd(path: &Path) -> Result<Option<std::ffi::OsString>> {
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
        if entry.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        return Ok(entry
            .get("payload")
            .and_then(|payload| payload.get("cwd"))
            .and_then(Value::as_str)
            .map(std::ffi::OsString::from));
    }
    Ok(None)
}

pub(super) fn poll_codex_session_file(
    path: &Path,
    offset: &mut u64,
    quality: &mut CodexQualitySignals,
    last_response_hash: &mut Option<u64>,
) -> Result<TrackerState> {
    let file_len = fs::metadata(path)?.len();
    if file_len < *offset {
        *offset = 0;
    }

    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(*offset))?;

    let mut completed = false;
    let mut had_new_events = false;
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

        had_new_events = true;

        if let Ok(entry) = serde_json::from_str::<Value>(&line) {
            update_codex_quality_signals(&entry, quality, last_response_hash);
            if entry.get("type").and_then(Value::as_str) == Some("event_msg")
                && entry
                    .get("payload")
                    .and_then(|payload| payload.get("type"))
                    .and_then(Value::as_str)
                    == Some("task_complete")
            {
                completed = true;
            }
        }

        *offset = reader.stream_position()?;
    }

    if completed {
        Ok(TrackerState::Completed)
    } else if had_new_events {
        Ok(TrackerState::Active)
    } else {
        Ok(TrackerState::Unknown)
    }
}

pub(super) fn update_codex_quality_signals(
    entry: &Value,
    quality: &mut CodexQualitySignals,
    last_response_hash: &mut Option<u64>,
) {
    if let Some(text) = codex_assistant_output_text(entry) {
        let normalized = normalize_codex_response_text(&text);
        let response_chars = normalized.chars().count();
        let previous_len = quality.last_response_chars;
        if let Some(previous_len) = previous_len {
            if response_chars < previous_len {
                quality.shortening_streak += 1;
            } else {
                quality.shortening_streak = 0;
            }
        } else {
            quality.shortening_streak = 0;
        }

        let response_hash = simple_hash(&normalized);
        if Some(response_hash) == *last_response_hash && !normalized.is_empty() {
            quality.repeated_output_streak += 1;
        } else {
            quality.repeated_output_streak = 1;
        }

        quality.shrinking_responses = quality.shortening_streak >= 2;
        quality.repeated_identical_outputs = quality.repeated_output_streak >= 3;
        *last_response_hash = Some(response_hash);
        quality.last_response_chars = Some(response_chars);
    }

    if let Some(tool_failure_message) = codex_tool_failure_message(entry) {
        quality.tool_failure_message = Some(tool_failure_message);
    }
}

fn codex_assistant_output_text(entry: &Value) -> Option<String> {
    let payload = entry.get("payload")?;
    if entry.get("type").and_then(Value::as_str) != Some("response_item") {
        return None;
    }
    if payload.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    if payload.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }

    let mut text = String::new();
    for item in payload.get("content")?.as_array()? {
        if item.get("type").and_then(Value::as_str) == Some("output_text") {
            if let Some(chunk) = item.get("text").and_then(Value::as_str) {
                text.push_str(chunk);
            }
        }
    }

    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

fn codex_tool_failure_message(entry: &Value) -> Option<String> {
    if entry.get("type").and_then(Value::as_str) != Some("response_item") {
        return None;
    }
    let payload = entry.get("payload")?;
    if payload.get("type").and_then(Value::as_str) != Some("function_call_output") {
        return None;
    }

    let output = payload.get("output").and_then(Value::as_str)?.trim();
    if !looks_like_codex_tool_failure(output) {
        return None;
    }

    Some(first_non_empty_line(output).unwrap_or(output).to_string())
}

fn normalize_codex_response_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn looks_like_codex_tool_failure(output: &str) -> bool {
    let lowered = output.to_ascii_lowercase();
    lowered.contains("sandboxdenied")
        || lowered.contains("timed out")
        || lowered.contains("failed to run")
        || lowered.contains("exec_command failed")
        || (lowered.contains("failed") && lowered.contains("exit code"))
        || (lowered.contains("error") && lowered.contains("process exited"))
}

fn first_non_empty_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

#[cfg(test)]
mod tests {
    use super::super::current_file_len;
    use super::*;
    use std::io::Write;

    #[test]
    fn discovers_codex_session_by_exact_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_root = tmp.path().join("sessions");
        let session_dir = sessions_root.join("2026").join("03").join("10");
        std::fs::create_dir_all(&session_dir).unwrap();

        let wanted_cwd = tmp
            .path()
            .join("repo")
            .join(".batty")
            .join("codex-context")
            .join("architect");
        let other_cwd = tmp.path().join("repo");
        let wanted_file = session_dir.join("wanted.jsonl");
        let other_file = session_dir.join("other.jsonl");

        std::fs::write(
            &wanted_file,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"cwd\":\"{}\"}}}}\n",
                wanted_cwd.display()
            ),
        )
        .unwrap();
        std::fs::write(
            &other_file,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"cwd\":\"{}\"}}}}\n",
                other_cwd.display()
            ),
        )
        .unwrap();

        let discovered = discover_codex_session_file(&sessions_root, &wanted_cwd, None).unwrap();
        assert_eq!(discovered.as_deref(), Some(wanted_file.as_path()));
    }

    #[test]
    fn discover_codex_session_file_requires_exact_session_id_when_configured() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_root = tmp.path().join("sessions");
        let session_dir = sessions_root.join("2026").join("03").join("21");
        std::fs::create_dir_all(&session_dir).unwrap();

        let cwd = tmp
            .path()
            .join("repo")
            .join(".batty")
            .join("codex-context")
            .join("eng-1");
        let other_file = session_dir.join("other-session.jsonl");
        std::fs::write(
            &other_file,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"cwd\":\"{}\"}}}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let discovered =
            discover_codex_session_file(&sessions_root, &cwd, Some("missing-session")).unwrap();
        assert!(discovered.is_none());
    }

    #[test]
    fn codex_session_poll_detects_task_complete() {
        let tmp = tempfile::tempdir().unwrap();
        let session_file = tmp.path().join("session.jsonl");
        let mut handle = File::create(&session_file).unwrap();
        writeln!(
            handle,
            "{{\"type\":\"session_meta\",\"payload\":{{\"cwd\":\"/tmp/example\"}}}}"
        )
        .unwrap();
        handle.flush().unwrap();

        let mut offset = 0;
        let mut quality = CodexQualitySignals::default();
        let mut last_response_hash = None;
        // session_meta is a new event but not task_complete → Active
        assert_eq!(
            poll_codex_session_file(
                &session_file,
                &mut offset,
                &mut quality,
                &mut last_response_hash,
            )
            .unwrap(),
            TrackerState::Active
        );

        writeln!(
            handle,
            "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\"}}}}"
        )
        .unwrap();
        handle.flush().unwrap();

        assert_eq!(
            poll_codex_session_file(
                &session_file,
                &mut offset,
                &mut quality,
                &mut last_response_hash,
            )
            .unwrap(),
            TrackerState::Completed
        );
        // No new events → Unknown
        assert_eq!(
            poll_codex_session_file(
                &session_file,
                &mut offset,
                &mut quality,
                &mut last_response_hash,
            )
            .unwrap(),
            TrackerState::Unknown
        );
    }

    #[test]
    fn codex_existing_session_ignores_historical_task_complete_when_binding() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_root = tmp.path().join("sessions");
        let session_dir = sessions_root.join("2026").join("03").join("10");
        std::fs::create_dir_all(&session_dir).unwrap();

        let cwd = tmp
            .path()
            .join("repo")
            .join(".batty")
            .join("codex-context")
            .join("eng-1");
        let session_file = session_dir.join("session.jsonl");
        std::fs::write(
            &session_file,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"cwd\":\"{}\"}}}}\n\
                 {{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\"}}}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let mut tracker = CodexSessionTracker {
            sessions_root,
            cwd,
            session_id: None,
            session_file: None,
            offset: 0,
            quality: CodexQualitySignals::default(),
            last_response_hash: None,
        };

        if tracker.session_file.is_none() {
            tracker.session_file =
                discover_codex_session_file(&tracker.sessions_root, &tracker.cwd, None).unwrap();
            if let Some(found) = tracker.session_file.as_ref() {
                tracker.offset = current_file_len(found).unwrap();
            }
        }

        // After binding at EOF, no new events → Unknown
        assert_eq!(
            poll_codex_session_file(
                tracker.session_file.as_ref().unwrap(),
                &mut tracker.offset,
                &mut tracker.quality,
                &mut tracker.last_response_hash,
            )
            .unwrap(),
            TrackerState::Unknown
        );

        let mut handle = std::fs::OpenOptions::new()
            .append(true)
            .open(tracker.session_file.as_ref().unwrap())
            .unwrap();
        writeln!(
            handle,
            "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\"}}}}"
        )
        .unwrap();
        handle.flush().unwrap();

        assert_eq!(
            poll_codex_session_file(
                tracker.session_file.as_ref().unwrap(),
                &mut tracker.offset,
                &mut tracker.quality,
                &mut tracker.last_response_hash,
            )
            .unwrap(),
            TrackerState::Completed
        );
    }

    #[test]
    fn codex_session_quality_detects_shrinking_responses() {
        let tmp = tempfile::tempdir().unwrap();
        let session_file = tmp.path().join("session.jsonl");
        std::fs::write(
            &session_file,
            concat!(
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"This is a fairly detailed reply with enough content.\"}]}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"Shorter follow-up reply.\"}]}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"Tiny.\"}]}}\n",
            ),
        )
        .unwrap();

        let mut offset = 0;
        let mut quality = CodexQualitySignals::default();
        let mut last_response_hash = None;
        assert_eq!(
            poll_codex_session_file(
                &session_file,
                &mut offset,
                &mut quality,
                &mut last_response_hash,
            )
            .unwrap(),
            TrackerState::Active
        );

        assert_eq!(quality.last_response_chars, Some(5));
        assert_eq!(quality.shortening_streak, 2);
        assert!(quality.shrinking_responses);
        assert!(!quality.repeated_identical_outputs);
    }

    #[test]
    fn codex_session_quality_detects_repeated_identical_outputs() {
        let tmp = tempfile::tempdir().unwrap();
        let session_file = tmp.path().join("session.jsonl");
        std::fs::write(
            &session_file,
            concat!(
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"Same response.\"}]}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"Same response.\"}]}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"Same response.\"}]}}\n",
            ),
        )
        .unwrap();

        let mut offset = 0;
        let mut quality = CodexQualitySignals::default();
        let mut last_response_hash = None;
        poll_codex_session_file(
            &session_file,
            &mut offset,
            &mut quality,
            &mut last_response_hash,
        )
        .unwrap();

        assert_eq!(quality.repeated_output_streak, 3);
        assert!(quality.repeated_identical_outputs);
        assert!(!quality.shrinking_responses);
    }

    #[test]
    fn codex_session_quality_detects_tool_failure_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let session_file = tmp.path().join("session.jsonl");
        std::fs::write(
            &session_file,
            "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call_output\",\"call_id\":\"call_123\",\"output\":\"exec_command failed: SandboxDenied { message: \\\"operation not permitted\\\" }\"}}\n",
        )
        .unwrap();

        let mut offset = 0;
        let mut quality = CodexQualitySignals::default();
        let mut last_response_hash = None;
        poll_codex_session_file(
            &session_file,
            &mut offset,
            &mut quality,
            &mut last_response_hash,
        )
        .unwrap();

        assert_eq!(
            quality.tool_failure_message.as_deref(),
            Some("exec_command failed: SandboxDenied { message: \"operation not permitted\" }")
        );
    }
}
