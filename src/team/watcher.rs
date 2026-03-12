//! Disk-based session monitoring — polls agent output via tmux capture-pane.
//!
//! Detects agent completion, crashes, and staleness by periodically capturing
//! pane output and checking for state changes. For Codex and Claude, this also
//! tails their on-disk session JSONL data to reduce false classifications from
//! stale pane text.

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
    tracker: Option<SessionTracker>,
}

#[derive(Debug, Clone)]
pub enum SessionTrackerConfig {
    Codex { cwd: PathBuf },
    Claude { cwd: PathBuf },
}

enum SessionTracker {
    Codex(CodexSessionTracker),
    Claude(ClaudeSessionTracker),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrackerKind {
    None,
    Codex,
    Claude,
}

struct CodexSessionTracker {
    sessions_root: PathBuf,
    cwd: PathBuf,
    session_file: Option<PathBuf>,
    offset: u64,
}

struct ClaudeSessionTracker {
    projects_root: PathBuf,
    cwd: PathBuf,
    session_id: Option<String>,
    session_file: Option<PathBuf>,
    offset: u64,
    last_state: TrackerState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrackerState {
    Active,
    Idle,
    Completed,
    Unknown,
}

impl SessionWatcher {
    pub fn new(
        pane_id: &str,
        member_name: &str,
        stale_secs: u64,
        tracker: Option<SessionTrackerConfig>,
    ) -> Self {
        Self {
            pane_id: pane_id.to_string(),
            member_name: member_name.to_string(),
            state: WatcherState::Idle,
            last_output_hash: 0,
            last_change: Instant::now(),
            last_capture: String::new(),
            stale_threshold: Duration::from_secs(stale_secs),
            tracker: tracker.map(|tracker| match tracker {
                SessionTrackerConfig::Codex { cwd } => SessionTracker::Codex(CodexSessionTracker {
                    sessions_root: default_codex_sessions_root(),
                    cwd,
                    session_file: None,
                    offset: 0,
                }),
                SessionTrackerConfig::Claude { cwd } => {
                    SessionTracker::Claude(ClaudeSessionTracker {
                        projects_root: default_claude_projects_root(),
                        cwd,
                        session_id: None,
                        session_file: None,
                        offset: 0,
                        last_state: TrackerState::Unknown,
                    })
                }
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

        // If idle, peek at the pane to detect if the agent started working.
        // This lets the watcher self-heal without requiring explicit activation
        // from the daemon whenever a nudge, standup, or external input arrives.
        if self.state == WatcherState::Idle {
            let capture = tmux::capture_pane(&self.pane_id).unwrap_or_default();
            let screen_state = classify_capture_state(&capture);
            let tracker_state = self.poll_tracker().unwrap_or(TrackerState::Unknown);
            let tracker_kind = self.tracker_kind();
            if !capture.is_empty() {
                self.last_capture = capture;
                let next_state = next_state_after_capture(
                    tracker_kind,
                    screen_state,
                    tracker_state,
                    false,
                    false,
                );
                if matches!(next_state, WatcherState::Active | WatcherState::Completed) {
                    self.last_output_hash = simple_hash(&self.last_capture);
                    self.last_change = Instant::now();
                    self.state = next_state;
                }
            }
            return Ok(self.state);
        }

        // Capture current pane content
        let capture = tmux::capture_pane(&self.pane_id).unwrap_or_default();
        let hash = simple_hash(&capture);
        let screen_state = classify_capture_state(&capture);
        let tracker_state = self.poll_tracker().unwrap_or(TrackerState::Unknown);
        let tracker_kind = self.tracker_kind();
        let stale = self.last_change.elapsed() > self.stale_threshold;

        if hash != self.last_output_hash {
            self.last_output_hash = hash;
            self.last_change = Instant::now();
            self.last_capture = capture;
            self.state =
                next_state_after_capture(tracker_kind, screen_state, tracker_state, false, false);
        } else {
            self.last_capture = capture;
            self.state =
                next_state_after_capture(tracker_kind, screen_state, tracker_state, stale, true);
        }

        Ok(self.state)
    }

    /// Mark this watcher as actively working.
    pub fn activate(&mut self) {
        self.state = WatcherState::Active;
        self.last_change = Instant::now();
        self.last_output_hash = 0;
        if let Some(tracker) = self.tracker.as_mut() {
            match tracker {
                SessionTracker::Codex(codex) => {
                    codex.session_file = None;
                    codex.offset = 0;
                }
                SessionTracker::Claude(claude) => {
                    claude.session_file = None;
                    claude.offset = 0;
                    claude.last_state = TrackerState::Unknown;
                }
            }
        }
    }

    pub fn set_session_id(&mut self, session_id: Option<String>) {
        if let Some(SessionTracker::Claude(claude)) = self.tracker.as_mut() {
            if claude.session_id == session_id {
                return;
            }
            claude.session_id = session_id;
            claude.session_file = None;
            claude.offset = 0;
            claude.last_state = TrackerState::Unknown;
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

    pub fn current_session_id(&self) -> Option<String> {
        match self.tracker.as_ref() {
            Some(SessionTracker::Codex(codex)) => session_file_id(codex.session_file.as_ref()),
            Some(SessionTracker::Claude(claude)) => session_file_id(claude.session_file.as_ref()),
            None => None,
        }
    }

    fn poll_tracker(&mut self) -> Result<TrackerState> {
        let Some(tracker) = self.tracker.as_mut() else {
            return Ok(TrackerState::Unknown);
        };

        match tracker {
            SessionTracker::Codex(codex) => {
                if codex.session_file.is_none() {
                    codex.session_file =
                        discover_codex_session_file(&codex.sessions_root, &codex.cwd)?;
                    if let Some(session_file) = codex.session_file.as_ref() {
                        codex.offset = current_file_len(session_file)?;
                    }
                    return Ok(TrackerState::Unknown);
                }

                let Some(session_file) = codex.session_file.clone() else {
                    return Ok(TrackerState::Unknown);
                };

                if !session_file.exists() {
                    codex.session_file = None;
                    codex.offset = 0;
                    return Ok(TrackerState::Unknown);
                }

                if poll_codex_session_file(&session_file, &mut codex.offset)? {
                    Ok(TrackerState::Completed)
                } else {
                    Ok(TrackerState::Unknown)
                }
            }
            SessionTracker::Claude(claude) => poll_claude_session(claude),
        }
    }

    fn tracker_kind(&self) -> TrackerKind {
        match self.tracker {
            Some(SessionTracker::Codex(_)) => TrackerKind::Codex,
            Some(SessionTracker::Claude(_)) => TrackerKind::Claude,
            None => TrackerKind::None,
        }
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
    let trimmed = recent_non_empty_lines(capture, 5);

    // Claude Code shows "esc to interrupt" in the bottom status bar while working
    for line in &trimmed {
        if line.contains("esc to interrupt") {
            return false;
        }
    }

    // Claude can also land in an interrupt-resolution UI after a blocked tool call
    // or nested interactive flow. That still represents an active task, not an idle
    // prompt waiting for a fresh assignment.
    if trimmed
        .iter()
        .any(|line| line.contains("What should Claude do instead?"))
        || trimmed
            .iter()
            .any(|line| line.contains("Conversation interrupted"))
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScreenState {
    Active,
    Idle,
    Unknown,
}

fn recent_non_empty_lines(capture: &str, limit: usize) -> Vec<&str> {
    capture
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(limit)
        .collect()
}

fn classify_capture_state(capture: &str) -> ScreenState {
    let trimmed = recent_non_empty_lines(capture, 12);

    if trimmed.iter().any(|line| line.contains("esc to interrupt")) {
        return ScreenState::Active;
    }

    if trimmed
        .iter()
        .any(|line| looks_like_claude_spinner_status(line))
    {
        return ScreenState::Active;
    }

    if is_at_agent_prompt(capture) {
        return ScreenState::Idle;
    }

    ScreenState::Unknown
}

fn looks_like_claude_spinner_status(line: &str) -> bool {
    let trimmed = line.trim();
    let Some(first) = trimmed.chars().next() else {
        return false;
    };
    matches!(first, '·' | '✢' | '✳' | '✶' | '✻' | '✽')
        && (trimmed.contains('…') || trimmed.contains("(thinking"))
}

fn next_state_after_capture(
    tracker_kind: TrackerKind,
    screen_state: ScreenState,
    tracker_state: TrackerState,
    stale: bool,
    unchanged_capture: bool,
) -> WatcherState {
    if tracker_state == TrackerState::Completed {
        return WatcherState::Completed;
    }

    if tracker_kind == TrackerKind::Claude {
        match screen_state {
            // Claude's live pane state is more reliable than session logs when
            // multiple matching JSONL files exist. A visible spinner or
            // interrupt bar means working; a clean prompt with neither means
            // idle, even if an old session file still looks active.
            ScreenState::Active => return WatcherState::Active,
            ScreenState::Idle => return WatcherState::Idle,
            ScreenState::Unknown => {}
        }
    }

    match tracker_state {
        TrackerState::Active => return WatcherState::Active,
        TrackerState::Idle => return WatcherState::Idle,
        TrackerState::Completed => return WatcherState::Completed,
        TrackerState::Unknown => {}
    }

    match screen_state {
        ScreenState::Active => WatcherState::Active,
        ScreenState::Idle => WatcherState::Idle,
        ScreenState::Unknown => {
            if stale {
                WatcherState::Stale
            } else if unchanged_capture {
                WatcherState::Active
            } else {
                WatcherState::Active
            }
        }
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

fn default_claude_projects_root() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
        .join(".claude")
        .join("projects")
}

fn current_file_len(path: &Path) -> Result<u64> {
    Ok(fs::metadata(path)?.len())
}

fn session_file_id(path: Option<&PathBuf>) -> Option<String> {
    path.and_then(|path| {
        path.file_stem()
            .and_then(|stem| stem.to_str())
            .map(|stem| stem.to_string())
    })
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

fn discover_claude_session_file(
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

fn poll_claude_session(tracker: &mut ClaudeSessionTracker) -> Result<TrackerState> {
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
    fn claude_historical_interruption_does_not_poison_idle_prompt() {
        let capture = concat!(
            "Interrupted · What should Claude do instead?\n",
            "Lots of old output here\n",
            "\n\n\n\n\n\n\n\n\n\n",
            "────────────────────────────\n",
            "❯ \n",
            "────────────────────────────\n",
            "  ⏵⏵ bypass permissions on (shift+tab to cycle)\n",
        );
        assert!(is_at_agent_prompt(capture));
        assert_eq!(classify_capture_state(capture), ScreenState::Idle);
    }

    #[test]
    fn claude_spinner_status_marks_capture_active() {
        let capture = concat!(
            "✶ Envisioning… (thinking with high effort)\n",
            "────────────────────────────\n",
            "❯ \n",
            "────────────────────────────\n",
            "  ⏵⏵ bypass permissions on (shift+tab to cycle) · esc to interrupt\n",
        );
        assert_eq!(classify_capture_state(capture), ScreenState::Active);
    }

    #[test]
    fn codex_prompt_keeps_active_state_until_completion_event() {
        assert_eq!(
            next_state_after_capture(
                TrackerKind::Codex,
                ScreenState::Idle,
                TrackerState::Unknown,
                false,
                false
            ),
            WatcherState::Idle
        );
        assert_eq!(
            next_state_after_capture(
                TrackerKind::Codex,
                ScreenState::Idle,
                TrackerState::Active,
                false,
                false
            ),
            WatcherState::Active
        );
        assert_eq!(
            next_state_after_capture(
                TrackerKind::Codex,
                ScreenState::Idle,
                TrackerState::Idle,
                false,
                true
            ),
            WatcherState::Idle
        );
        assert_eq!(
            next_state_after_capture(
                TrackerKind::Codex,
                ScreenState::Unknown,
                TrackerState::Unknown,
                true,
                true
            ),
            WatcherState::Stale
        );
        assert_eq!(
            next_state_after_capture(
                TrackerKind::Codex,
                ScreenState::Active,
                TrackerState::Unknown,
                false,
                true
            ),
            WatcherState::Active
        );
        assert_eq!(
            next_state_after_capture(
                TrackerKind::Codex,
                ScreenState::Idle,
                TrackerState::Completed,
                false,
                true
            ),
            WatcherState::Completed
        );
    }

    #[test]
    fn claude_idle_prompt_beats_stale_file_activity() {
        assert_eq!(
            next_state_after_capture(
                TrackerKind::Claude,
                ScreenState::Idle,
                TrackerState::Active,
                false,
                true
            ),
            WatcherState::Idle
        );
    }

    #[test]
    fn claude_spinner_beats_idle_file_state() {
        assert_eq!(
            next_state_after_capture(
                TrackerKind::Claude,
                ScreenState::Active,
                TrackerState::Idle,
                false,
                true
            ),
            WatcherState::Active
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
    fn watcher_exposes_tracker_session_id_from_bound_file() {
        let mut watcher = SessionWatcher::new("%0", "architect", 300, None);
        watcher.tracker = Some(SessionTracker::Claude(ClaudeSessionTracker {
            projects_root: PathBuf::from("/tmp"),
            cwd: PathBuf::from("/repo"),
            session_id: Some("1e94dc68-6004-402a-9a7b-1bfca674806e".to_string()),
            session_file: Some(PathBuf::from(
                "/tmp/-Users-zedmor-project/1e94dc68-6004-402a-9a7b-1bfca674806e.jsonl",
            )),
            offset: 0,
            last_state: TrackerState::Unknown,
        }));

        assert_eq!(
            watcher.current_session_id().as_deref(),
            Some("1e94dc68-6004-402a-9a7b-1bfca674806e")
        );
    }
}
