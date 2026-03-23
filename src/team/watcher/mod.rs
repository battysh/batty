//! Disk-based session monitoring — polls agent output via tmux capture-pane.
//!
//! Detects agent completion, crashes, and staleness by periodically capturing
//! pane output and checking for state changes. For Codex and Claude, this also
//! tails their on-disk session JSONL data to reduce false classifications from
//! stale pane text.

mod claude;
mod codex;
mod screen;

use anyhow::Result;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::tmux;

pub use screen::is_at_agent_prompt;

use claude::ClaudeSessionTracker;
use codex::CodexSessionTracker;
use screen::{classify_capture_state, detect_context_exhausted, next_state_after_capture};

/// State of a watched agent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatcherState {
    /// Agent is actively producing output.
    Active,
    /// Agent CLI has started and is showing its input prompt, ready for messages.
    /// Transitional state between pane creation and first Idle classification.
    Ready,
    /// No agent running in pane (idle / waiting for assignment).
    Idle,
    /// The tmux pane no longer exists or its process has exited.
    PaneDead,
    /// Agent reported that its conversation/session is too large to continue.
    ContextExhausted,
}

pub struct SessionWatcher {
    pub pane_id: String,
    pub member_name: String,
    pub state: WatcherState,
    completion_observed: bool,
    last_output_hash: u64,
    last_capture: String,
    /// Timestamp of the last time pane output changed (hash differed from previous poll).
    last_output_changed_at: Instant,
    tracker: Option<SessionTracker>,
    /// Whether the agent prompt has been observed at least once since creation
    /// or last `activate()`. False means the pane may not be ready for messages.
    ready_confirmed: bool,
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
pub(super) enum TrackerKind {
    None,
    Codex,
    Claude,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TrackerState {
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
            last_output_changed_at: Instant::now(),
            ready_confirmed: false,
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

        // If idle or ready, peek at the pane to detect if the agent started working.
        // This lets the watcher self-heal without requiring explicit activation
        // from the daemon whenever a nudge, standup, or external input arrives.
        if matches!(self.state, WatcherState::Idle | WatcherState::Ready) {
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
            // When we first see the agent prompt, confirm readiness.
            if screen_state == screen::ScreenState::Idle && !self.ready_confirmed {
                self.ready_confirmed = true;
                self.last_capture = capture;
                self.state = WatcherState::Ready;
                return Ok(self.state);
            }
            let tracker_state = self.poll_tracker().unwrap_or(TrackerState::Unknown);
            self.completion_observed = tracker_state == TrackerState::Completed;
            let tracker_kind = self.tracker_kind();
            if !capture.is_empty() {
                self.last_capture = capture;
                let next_state =
                    next_state_after_capture(tracker_kind, screen_state, tracker_state, self.state);
                if next_state != WatcherState::Idle || self.ready_confirmed {
                    self.last_output_hash = simple_hash(&self.last_capture);
                    self.last_output_changed_at = Instant::now();
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
            self.last_output_changed_at = Instant::now();
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

    /// Whether this agent's pane has been confirmed ready for message delivery.
    ///
    /// Returns `true` once the agent prompt has been observed at least once since
    /// the watcher was created or last activated. This prevents injecting messages
    /// into panes where the agent CLI hasn't finished starting.
    pub fn is_ready_for_delivery(&self) -> bool {
        self.ready_confirmed
    }

    /// Externally confirm that the agent pane is ready (e.g. from a delivery
    /// readiness check that observed the prompt). Updates state to Ready if it
    /// was still in the initial Idle state before first readiness confirmation.
    pub fn confirm_ready(&mut self) {
        let was_unconfirmed = !self.ready_confirmed;
        self.ready_confirmed = true;
        if was_unconfirmed && self.state == WatcherState::Idle {
            self.state = WatcherState::Ready;
        }
    }

    /// Mark this watcher as actively working.
    pub fn activate(&mut self) {
        self.state = WatcherState::Active;
        self.completion_observed = false;
        self.last_output_hash = 0;
        self.last_output_changed_at = Instant::now();
        // A message was just injected so the pane was confirmed ready.
        self.ready_confirmed = true;
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

    /// Seconds since the last time pane output changed.
    pub fn secs_since_last_output_change(&self) -> u64 {
        self.last_output_changed_at.elapsed().as_secs()
    }

    /// Get the last captured pane output.
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
                    codex.session_file = codex::discover_codex_session_file(
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

                let state = codex::poll_codex_session_file(
                    &session_file,
                    &mut codex.offset,
                    &mut codex.quality,
                    &mut codex.last_response_hash,
                )?;

                // When idle with no new events, check if a newer session file
                // appeared (agent started a new task). Re-discover so the
                // tracker picks up the latest session.
                if state == TrackerState::Unknown
                    && matches!(current_state, WatcherState::Idle | WatcherState::Ready)
                {
                    if let Some(latest) = codex::discover_codex_session_file(
                        &codex.sessions_root,
                        &codex.cwd,
                        codex.session_id.as_deref(),
                    )? {
                        if latest != session_file {
                            codex.session_file = Some(latest.clone());
                            codex.offset = 0;
                            codex.quality = CodexQualitySignals::default();
                            codex.last_response_hash = None;
                            return codex::poll_codex_session_file(
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
            SessionTracker::Claude(claude) => claude::poll_claude_session(claude),
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

// --- Shared utility functions used by submodules ---

pub(super) fn simple_hash(s: &str) -> u64 {
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

pub(super) fn current_file_len(path: &Path) -> Result<u64> {
    Ok(fs::metadata(path)?.len())
}

pub(super) fn session_file_id(path: Option<&PathBuf>) -> Option<String> {
    path.and_then(|path| {
        path.file_stem()
            .and_then(|stem| stem.to_str())
            .map(|stem| stem.to_string())
    })
}

pub(super) fn read_dir_paths(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        paths.push(entry.path());
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

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
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
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
    #[cfg_attr(not(feature = "integration"), ignore)]
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
    #[cfg_attr(not(feature = "integration"), ignore)]
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
    #[cfg_attr(not(feature = "integration"), ignore)]
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
    #[cfg_attr(not(feature = "integration"), ignore)]
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
    fn production_watcher_has_no_unwrap_or_expect_calls() {
        // Check all submodule files as well as the main module.
        let mod_src = include_str!("mod.rs");
        assert_eq!(
            production_unwrap_expect_count(mod_src),
            0,
            "production watcher/mod.rs should avoid unwrap/expect"
        );
        let screen_src = include_str!("screen.rs");
        assert_eq!(
            production_unwrap_expect_count(screen_src),
            0,
            "production watcher/screen.rs should avoid unwrap/expect"
        );
        let codex_src = include_str!("codex.rs");
        assert_eq!(
            production_unwrap_expect_count(codex_src),
            0,
            "production watcher/codex.rs should avoid unwrap/expect"
        );
        let claude_src = include_str!("claude.rs");
        assert_eq!(
            production_unwrap_expect_count(claude_src),
            0,
            "production watcher/claude.rs should avoid unwrap/expect"
        );
    }

    #[test]
    fn secs_since_last_output_change_starts_at_zero() {
        let w = SessionWatcher::new("%0", "eng-1-1", 300, None);
        // Freshly created watcher — elapsed should be near zero.
        assert!(w.secs_since_last_output_change() < 2);
    }

    #[test]
    fn activate_resets_last_output_changed_at() {
        let mut w = SessionWatcher::new("%0", "eng-1-1", 300, None);
        // Simulate time passing by backdating the field.
        w.last_output_changed_at = Instant::now() - std::time::Duration::from_secs(600);
        assert!(w.secs_since_last_output_change() >= 600);

        w.activate();
        assert!(w.secs_since_last_output_change() < 2);
    }

    // --- Readiness gate tests ---

    #[test]
    fn new_watcher_is_not_ready_for_delivery() {
        let w = SessionWatcher::new("%0", "eng-1-1", 300, None);
        assert!(!w.is_ready_for_delivery());
        assert_eq!(w.state, WatcherState::Idle);
    }

    #[test]
    fn confirm_ready_sets_ready_state() {
        let mut w = SessionWatcher::new("%0", "eng-1-1", 300, None);
        w.confirm_ready();
        assert!(w.is_ready_for_delivery());
        assert_eq!(w.state, WatcherState::Ready);
    }

    #[test]
    fn activate_sets_ready_confirmed() {
        let mut w = SessionWatcher::new("%0", "eng-1-1", 300, None);
        assert!(!w.is_ready_for_delivery());
        w.activate();
        assert!(w.is_ready_for_delivery());
        assert_eq!(w.state, WatcherState::Active);
    }

    #[test]
    fn deactivate_preserves_readiness() {
        let mut w = SessionWatcher::new("%0", "eng-1-1", 300, None);
        w.activate();
        assert!(w.is_ready_for_delivery());
        w.deactivate();
        assert!(w.is_ready_for_delivery());
        assert_eq!(w.state, WatcherState::Idle);
    }

    #[test]
    fn confirm_ready_on_already_idle_with_completion_does_not_override() {
        let mut w = SessionWatcher::new("%0", "eng-1-1", 300, None);
        w.activate();
        w.deactivate();
        w.confirm_ready();
        assert_eq!(w.state, WatcherState::Idle);
        assert!(w.is_ready_for_delivery());
    }
}
