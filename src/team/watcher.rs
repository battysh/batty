//! Disk-based session monitoring — polls agent output via tmux capture-pane.
//!
//! Detects agent completion, crashes, and staleness by periodically capturing
//! pane output and checking for state changes. For Codex, this also tails the
//! authoritative `~/.codex/sessions` JSONL log associated with the member's
//! unique working directory.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::Value;

use crate::tmux;

/// State of a watched agent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatcherState {
    /// Agent is actively producing output.
    Active,
    /// Agent completed its task (returned to shell or exited).
    Completed,
    /// No agent running in pane (idle / waiting for assignment).
    Idle,
    /// No new output for longer than the stale threshold.
    Stale,
}

pub struct SessionWatcher {
    pub pane_id: String,
    #[allow(dead_code)] // Useful for diagnostics; currently the map key is used instead.
    pub member_name: String,
    pub state: WatcherState,
    last_output_hash: u64,
    last_change: Instant,
    last_capture: String,
    stale_threshold: Duration,
    codex: Option<CodexSessionTracker>,
}

struct CodexSessionTracker {
    sessions_root: PathBuf,
    cwd: PathBuf,
    session_file: Option<PathBuf>,
    offset: u64,
}

impl SessionWatcher {
    pub fn new(
        pane_id: &str,
        member_name: &str,
        stale_secs: u64,
        codex_cwd: Option<PathBuf>,
    ) -> Self {
        Self {
            pane_id: pane_id.to_string(),
            member_name: member_name.to_string(),
            state: WatcherState::Idle,
            last_output_hash: 0,
            last_change: Instant::now(),
            last_capture: String::new(),
            stale_threshold: Duration::from_secs(stale_secs),
            codex: codex_cwd.map(|cwd| CodexSessionTracker {
                sessions_root: default_codex_sessions_root(),
                cwd,
                session_file: None,
                offset: 0,
            }),
        }
    }

    /// Poll the pane and update state.
    pub fn poll(&mut self) -> Result<WatcherState> {
        // Check if pane still exists
        if !tmux::pane_exists(&self.pane_id) {
            self.state = WatcherState::Completed;
            return Ok(self.state);
        }

        // Check if pane process died
        if tmux::pane_dead(&self.pane_id).unwrap_or(false) {
            self.state = WatcherState::Completed;
            return Ok(self.state);
        }

        // If idle, stay idle until explicitly activated
        if self.state == WatcherState::Idle {
            return Ok(self.state);
        }

        // Capture current pane content
        let capture = tmux::capture_pane(&self.pane_id).unwrap_or_default();
        let hash = simple_hash(&capture);
        let prompt_visible = is_at_agent_prompt(&capture);
        let codex_managed = self.codex.is_some();
        let codex_completed = self.poll_codex_session().unwrap_or(false);
        let stale = self.last_change.elapsed() > self.stale_threshold;

        if hash != self.last_output_hash {
            self.last_output_hash = hash;
            self.last_change = Instant::now();
            self.last_capture = capture;
            self.state =
                next_state_after_capture(codex_managed, prompt_visible, codex_completed, false);
        } else {
            self.last_capture = capture;
            self.state = if stale {
                WatcherState::Stale
            } else {
                next_state_after_capture(codex_managed, prompt_visible, codex_completed, true)
            };
        }

        Ok(self.state)
    }

    /// Mark this watcher as actively working.
    pub fn activate(&mut self) {
        self.state = WatcherState::Active;
        self.last_change = Instant::now();
        self.last_output_hash = 0;
        if let Some(codex) = self.codex.as_mut() {
            codex.session_file = None;
            codex.offset = 0;
        }
    }

    /// Mark this watcher as idle.
    pub fn deactivate(&mut self) {
        self.state = WatcherState::Idle;
    }

    /// Get the last captured pane output.
    #[allow(dead_code)] // Standup/reporting helpers can read the full capture directly when needed.
    pub fn last_output(&self) -> &str {
        &self.last_capture
    }

    /// Get the last N lines of captured output.
    pub fn last_lines(&self, n: usize) -> String {
        let lines: Vec<&str> = self.last_capture.lines().collect();
        let start = lines.len().saturating_sub(n);
        lines[start..].join("\n")
    }

    fn poll_codex_session(&mut self) -> Result<bool> {
        let Some(codex) = self.codex.as_mut() else {
            return Ok(false);
        };

        if codex.session_file.is_none() {
            codex.session_file = discover_codex_session_file(&codex.sessions_root, &codex.cwd)?;
            if let Some(session_file) = codex.session_file.as_ref() {
                codex.offset = current_file_len(session_file)?;
            }
            return Ok(false);
        }

        let Some(session_file) = codex.session_file.clone() else {
            return Ok(false);
        };

        if !session_file.exists() {
            codex.session_file = None;
            codex.offset = 0;
            return Ok(false);
        }

        poll_codex_session_file(&session_file, &mut codex.offset)
    }
}

/// Check if the captured pane output shows an idle prompt.
///
/// This covers Claude's `❯` prompt, a shell prompt, and Codex's `›` composer.
///
/// Claude Code always renders `❯` at the bottom of the screen — even while
/// actively working.  The reliable differentiator is the status bar at the
/// very bottom: when Claude is processing, it appends `· esc to interrupt`.
/// If we detect that indicator we return `false` immediately.
fn is_at_agent_prompt(capture: &str) -> bool {
    let trimmed: Vec<&str> = capture
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(5)
        .collect();

    // Claude Code shows "esc to interrupt" in the bottom status bar while working
    for line in &trimmed {
        if line.contains("esc to interrupt") {
            return false;
        }
    }

    // Claude can also land in an interrupt-resolution UI after a blocked tool call
    // or nested interactive flow. That still represents an active task, not an idle
    // prompt waiting for a fresh assignment.
    if capture.contains("What should Claude do instead?")
        || capture.contains("Conversation interrupted")
    {
        return false;
    }

    for line in &trimmed {
        let l = line.trim();
        // Claude Code idle prompt
        if l == "❯" || l.starts_with("❯ ") {
            return true;
        }
        // Codex idle composer prompt
        if l == "›" || l.starts_with("› ") {
            return true;
        }
        // Fell back to shell
        if l.ends_with("$ ") || l == "$" {
            return true;
        }
    }
    false
}

fn next_state_after_capture(
    codex_managed: bool,
    prompt_visible: bool,
    codex_completed: bool,
    unchanged_capture: bool,
) -> WatcherState {
    if codex_completed {
        return WatcherState::Completed;
    }

    if codex_managed {
        return if prompt_visible && unchanged_capture {
            WatcherState::Idle
        } else {
            WatcherState::Active
        };
    }

    if prompt_visible {
        WatcherState::Idle
    } else {
        WatcherState::Active
    }
}

fn simple_hash(s: &str) -> u64 {
    // FNV-1a style hash, good enough for change detection
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in s.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn default_codex_sessions_root() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
        .join(".codex")
        .join("sessions")
}

fn current_file_len(path: &Path) -> Result<u64> {
    Ok(fs::metadata(path)?.len())
}

fn discover_codex_session_file(sessions_root: &Path, cwd: &Path) -> Result<Option<PathBuf>> {
    if !sessions_root.exists() {
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

fn read_dir_paths(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        paths.push(entry.path());
    }
    Ok(paths)
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

fn poll_codex_session_file(path: &Path, offset: &mut u64) -> Result<bool> {
    let file_len = fs::metadata(path)?.len();
    if file_len < *offset {
        *offset = 0;
    }

    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(*offset))?;

    let mut completed = false;
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

    Ok(completed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn simple_hash_differs_for_different_input() {
        assert_ne!(simple_hash("hello"), simple_hash("world"));
        assert_eq!(simple_hash("same"), simple_hash("same"));
    }

    #[test]
    fn new_watcher_starts_idle() {
        let w = SessionWatcher::new("%0", "eng-1-1", 300, None);
        assert_eq!(w.state, WatcherState::Idle);
    }

    #[test]
    fn activate_sets_active() {
        let mut w = SessionWatcher::new("%0", "eng-1-1", 300, None);
        w.activate();
        assert_eq!(w.state, WatcherState::Active);
    }

    #[test]
    fn deactivate_sets_idle() {
        let mut w = SessionWatcher::new("%0", "eng-1-1", 300, None);
        w.activate();
        w.deactivate();
        assert_eq!(w.state, WatcherState::Idle);
    }

    #[test]
    fn last_lines_returns_tail() {
        let mut w = SessionWatcher::new("%0", "eng-1-1", 300, None);
        w.last_capture = "line1\nline2\nline3\nline4\nline5".to_string();
        assert_eq!(w.last_lines(3), "line3\nline4\nline5");
        assert_eq!(w.last_lines(10), "line1\nline2\nline3\nline4\nline5");
    }

    #[test]
    fn detects_claude_code_prompt() {
        let capture = "⏺ Done.\n\n❯ \n\n  bypass permissions\n";
        assert!(is_at_agent_prompt(capture));
    }

    #[test]
    fn detects_shell_prompt() {
        let capture = "some output\n$ \n";
        assert!(is_at_agent_prompt(capture));
    }

    #[test]
    fn detects_codex_prompt() {
        let capture =
            "› Improve documentation in @filename\n\n  gpt-5.4 high · 84% left · ~/repo\n";
        assert!(is_at_agent_prompt(capture));
    }

    #[test]
    fn no_prompt_when_working() {
        let capture = "⏺ Bash(python -m pytest)\n  ⎿  running tests...\n";
        assert!(!is_at_agent_prompt(capture));
    }

    #[test]
    fn claude_working_not_idle_despite_prompt_visible() {
        // Claude Code always renders ❯ at the bottom — but shows
        // "esc to interrupt" in the status bar while processing.
        let capture = concat!(
            "✻ Slithering… (4m 12s)\n",
            "  ⎿  Tip: Use /btw to ask a quick side question\n",
            "────────────────────────────\n",
            "❯ \n",
            "────────────────────────────\n",
            "  ⏵⏵ bypass permissions on (shift+tab to cycle) · esc to interrupt\n",
        );
        assert!(!is_at_agent_prompt(capture));
    }

    #[test]
    fn claude_idle_detected_without_esc_to_interrupt() {
        let capture = concat!(
            "⏺ Done.\n",
            "────────────────────────────\n",
            "❯ \n",
            "────────────────────────────\n",
            "  ⏵⏵ bypass permissions on (shift+tab to cycle)\n",
        );
        assert!(is_at_agent_prompt(capture));
    }

    #[test]
    fn claude_interrupted_prompt_not_idle() {
        let capture = concat!(
            "■ Conversation interrupted - tell the model what to do differently.\n",
            "  Something went wrong? Hit `/feedback` to report the issue.\n",
            "\n",
            "Interrupted · What should Claude do instead?\n",
            "❯ \n",
            "  ⏵⏵ bypass permissions on (shift+tab to cycle)\n",
        );
        assert!(!is_at_agent_prompt(capture));
    }

    #[test]
    fn codex_prompt_keeps_active_state_until_completion_event() {
        assert_eq!(
            next_state_after_capture(true, true, false, false),
            WatcherState::Active
        );
        assert_eq!(
            next_state_after_capture(true, true, false, true),
            WatcherState::Idle
        );
        assert_eq!(
            next_state_after_capture(true, false, false, true),
            WatcherState::Active
        );
        assert_eq!(
            next_state_after_capture(true, true, true, true),
            WatcherState::Completed
        );
    }

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

        let discovered = discover_codex_session_file(&sessions_root, &wanted_cwd).unwrap();
        assert_eq!(discovered.as_deref(), Some(wanted_file.as_path()));
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
        assert!(!poll_codex_session_file(&session_file, &mut offset).unwrap());

        writeln!(
            handle,
            "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\"}}}}"
        )
        .unwrap();
        handle.flush().unwrap();

        assert!(poll_codex_session_file(&session_file, &mut offset).unwrap());
        assert!(!poll_codex_session_file(&session_file, &mut offset).unwrap());
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
            session_file: None,
            offset: 0,
        };

        if tracker.session_file.is_none() {
            tracker.session_file =
                discover_codex_session_file(&tracker.sessions_root, &tracker.cwd).unwrap();
            if let Some(found) = tracker.session_file.as_ref() {
                tracker.offset = current_file_len(found).unwrap();
            }
        }

        assert!(
            !poll_codex_session_file(tracker.session_file.as_ref().unwrap(), &mut tracker.offset)
                .unwrap()
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

        assert!(
            poll_codex_session_file(tracker.session_file.as_ref().unwrap(), &mut tracker.offset)
                .unwrap()
        );
    }
}
