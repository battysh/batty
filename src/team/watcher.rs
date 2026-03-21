//! Disk-based session monitoring — polls agent output via tmux capture-pane.
//!
//! Detects agent completion, crashes, and staleness by periodically capturing
//! pane output and checking for state changes. For Codex and Claude, this also
//! tails their on-disk session JSONL data to reduce false classifications from
//! stale pane text.

use anyhow::Result;
use serde_json::Value;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::tmux;

/// State of a watched agent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatcherState {
    /// Agent is actively producing output.
    Active,
    /// No agent running in pane (idle / waiting for assignment).
    Idle,
    /// The tmux pane no longer exists or its process has exited.
    PaneDead,
    /// Agent reported that its conversation/session is too large to continue.
    ContextExhausted,
}

pub struct SessionWatcher {
    pub pane_id: String,
    #[allow(dead_code)] // Useful for diagnostics; currently the map key is used instead.
    pub member_name: String,
    pub state: WatcherState,
    completion_observed: bool,
    last_output_hash: u64,
    last_capture: String,
    tracker: Option<SessionTracker>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodexQualitySignals {
    pub last_response_chars: Option<usize>,
    pub shortening_streak: u32,
    pub repeated_output_streak: u32,
    pub shrinking_responses: bool,
    pub repeated_identical_outputs: bool,
    pub tool_failure_message: Option<String>,
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
    session_id: Option<String>,
    session_file: Option<PathBuf>,
    offset: u64,
    quality: CodexQualitySignals,
    last_response_hash: Option<u64>,
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
        _stale_secs: u64,
        tracker: Option<SessionTrackerConfig>,
    ) -> Self {
        Self {
            pane_id: pane_id.to_string(),
            member_name: member_name.to_string(),
            state: WatcherState::Idle,
            completion_observed: false,
            last_output_hash: 0,
            last_capture: String::new(),
            tracker: tracker.map(|tracker| match tracker {
                SessionTrackerConfig::Codex { cwd } => SessionTracker::Codex(CodexSessionTracker {
                    sessions_root: default_codex_sessions_root(),
                    cwd,
                    session_id: None,
                    session_file: None,
                    offset: 0,
                    quality: CodexQualitySignals::default(),
                    last_response_hash: None,
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
            self.state = WatcherState::PaneDead;
            return Ok(self.state);
        }

        // Check if pane process died
        if tmux::pane_dead(&self.pane_id).unwrap_or(false) {
            self.state = WatcherState::PaneDead;
            return Ok(self.state);
        }

        // If idle, peek at the pane to detect if the agent started working.
        // This lets the watcher self-heal without requiring explicit activation
        // from the daemon whenever a nudge, standup, or external input arrives.
        if self.state == WatcherState::Idle {
            let capture = match tmux::capture_pane(&self.pane_id) {
                Ok(capture) => capture,
                Err(_) => {
                    self.state = WatcherState::PaneDead;
                    return Ok(self.state);
                }
            };
            if detect_context_exhausted(&capture) {
                self.last_capture = capture;
                self.state = WatcherState::ContextExhausted;
                return Ok(self.state);
            }
            let screen_state = classify_capture_state(&capture);
            let tracker_state = self.poll_tracker().unwrap_or(TrackerState::Unknown);
            self.completion_observed = tracker_state == TrackerState::Completed;
            let tracker_kind = self.tracker_kind();
            if !capture.is_empty() {
                self.last_capture = capture;
                let next_state =
                    next_state_after_capture(tracker_kind, screen_state, tracker_state, self.state);
                if next_state != WatcherState::Idle {
                    self.last_output_hash = simple_hash(&self.last_capture);
                    self.state = next_state;
                }
            }
            return Ok(self.state);
        }

        // Capture current pane content
        let capture = match tmux::capture_pane(&self.pane_id) {
            Ok(capture) => capture,
            Err(_) => {
                self.state = WatcherState::PaneDead;
                return Ok(self.state);
            }
        };
        if detect_context_exhausted(&capture) {
            self.last_capture = capture;
            self.state = WatcherState::ContextExhausted;
            return Ok(self.state);
        }
        let hash = simple_hash(&capture);
        let screen_state = classify_capture_state(&capture);
        let tracker_state = self.poll_tracker().unwrap_or(TrackerState::Unknown);
        self.completion_observed = tracker_state == TrackerState::Completed;
        let tracker_kind = self.tracker_kind();

        if hash != self.last_output_hash {
            self.last_output_hash = hash;
            self.last_capture = capture;
            self.state =
                next_state_after_capture(tracker_kind, screen_state, tracker_state, self.state);
        } else {
            self.last_capture = capture;
            self.state =
                next_state_after_capture(tracker_kind, screen_state, tracker_state, self.state);
        }

        Ok(self.state)
    }

    /// Mark this watcher as actively working.
    pub fn activate(&mut self) {
        self.state = WatcherState::Active;
        self.completion_observed = false;
        self.last_output_hash = 0;
        if let Some(tracker) = self.tracker.as_mut() {
            match tracker {
                SessionTracker::Codex(codex) => {
                    codex.session_file = None;
                    codex.offset = 0;
                    codex.quality = CodexQualitySignals::default();
                    codex.last_response_hash = None;
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
        if let Some(tracker) = self.tracker.as_mut() {
            match tracker {
                SessionTracker::Codex(codex) => {
                    if codex.session_id == session_id {
                        return;
                    }
                    self.completion_observed = false;
                    codex.session_id = session_id;
                    codex.session_file = None;
                    codex.offset = 0;
                    codex.quality = CodexQualitySignals::default();
                    codex.last_response_hash = None;
                }
                SessionTracker::Claude(claude) => {
                    if claude.session_id == session_id {
                        return;
                    }
                    self.completion_observed = false;
                    claude.session_id = session_id;
                    claude.session_file = None;
                    claude.offset = 0;
                    claude.last_state = TrackerState::Unknown;
                }
            }
        }
    }

    /// Mark this watcher as idle.
    pub fn deactivate(&mut self) {
        self.state = WatcherState::Idle;
        self.completion_observed = false;
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

    pub fn current_session_size_bytes(&self) -> Option<u64> {
        let path = match self.tracker.as_ref() {
            Some(SessionTracker::Codex(codex)) => codex.session_file.as_ref(),
            Some(SessionTracker::Claude(claude)) => claude.session_file.as_ref(),
            None => None,
        }?;
        fs::metadata(path).ok().map(|metadata| metadata.len())
    }

    pub fn codex_quality_signals(&self) -> Option<CodexQualitySignals> {
        match self.tracker.as_ref() {
            Some(SessionTracker::Codex(codex)) => Some(codex.quality.clone()),
            _ => None,
        }
    }

    pub fn take_completion_event(&mut self) -> bool {
        let observed = self.completion_observed;
        self.completion_observed = false;
        observed
    }

    fn poll_tracker(&mut self) -> Result<TrackerState> {
        let current_state = self.state;
        let Some(tracker) = self.tracker.as_mut() else {
            return Ok(TrackerState::Unknown);
        };

        match tracker {
            SessionTracker::Codex(codex) => {
                if codex.session_file.is_none() {
                    codex.session_file = discover_codex_session_file(
                        &codex.sessions_root,
                        &codex.cwd,
                        codex.session_id.as_deref(),
                    )?;
                    if let Some(session_file) = codex.session_file.as_ref() {
                        codex.offset = current_file_len(session_file)?;
                    }
                    codex.quality = CodexQualitySignals::default();
                    codex.last_response_hash = None;
                    return Ok(TrackerState::Unknown);
                }

                let Some(session_file) = codex.session_file.clone() else {
                    return Ok(TrackerState::Unknown);
                };

                if !session_file.exists() {
                    codex.session_file = None;
                    codex.offset = 0;
                    codex.quality = CodexQualitySignals::default();
                    codex.last_response_hash = None;
                    return Ok(TrackerState::Unknown);
                }

                let state = poll_codex_session_file(
                    &session_file,
                    &mut codex.offset,
                    &mut codex.quality,
                    &mut codex.last_response_hash,
                )?;

                // When idle with no new events, check if a newer session file
                // appeared (agent started a new task). Re-discover so the
                // tracker picks up the latest session.
                if state == TrackerState::Unknown && current_state == WatcherState::Idle {
                    if let Some(latest) = discover_codex_session_file(
                        &codex.sessions_root,
                        &codex.cwd,
                        codex.session_id.as_deref(),
                    )? {
                        if latest != session_file {
                            codex.session_file = Some(latest.clone());
                            codex.offset = 0;
                            codex.quality = CodexQualitySignals::default();
                            codex.last_response_hash = None;
                            return poll_codex_session_file(
                                &latest,
                                &mut codex.offset,
                                &mut codex.quality,
                                &mut codex.last_response_hash,
                            );
                        }
                    }
                }

                Ok(state)
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
pub fn is_at_agent_prompt(capture: &str) -> bool {
    // Use 12 non-empty lines to account for Claude's separators and status
    // bar pushing the prompt further up than a tight tail window.
    let trimmed = recent_non_empty_lines(capture, 12);

    // Claude Code shows "esc to interrupt" in the current bottom status bar
    // while working. Restrict this check to the raw bottom window so older
    // non-empty lines higher in the transcript do not pin the watcher active.
    for line in &recent_lines(capture, 6) {
        if is_live_interrupt_footer(line) {
            return false;
        }
    }

    for line in &trimmed {
        let l = line.trim();
        // Claude Code idle prompt
        if starts_with_agent_prompt(l, '❯') {
            return true;
        }
        // Codex idle composer prompt
        if starts_with_agent_prompt(l, '›') {
            return true;
        }
        // Fell back to shell
        if l.ends_with("$ ") || l == "$" {
            return true;
        }
    }
    false
}

fn starts_with_agent_prompt(line: &str, prompt: char) -> bool {
    let Some(rest) = line.strip_prefix(prompt) else {
        return false;
    };
    rest.is_empty()
        || rest
            .chars()
            .next()
            .map(char::is_whitespace)
            .unwrap_or(false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScreenState {
    Active,
    Idle,
    ContextExhausted,
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

fn recent_lines(capture: &str, limit: usize) -> Vec<&str> {
    capture.lines().rev().take(limit).collect()
}

fn classify_capture_state(capture: &str) -> ScreenState {
    let trimmed = recent_non_empty_lines(capture, 12);

    if recent_lines(capture, 6)
        .iter()
        .any(|line| is_live_interrupt_footer(line))
    {
        return ScreenState::Active;
    }

    if capture_contains_context_exhaustion(capture) {
        return ScreenState::ContextExhausted;
    }

    if is_at_agent_prompt(capture) {
        return ScreenState::Idle;
    }

    if trimmed
        .iter()
        .any(|line| looks_like_claude_spinner_status(line))
    {
        return ScreenState::Active;
    }

    ScreenState::Unknown
}

fn detect_context_exhausted(capture: &str) -> bool {
    capture_contains_context_exhaustion(capture)
}

fn looks_like_claude_spinner_status(line: &str) -> bool {
    let trimmed = line.trim();
    let Some(first) = trimmed.chars().next() else {
        return false;
    };
    matches!(first, '·' | '✢' | '✳' | '✶' | '✻' | '✽')
        && (trimmed.contains('…') || trimmed.contains("(thinking"))
}

fn is_live_interrupt_footer(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.contains("esc to interrupt")
        || trimmed.contains("esc to inter")
        || trimmed.contains("esc to in…")
        || trimmed.contains("esc to in...")
}

fn capture_contains_context_exhaustion(capture: &str) -> bool {
    let lowered = capture.to_ascii_lowercase();
    lowered.contains("context window exceeded")
        || lowered.contains("context window is full")
        || lowered.contains("conversation is too long")
        || lowered.contains("maximum context length")
        || lowered.contains("context limit reached")
        || lowered.contains("truncated due to context limit")
        || lowered.contains("input exceeds the model")
        || lowered.contains("prompt is too long")
}

fn next_state_after_capture(
    tracker_kind: TrackerKind,
    screen_state: ScreenState,
    tracker_state: TrackerState,
    previous_state: WatcherState,
) -> WatcherState {
    if screen_state == ScreenState::ContextExhausted {
        return WatcherState::ContextExhausted;
    }

    if tracker_kind == TrackerKind::Claude {
        match screen_state {
            // Claude's live pane state is more reliable than session logs when
            // multiple matching JSONL files exist. A visible spinner or
            // interrupt bar means working; a clean prompt with neither means
            // idle, even if an old session file still looks active.
            ScreenState::Active => return WatcherState::Active,
            ScreenState::Idle => return WatcherState::Idle,
            ScreenState::ContextExhausted => return WatcherState::ContextExhausted,
            ScreenState::Unknown => {}
        }
    }

    match tracker_state {
        TrackerState::Active => return WatcherState::Active,
        TrackerState::Idle | TrackerState::Completed => return WatcherState::Idle,
        TrackerState::Unknown => {}
    }

    match screen_state {
        ScreenState::Active => WatcherState::Active,
        ScreenState::Idle => WatcherState::Idle,
        ScreenState::ContextExhausted => WatcherState::ContextExhausted,
        ScreenState::Unknown => previous_state,
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

fn discover_codex_session_file(
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

fn poll_codex_session_file(
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

fn update_codex_quality_signals(
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
    use serial_test::serial;
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
    fn claude_working_not_idle_when_interrupt_footer_is_truncated() {
        let capture = concat!(
            "✢ Cascading… (48s · ↓ 130 tokens · thought for 17s)\n",
            "  ⎿  Tip: Use /btw to ask a quick side question\n",
            "────────────────────────────\n",
            "❯ \n",
            "────────────────────────────\n",
            "  ⏵⏵ bypass permissions on (shift+tab to cycle) · esc to in…\n",
        );
        assert!(!is_at_agent_prompt(capture));
        assert_eq!(classify_capture_state(capture), ScreenState::Active);
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
        assert_eq!(classify_capture_state(capture), ScreenState::Idle);
    }

    #[test]
    fn claude_context_window_message_marks_capture_exhausted() {
        let capture = concat!(
            "Claude cannot continue: conversation is too long.\n",
            "Start a new conversation or clear earlier context.\n",
            "❯ \n",
        );
        assert_eq!(
            classify_capture_state(capture),
            ScreenState::ContextExhausted
        );
    }

    #[test]
    fn codex_context_limit_message_marks_capture_exhausted() {
        let capture = concat!(
            "Request truncated due to context limit.\n",
            "Please start a fresh session with a smaller prompt.\n",
            "› \n",
        );
        assert_eq!(
            classify_capture_state(capture),
            ScreenState::ContextExhausted
        );
    }

    #[test]
    fn ambiguous_context_wording_does_not_mark_capture_exhausted() {
        let capture = concat!(
            "We should reduce context window usage in the next refactor.\n",
            "That note is informational only.\n",
        );
        assert_eq!(classify_capture_state(capture), ScreenState::Unknown);
    }

    #[test]
    fn claude_pasted_text_prompt_counts_as_idle() {
        let capture = concat!(
            "✻ Crunched for 54s\n",
            "────────────────────────────────────────────────────────\n",
            "❯\u{00a0}[Pasted text #2 +40 lines]\n",
            "  --- Message from human ---\n",
            "  Provide me report of latest development\n",
            "  --- end message ---\n",
            "  To reply, run: batty send human \"<your response>\"\n",
            "────────────────────────────────────────────────────────\n",
            "  ⏵⏵ bypass permissions on (shift+tab to cycle)\n",
        );
        assert!(is_at_agent_prompt(capture));
        assert_eq!(classify_capture_state(capture), ScreenState::Idle);
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
        assert!(is_at_agent_prompt(capture));
        assert_eq!(classify_capture_state(capture), ScreenState::Idle);
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
    fn claude_recent_interruption_without_esc_still_counts_as_idle() {
        let capture = concat!(
            "--- Message from manager ---\n",
            "No worries about the interrupted background task.\n",
            "--- end message ---\n",
            "To reply, run: batty send manager \"<your response>\"\n",
            "  ⎿  Interrupted · What should Claude do instead?\n",
            "\n",
            "⏺ Background command stopped\n",
            "────────────────────────────────────────────────────────\n",
            "❯ \n",
            "────────────────────────────────────────────────────────\n",
            "  ⏵⏵ bypass permissions on (shift+tab to cycle)\n",
        );
        assert!(is_at_agent_prompt(capture));
        assert_eq!(classify_capture_state(capture), ScreenState::Idle);
    }

    #[test]
    fn stale_esc_line_above_latest_prompt_does_not_pin_active() {
        let capture = concat!(
            "⏺ Bash(tmux capture-pane -t batty-mafia-adversarial-research:0.5 -p 2>/dev/null | tail -30)\n",
            "  ⎿  • Working (5s • esc to interrupt)\n",
            "     • Messages to be submitted after next tool call (press\n",
            "     … +9 lines (ctrl+o to expand)\n",
            "\n",
            "⏺ Good, the message is queued and will be processed. Let me wait a bit and check back.\n",
            "\n",
            "⏺ Bash(sleep 30 && tmux capture-pane -t batty-mafia-adversarial-research:0.5 -p 2>/dev/null | tail -30)\n",
            "  ⎿  Interrupted · What should Claude do instead?\n",
            "\n",
            "───────────────────────────────────────────────────────────────────────────────────────────────────────────\n",
            "❯\u{00a0}\n",
            "───────────────────────────────────────────────────────────────────────────────────────────────────────────\n",
            "  ⏵⏵ bypass permissions on (shift+tab to cycle)\n",
        );
        assert!(is_at_agent_prompt(capture));
        assert_eq!(classify_capture_state(capture), ScreenState::Idle);
    }

    #[test]
    fn claude_prompt_deep_in_output_still_detected_as_idle() {
        // Regression: Claude Code's idle layout can push the ❯ prompt past
        // a narrow line window when tool output, separator lines, and the
        // status bar fill the bottom of the pane. The old 5-line window
        // missed this and returned Unknown, which fell through to a stale
        // tracker state and blocked message delivery.
        let capture = concat!(
            "⏺ Task merged to main.\n",
            "\n",
            "  ┌──────────┬──────────────────────┬──────────┐\n",
            "  │ Engineer │       Assignment       │  Status  │\n",
            "  ├──────────┼──────────────────────┼──────────┤\n",
            "  │ eng-1-1  │ Add features           │ Assigned │\n",
            "  └──────────┴──────────────────────┴──────────┘\n",
            "\n",
            "✻ Sautéed for 1m 56s\n",
            "\n",
            "────────────────────────────────────────────────────────\n",
            "❯ \n",
            "────────────────────────────────────────────────────────\n",
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
    fn claude_truncated_interrupt_footer_marks_capture_active() {
        let capture = concat!(
            "✻ Baked for 4m 30s\n",
            "────────────────────────────\n",
            "❯ \n",
            "────────────────────────────\n",
            "  ⏵⏵ bypass permissions on (shift+tab to cycle) · esc to in…\n",
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
                WatcherState::Idle,
            ),
            WatcherState::Idle
        );
        assert_eq!(
            next_state_after_capture(
                TrackerKind::Codex,
                ScreenState::Idle,
                TrackerState::Active,
                WatcherState::Idle,
            ),
            WatcherState::Active
        );
        assert_eq!(
            next_state_after_capture(
                TrackerKind::Codex,
                ScreenState::Idle,
                TrackerState::Idle,
                WatcherState::Active,
            ),
            WatcherState::Idle
        );
        assert_eq!(
            next_state_after_capture(
                TrackerKind::Codex,
                ScreenState::Unknown,
                TrackerState::Unknown,
                WatcherState::Active,
            ),
            WatcherState::Active
        );
        assert_eq!(
            next_state_after_capture(
                TrackerKind::Codex,
                ScreenState::Active,
                TrackerState::Unknown,
                WatcherState::Idle,
            ),
            WatcherState::Active
        );
        assert_eq!(
            next_state_after_capture(
                TrackerKind::Codex,
                ScreenState::Idle,
                TrackerState::Completed,
                WatcherState::Active,
            ),
            WatcherState::Idle
        );
        assert_eq!(
            next_state_after_capture(
                TrackerKind::Codex,
                ScreenState::ContextExhausted,
                TrackerState::Unknown,
                WatcherState::Active,
            ),
            WatcherState::ContextExhausted
        );
    }

    #[test]
    fn claude_idle_prompt_beats_stale_file_activity() {
        assert_eq!(
            next_state_after_capture(
                TrackerKind::Claude,
                ScreenState::Idle,
                TrackerState::Active,
                WatcherState::Active,
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
                WatcherState::Idle,
            ),
            WatcherState::Active
        );
    }

    #[test]
    #[serial]
    fn idle_poll_consumes_non_empty_capture() {
        let session = "batty-test-watcher-idle-poll";
        let _ = crate::tmux::kill_session(session);

        crate::tmux::create_session(
            session,
            "bash",
            &[
                "-lc".to_string(),
                "printf 'watcher-idle-poll\\n'; sleep 3".to_string(),
            ],
            "/tmp",
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));

        let pane_id = crate::tmux::pane_id(session).unwrap();
        let mut watcher = SessionWatcher::new(&pane_id, "eng-1-1", 300, None);

        assert_eq!(watcher.poll().unwrap(), WatcherState::Idle);
        assert!(!watcher.last_output().is_empty());

        crate::tmux::kill_session(session).unwrap();
    }

    #[test]
    #[serial]
    fn active_poll_updates_state_when_capture_changes() {
        let session = "batty-test-watcher-active-change";
        let _ = crate::tmux::kill_session(session);

        crate::tmux::create_session(
            session,
            "bash",
            &[
                "-lc".to_string(),
                "printf 'watcher-active-change\\n'; sleep 3".to_string(),
            ],
            "/tmp",
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));

        let pane_id = crate::tmux::pane_id(session).unwrap();
        let mut watcher = SessionWatcher::new(&pane_id, "eng-1-1", 300, None);
        watcher.state = WatcherState::Active;

        assert_eq!(watcher.poll().unwrap(), WatcherState::Active);
        assert_ne!(watcher.last_output_hash, 0);
        assert!(!watcher.last_output().is_empty());

        crate::tmux::kill_session(session).unwrap();
    }

    #[test]
    #[serial]
    fn idle_poll_detects_context_exhaustion() {
        let session = format!("batty-test-watcher-context-exhaust-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        crate::tmux::create_session(&session, "cat", &[], "/tmp").unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        crate::tmux::send_keys(&pane_id, "Conversation is too long to continue.", true).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(150));

        let mut watcher = SessionWatcher::new(&pane_id, "eng-1-1", 300, None);

        assert_eq!(watcher.poll().unwrap(), WatcherState::ContextExhausted);
        assert!(watcher.last_output().contains("Conversation is too long"));

        crate::tmux::kill_session(&session).unwrap();
    }

    #[test]
    #[serial]
    fn active_poll_keeps_previous_state_when_capture_is_unchanged() {
        let session = "batty-test-watcher-unchanged";
        let _ = crate::tmux::kill_session(session);

        crate::tmux::create_session(
            session,
            "bash",
            &[
                "-lc".to_string(),
                "printf 'watcher-unchanged\\n'; sleep 3".to_string(),
            ],
            "/tmp",
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));

        let pane_id = crate::tmux::pane_id(session).unwrap();
        let capture = crate::tmux::capture_pane(&pane_id).unwrap();
        let mut watcher = SessionWatcher::new(&pane_id, "eng-1-1", 0, None);
        watcher.state = WatcherState::Active;
        watcher.last_capture = capture.clone();
        watcher.last_output_hash = simple_hash(&capture);

        assert_eq!(watcher.poll().unwrap(), WatcherState::Active);

        crate::tmux::kill_session(session).unwrap();
    }

    #[test]
    fn missing_pane_poll_reports_pane_dead() {
        let mut watcher = SessionWatcher::new("%999999", "eng-1-1", 300, None);
        assert_eq!(watcher.poll().unwrap(), WatcherState::PaneDead);
    }

    #[test]
    #[serial]
    fn pane_dead_poll_reports_pane_dead() {
        let session = format!("batty-test-watcher-pane-dead-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        crate::tmux::create_session(&session, "bash", &[], "/tmp").unwrap();
        crate::tmux::create_window(&session, "keeper", "sleep", &["30".to_string()], "/tmp")
            .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        std::process::Command::new("tmux")
            .args(["set-option", "-p", "-t", &pane_id, "remain-on-exit", "on"])
            .output()
            .unwrap();

        crate::tmux::send_keys(&pane_id, "exit", true).unwrap();
        for _ in 0..5 {
            if crate::tmux::pane_dead(&pane_id).unwrap_or(false) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        assert!(crate::tmux::pane_dead(&pane_id).unwrap());

        let mut watcher = SessionWatcher::new(&pane_id, "eng-1-1", 300, None);
        assert_eq!(watcher.poll().unwrap(), WatcherState::PaneDead);

        crate::tmux::kill_session(&session).unwrap();
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
            concat!(
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call_output\",\"call_id\":\"call_123\",\"output\":\"exec_command failed: SandboxDenied { message: \\\"operation not permitted\\\" }\"}}\n",
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

        assert_eq!(
            quality.tool_failure_message.as_deref(),
            Some("exec_command failed: SandboxDenied { message: \"operation not permitted\" }")
        );
    }

    #[test]
    fn watcher_exposes_codex_quality_signals() {
        let mut watcher = SessionWatcher::new("%0", "eng-1-1", 300, None);
        watcher.tracker = Some(SessionTracker::Codex(CodexSessionTracker {
            sessions_root: PathBuf::from("/tmp"),
            cwd: PathBuf::from("/repo"),
            session_id: None,
            session_file: None,
            offset: 0,
            quality: CodexQualitySignals {
                last_response_chars: Some(12),
                shortening_streak: 2,
                repeated_output_streak: 3,
                shrinking_responses: true,
                repeated_identical_outputs: true,
                tool_failure_message: Some("exec_command failed".to_string()),
            },
            last_response_hash: Some(simple_hash("same response")),
        }));

        assert_eq!(
            watcher.codex_quality_signals(),
            Some(CodexQualitySignals {
                last_response_chars: Some(12),
                shortening_streak: 2,
                repeated_output_streak: 3,
                shrinking_responses: true,
                repeated_identical_outputs: true,
                tool_failure_message: Some("exec_command failed".to_string()),
            })
        );
    }

    #[test]
    fn watcher_set_session_id_rebinds_codex_tracker() {
        let mut watcher = SessionWatcher::new("%0", "eng-1", 300, None);
        watcher.tracker = Some(SessionTracker::Codex(CodexSessionTracker {
            sessions_root: PathBuf::from("/tmp"),
            cwd: PathBuf::from("/repo"),
            session_id: Some("old-session".to_string()),
            session_file: Some(PathBuf::from("/tmp/old-session.jsonl")),
            offset: 42,
            quality: CodexQualitySignals {
                last_response_chars: Some(12),
                shortening_streak: 1,
                repeated_output_streak: 2,
                shrinking_responses: true,
                repeated_identical_outputs: true,
                tool_failure_message: Some("failure".to_string()),
            },
            last_response_hash: Some(simple_hash("old")),
        }));

        watcher.set_session_id(Some("new-session".to_string()));

        let Some(SessionTracker::Codex(codex)) = watcher.tracker.as_ref() else {
            panic!("expected codex tracker");
        };
        assert_eq!(codex.session_id.as_deref(), Some("new-session"));
        assert!(codex.session_file.is_none());
        assert_eq!(codex.offset, 0);
        assert_eq!(codex.quality, CodexQualitySignals::default());
        assert!(codex.last_response_hash.is_none());
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
