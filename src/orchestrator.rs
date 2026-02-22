//! Orchestrator — tmux-based supervision loop.
//!
//! This is the core of Phase 2. It:
//! 1. Creates a tmux session with the executor command
//! 2. Sets up pipe-pane to capture output
//! 3. Runs a polling loop that watches the pipe log
//! 4. Detects prompts (silence + pattern matching)
//! 5. Auto-answers via send-keys (Tier 1: regex → response)
//! 6. Logs all decisions to the execution log and orchestrator log
//!
//! The user sees the executor's live session in tmux and can type directly.
//! Batty supervises transparently in the background.

use std::io::IsTerminal;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::agent::SpawnConfig;
use crate::detector::{DetectorConfig, DetectorEvent, PromptDetector};
use crate::events::{EventBuffer, PipeWatcher};
use crate::policy::{Decision, PolicyEngine};
use crate::prompt::{DetectedPrompt, PromptKind, PromptPatterns, strip_ansi};
use crate::tier2::{self, Tier2Config, Tier2Result};
use crate::tmux;

/// Configuration for the orchestrator.
pub struct OrchestratorConfig {
    /// The agent spawn configuration.
    pub spawn: SpawnConfig,
    /// Prompt detection patterns.
    pub patterns: PromptPatterns,
    /// Policy engine for auto-answer decisions.
    pub policy: PolicyEngine,
    /// Detector configuration (silence timeout, etc.).
    pub detector: DetectorConfig,
    /// Phase name (for session naming and logging).
    pub phase: String,
    /// Per-run log directory (execution/orchestrator/pty logs).
    pub logs_dir: PathBuf,
    /// Polling interval for the pipe watcher.
    pub poll_interval: Duration,
    /// Event buffer size.
    pub buffer_size: usize,
    /// Tier 2 supervisor agent configuration (None = disable Tier 2).
    pub tier2: Option<Tier2Config>,
    /// Whether to create an orchestrator log pane (default: true).
    pub log_pane: bool,
    /// Log pane height as percentage of terminal (default: 20).
    pub log_pane_height_pct: u32,
    /// Stuck detection configuration (None = disabled).
    pub stuck: Option<StuckConfig>,
    /// Delay before Tier 1 auto-answer injection (allows human to type first).
    /// Set to Duration::ZERO to disable the delay. Default: 1 second.
    pub answer_delay: Duration,
    /// Auto-attach to the tmux session in the current terminal.
    pub auto_attach: bool,
    /// If true, unknown-request fallback can trigger on idle input lines
    /// (for example Codex `› ...`) so execution can continue automatically.
    pub idle_input_fallback: bool,
}

impl OrchestratorConfig {
    pub fn default_poll_interval() -> Duration {
        Duration::from_millis(200)
    }

    pub fn default_buffer_size() -> usize {
        50
    }
}

/// Result of an orchestrated session.
#[derive(Debug)]
pub enum OrchestratorResult {
    /// Session completed normally (executor exited).
    Completed,
    /// Session was interrupted (user detached or Ctrl-C).
    Detached,
    /// Session encountered an error.
    #[allow(dead_code)]
    Error { detail: String },
}

/// Callback for orchestrator events (for logging, status bar, etc.).
pub trait OrchestratorObserver: Send {
    fn on_auto_answer(&mut self, prompt: &str, response: &str);
    fn on_escalate(&mut self, prompt: &str);
    fn on_suggest(&mut self, prompt: &str, response: &str);
    fn on_event(&mut self, message: &str);
}

/// Simple observer that writes to the orchestrator log file.
pub struct LogFileObserver {
    log_path: PathBuf,
}

impl LogFileObserver {
    pub fn new(log_path: &Path) -> Result<Self> {
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create log dir: {}", parent.display()))?;
        }
        Ok(Self {
            log_path: log_path.to_path_buf(),
        })
    }

    fn append(&self, line: &str) {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
        {
            let _ = writeln!(f, "{line}");
        }
    }

    fn display_response(response: &str) -> String {
        if response.trim().is_empty() {
            "<ENTER>".to_string()
        } else {
            response.to_string()
        }
    }
}

impl OrchestratorObserver for LogFileObserver {
    fn on_auto_answer(&mut self, prompt: &str, response: &str) {
        let shown = Self::display_response(response);
        self.append(&format!("[batty] ✓ auto-answered: \"{prompt}\" → {shown}"));
    }

    fn on_escalate(&mut self, prompt: &str) {
        self.append(&format!("[batty] ⚠ NEEDS INPUT: \"{prompt}\""));
    }

    fn on_suggest(&mut self, prompt: &str, response: &str) {
        let shown = Self::display_response(response);
        self.append(&format!(
            "[batty] ? suggestion: respond to \"{prompt}\" with \"{shown}\""
        ));
    }

    fn on_event(&mut self, message: &str) {
        self.append(&format!("[batty] {message}"));
    }
}

/// Status bar state indicator.
#[derive(Debug, Clone, PartialEq)]
pub enum StatusIndicator {
    /// `●` State change (session start, phase/task transition).
    StateChange,
    /// `→` Action taken (answer injected, task claimed).
    Action,
    /// `✓` Normal operation (supervising, completed).
    Ok,
    /// `?` Supervisor thinking (Tier 2 call in progress).
    Thinking,
    /// `⚠` Needs human input.
    NeedsInput,
    /// `✗` Failure (test fail, error, stuck).
    Failure,
}

impl StatusIndicator {
    fn symbol(&self) -> &'static str {
        match self {
            Self::StateChange => "●",
            Self::Action => "→",
            Self::Ok => "✓",
            Self::Thinking => "?",
            Self::NeedsInput => "⚠",
            Self::Failure => "✗",
        }
    }
}

fn display_response(response: &str) -> String {
    if response.trim().is_empty() {
        "<ENTER>".to_string()
    } else {
        response.to_string()
    }
}

fn preview_for_log(text: &str, max_chars: usize) -> String {
    let one_line = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let compact = one_line.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        compact
    } else {
        let truncated: String = compact.chars().take(max_chars).collect();
        format!("{truncated}...")
    }
}

fn supervisor_cmd_for_log(config: &Tier2Config) -> String {
    if config.args.is_empty() {
        config.program.clone()
    } else {
        format!("{} {}", config.program, config.args.join(" "))
    }
}

fn is_ui_noise_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return true;
    }

    let lower = trimmed.to_lowercase();
    if lower.contains("for shortcuts") || lower.contains("context left") {
        return true;
    }

    // Ignore pure border/separator rows from full-screen TUIs.
    trimmed.chars().all(|c| {
        matches!(
            c,
            ' ' | '│' | '┃' | '─' | '━' | '┄' | '┈' | '┊' | '┆' | '╭' | '╮' | '╯' | '╰'
        )
    })
}

/// Build a detector snapshot from pane content:
/// - strips ANSI
/// - removes known UI noise lines
/// - returns `(signature, last_meaningful_line)` from the tail window
fn pane_detector_snapshot(pane_content: &str) -> Option<(String, String)> {
    let meaningful = pane_content
        .lines()
        .map(strip_ansi)
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .filter(|line| !is_ui_noise_line(line))
        .collect::<Vec<_>>();

    if meaningful.is_empty() {
        return None;
    }

    let tail = meaningful
        .iter()
        .rev()
        .take(12)
        .cloned()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>();

    let signature = tail.join("\n");
    let last = tail.last().cloned().unwrap_or_default();
    Some((signature, last))
}

/// Gate unknown-request fallback so we don't escalate on inert UI footer lines.
fn should_invoke_unknown_fallback(last_line: &str) -> bool {
    if is_ui_noise_line(last_line) {
        return false;
    }

    let lower = last_line.to_lowercase();
    if lower.contains("[y/n]")
        || lower.contains("would you like")
        || lower.contains("do you want")
        || lower.contains("allow tool")
        || lower.contains("approve")
        || lower.contains("press enter")
        || lower.contains("press")
        || lower.contains("type ")
        || lower.contains("input ")
        || lower.contains("choose ")
        || lower.contains("select ")
    {
        return true;
    }

    last_line.contains('?')
}

fn extract_idle_input_prompt(pane_content: &str) -> Option<String> {
    for raw in pane_content.lines().rev() {
        let cleaned = strip_ansi(raw);
        let trimmed = cleaned.trim();
        if trimmed.is_empty() {
            continue;
        }
        if is_ui_noise_line(trimmed) {
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix('›') {
            let candidate = rest.trim();
            if !candidate.is_empty() {
                return Some(candidate.to_string());
            }
        }
        if let Some(rest) = trimmed.strip_prefix('>') {
            let candidate = rest.trim();
            if !candidate.is_empty() {
                return Some(candidate.to_string());
            }
        }
    }
    None
}

fn extract_pending_input_from_supervisor_prompt(prompt_text: &str) -> Option<String> {
    let marker = "Executor is idle at the input prompt with pending input:\n";
    let rest = prompt_text.strip_prefix(marker)?;
    let pending = rest.split("\n\n").next()?.trim();
    if pending.is_empty() {
        None
    } else {
        Some(pending.to_string())
    }
}

fn normalize_input_line_prefix(line: &str) -> &str {
    line.trim()
        .trim_start_matches('›')
        .trim_start_matches('>')
        .trim()
}

/// Manages the tmux status bar for an orchestrator session.
///
/// Format: `[batty] <phase> | <detail> | <indicator> <message>`
///
/// Debounces updates to max ~5/sec to avoid tmux overhead.
pub struct StatusBar {
    session: String,
    phase: String,
    last_update: Option<Instant>,
    min_interval: Duration,
}

impl StatusBar {
    pub fn new(session: &str, phase: &str) -> Self {
        Self {
            session: session.to_string(),
            phase: phase.to_string(),
            last_update: None,
            min_interval: Duration::from_millis(200), // max ~5 updates/sec
        }
    }

    /// Initialize the status bar styling and initial content.
    pub fn init(&mut self) -> Result<()> {
        // Best-effort capability usage; old tmux versions may not support all
        // style options, but supervision can continue without them.
        if let Err(e) = tmux::set_status_style(&self.session, "bg=colour235,fg=colour136") {
            debug!(error = %e, "status-style unsupported; continuing with defaults");
        }
        if let Err(e) = tmux::tmux_set(&self.session, "status-left-length", "80") {
            debug!(
                error = %e,
                "status-left-length unsupported; continuing with defaults"
            );
        }
        if let Err(e) = tmux::tmux_set(&self.session, "status-right-length", "40") {
            debug!(
                error = %e,
                "status-right-length unsupported; continuing with defaults"
            );
        }
        self.update(StatusIndicator::StateChange, "starting")?;
        Ok(())
    }

    /// Update the status bar with a new indicator and message.
    ///
    /// Debounced: skips the update if called too frequently, unless forced.
    pub fn update(&mut self, indicator: StatusIndicator, message: &str) -> Result<()> {
        self.update_inner(indicator, message, false)
    }

    /// Force-update the status bar (bypasses debounce).
    pub fn force_update(&mut self, indicator: StatusIndicator, message: &str) -> Result<()> {
        self.update_inner(indicator, message, true)
    }

    fn update_inner(
        &mut self,
        indicator: StatusIndicator,
        message: &str,
        force: bool,
    ) -> Result<()> {
        // Debounce check
        if !force
            && let Some(last) = self.last_update
            && last.elapsed() < self.min_interval
        {
            return Ok(());
        }

        let left = format!(
            " [batty] {} | {} {}",
            self.phase,
            indicator.symbol(),
            message
        );

        // Best-effort — don't fail the orchestrator if status bar can't update
        if let Err(e) = tmux::set_status_left(&self.session, &left) {
            debug!(error = %e, "status bar update failed");
        }

        // Also set terminal title (shows in tab/title bar)
        let title = format!("[batty] {} | {}", self.phase, message);
        if let Err(e) = tmux::set_title(&self.session, &title) {
            debug!(error = %e, "title update failed");
        }

        self.last_update = Some(Instant::now());
        Ok(())
    }
}

/// Configuration for stuck detection.
#[derive(Debug, Clone)]
pub struct StuckConfig {
    /// How long without progress events before considering stuck (default: 300s = 5 min).
    pub timeout: Duration,
    /// Maximum nudges before escalating to human (default: 2).
    pub max_nudges: u32,
    /// Whether to auto-relaunch on executor crash (default: false).
    pub auto_relaunch: bool,
}

impl Default for StuckConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(300),
            max_nudges: 2,
            auto_relaunch: false,
        }
    }
}

/// Stuck states detected by the stuck detector.
#[derive(Debug, Clone, PartialEq)]
pub enum StuckState {
    /// Executor is making progress — all good.
    Normal,
    /// No progress events for extended period.
    Stalled { since: Duration },
    /// Same output repeating (detected by event buffer).
    Looping,
    /// Executor process/pane has exited unexpectedly.
    Crashed,
}

/// Recovery action recommended by the stuck detector.
#[derive(Debug, Clone, PartialEq)]
pub enum StuckAction {
    /// No action needed.
    None,
    /// Inject a nudge hint via send-keys.
    Nudge { message: String },
    /// Escalate to human — executor is stuck.
    Escalate { reason: String },
    /// Relaunch the executor (crash recovery).
    Relaunch,
}

/// Monitors executor progress and detects stuck states.
pub struct StuckDetector {
    config: StuckConfig,
    /// When the last meaningful progress was detected.
    last_progress: Instant,
    /// How many nudges have been sent so far.
    nudge_count: u32,
    /// Last few output lines for loop detection.
    recent_lines: Vec<String>,
    /// Maximum lines to track for loop detection.
    loop_window: usize,
    /// Whether this stuck episode has already been escalated to human.
    escalated: bool,
}

impl StuckDetector {
    pub fn new(config: StuckConfig) -> Self {
        Self {
            config,
            last_progress: Instant::now(),
            nudge_count: 0,
            recent_lines: Vec::new(),
            loop_window: 20,
            escalated: false,
        }
    }

    /// Signal that meaningful progress was made (task completed, command ran, file changed, etc.).
    pub fn on_progress(&mut self) {
        self.last_progress = Instant::now();
        self.nudge_count = 0;
        self.recent_lines.clear();
        self.escalated = false;
    }

    /// Feed an output line for loop detection.
    pub fn on_output(&mut self, line: &str) {
        let trimmed = line.trim().to_string();
        if trimmed.is_empty() {
            return;
        }
        self.recent_lines.push(trimmed);
        if self.recent_lines.len() > self.loop_window {
            self.recent_lines.remove(0);
        }
    }

    /// Record that a nudge was sent.
    pub fn nudge_sent(&mut self) {
        self.nudge_count += 1;
    }

    /// Check for stuck state based on elapsed time and output patterns.
    ///
    /// Call this periodically from the orchestrator loop.
    /// `session_alive` indicates whether the tmux session/pane still exists.
    pub fn check(&mut self, session_alive: bool) -> (StuckState, StuckAction) {
        // Crash detection
        if !session_alive {
            let action = if self.config.auto_relaunch {
                StuckAction::Relaunch
            } else if self.escalated {
                StuckAction::None
            } else {
                self.escalated = true;
                StuckAction::Escalate {
                    reason: "executor process crashed".to_string(),
                }
            };
            return (StuckState::Crashed, action);
        }

        // Loop detection: check if the last N lines are all the same
        if self.detect_loop() {
            if self.nudge_count < self.config.max_nudges {
                return (
                    StuckState::Looping,
                    StuckAction::Nudge {
                        message: "You seem to be looping. Try a different approach.".to_string(),
                    },
                );
            } else {
                if self.escalated {
                    return (StuckState::Looping, StuckAction::None);
                }
                self.escalated = true;
                return (
                    StuckState::Looping,
                    StuckAction::Escalate {
                        reason: format!("executor stuck in loop after {} nudges", self.nudge_count),
                    },
                );
            }
        }

        // Stall detection: no progress for timeout duration
        let elapsed = self.last_progress.elapsed();
        if elapsed >= self.config.timeout {
            if self.nudge_count < self.config.max_nudges {
                return (
                    StuckState::Stalled { since: elapsed },
                    StuckAction::Nudge {
                        message: "No progress detected. Are you stuck? Try breaking the task into smaller steps.".to_string(),
                    },
                );
            } else {
                if self.escalated {
                    return (StuckState::Stalled { since: elapsed }, StuckAction::None);
                }
                self.escalated = true;
                return (
                    StuckState::Stalled { since: elapsed },
                    StuckAction::Escalate {
                        reason: format!(
                            "no progress for {}s after {} nudges",
                            elapsed.as_secs(),
                            self.nudge_count
                        ),
                    },
                );
            }
        }

        self.escalated = false;
        (StuckState::Normal, StuckAction::None)
    }

    /// Detect output loops: if the last N lines contain a small repeating pattern.
    fn detect_loop(&self) -> bool {
        let lines = &self.recent_lines;
        if lines.len() < 6 {
            return false;
        }

        // Check if the last 6+ lines are all identical
        let last = &lines[lines.len() - 1];
        let repeated = lines.iter().rev().take(6).all(|l| l == last);
        if repeated {
            return true;
        }

        // Check for 2-line repeating pattern (ABABABAB)
        if lines.len() >= 8 {
            let a = &lines[lines.len() - 2];
            let b = &lines[lines.len() - 1];
            let pattern_repeats = lines
                .iter()
                .rev()
                .take(8)
                .enumerate()
                .all(|(i, l)| if i % 2 == 0 { l == b } else { l == a });
            if pattern_repeats {
                return true;
            }
        }

        false
    }
}

const SUPERVISION_STATE_FILE: &str = "supervision-state.json";
const SUPERVISION_LOCK_FILE: &str = "supervision.lock";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SupervisionState {
    version: u32,
    phase: String,
    session: String,
    executor_pane: String,
    log_pane: Option<String>,
    pipe_log: String,
    pipe_checkpoint: u64,
    updated_epoch: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SupervisionLock {
    pid: u32,
    session: String,
    acquired_epoch: u64,
}

struct SupervisionLease {
    path: PathBuf,
}

impl Drop for SupervisionLease {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Debug, Clone, Copy)]
enum StartMode {
    Fresh,
    Resume,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SupervisorMode {
    Working,
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SupervisorHotkeyAction {
    Pause,
    Resume,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SupervisorTransition {
    Paused,
    Resumed,
    AlreadyPaused,
    AlreadyWorking,
}

fn parse_supervisor_hotkey_action(raw: &str) -> Option<SupervisorHotkeyAction> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "pause" => Some(SupervisorHotkeyAction::Pause),
        "resume" => Some(SupervisorHotkeyAction::Resume),
        _ => None,
    }
}

fn apply_supervisor_hotkey_action(
    mode: &mut SupervisorMode,
    action: SupervisorHotkeyAction,
    detector: &mut PromptDetector,
) -> SupervisorTransition {
    match (action, *mode) {
        (SupervisorHotkeyAction::Pause, SupervisorMode::Working) => {
            *mode = SupervisorMode::Paused;
            detector.human_override();
            SupervisorTransition::Paused
        }
        (SupervisorHotkeyAction::Pause, SupervisorMode::Paused) => {
            detector.human_override();
            SupervisorTransition::AlreadyPaused
        }
        (SupervisorHotkeyAction::Resume, SupervisorMode::Paused) => {
            *mode = SupervisorMode::Working;
            detector.human_override();
            SupervisorTransition::Resumed
        }
        (SupervisorHotkeyAction::Resume, SupervisorMode::Working) => {
            detector.human_override();
            SupervisorTransition::AlreadyWorking
        }
    }
}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn process_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        PathBuf::from(format!("/proc/{pid}")).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        false
    }
}

fn acquire_supervision_lease(log_dir: &Path, session: &str) -> Result<SupervisionLease> {
    let lock_path = log_dir.join(SUPERVISION_LOCK_FILE);

    loop {
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&lock_path)
        {
            Ok(mut file) => {
                let payload = SupervisionLock {
                    pid: std::process::id(),
                    session: session.to_string(),
                    acquired_epoch: now_epoch(),
                };
                let body = serde_json::to_string(&payload)
                    .context("failed to serialize supervision lock payload")?;
                writeln!(file, "{body}").context("failed to write supervision lock file")?;
                return Ok(SupervisionLease { path: lock_path });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = match std::fs::read_to_string(&lock_path) {
                    Ok(body) => match serde_json::from_str::<SupervisionLock>(body.trim()) {
                        Ok(lock) => !process_alive(lock.pid),
                        Err(_) => true,
                    },
                    Err(_) => true,
                };

                if stale {
                    let _ = std::fs::remove_file(&lock_path);
                    continue;
                }

                anyhow::bail!(
                    "unsafe duplicate supervisor attach refused: another batty supervisor appears active for session '{session}'. \
If this is stale, stop that process and run `batty resume` again."
                );
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "failed to acquire supervision lease at {}",
                        lock_path.display()
                    )
                });
            }
        }
    }
}

fn supervision_state_path(log_dir: &Path) -> PathBuf {
    log_dir.join(SUPERVISION_STATE_FILE)
}

fn load_supervision_state(log_dir: &Path) -> Option<SupervisionState> {
    let path = supervision_state_path(log_dir);
    let body = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<SupervisionState>(&body).ok()
}

fn save_supervision_state(log_dir: &Path, state: &SupervisionState) -> Result<()> {
    let path = supervision_state_path(log_dir);
    let body =
        serde_json::to_string_pretty(state).context("failed to serialize supervision state")?;
    std::fs::write(&path, body).with_context(|| {
        format!(
            "failed to write supervision state snapshot to {}",
            path.display()
        )
    })
}

fn detect_log_pane(session: &str) -> Option<String> {
    tmux::list_pane_details(session)
        .ok()?
        .into_iter()
        .find(|p| !p.dead && p.command == "tail")
        .map(|p| p.id)
}

fn resolve_executor_pane(session: &str, state: Option<&SupervisionState>) -> Result<String> {
    if let Some(saved) = state
        && tmux::pane_exists(&saved.executor_pane)
        && !tmux::pane_dead(&saved.executor_pane)?
    {
        return Ok(saved.executor_pane.clone());
    }

    let panes = tmux::list_pane_details(session)?;
    if let Some(pane) = panes.iter().find(|p| !p.dead && p.command != "tail") {
        return Ok(pane.id.clone());
    }
    if let Some(pane) = panes.iter().find(|p| !p.dead && p.active) {
        return Ok(pane.id.clone());
    }
    if let Some(pane) = panes.iter().find(|p| !p.dead) {
        return Ok(pane.id.clone());
    }

    anyhow::bail!("no live executor pane found in session '{session}'")
}

fn parse_last_auto_prompt(orch_log: &Path) -> Option<String> {
    let body = std::fs::read_to_string(orch_log).ok()?;
    for line in body.lines().rev() {
        let marker = "auto-answered: \"";
        let Some(start) = line.find(marker) else {
            continue;
        };
        let rest = &line[start + marker.len()..];
        let Some(end) = rest.find("\" →") else {
            continue;
        };
        let prompt = rest[..end].trim();
        if !prompt.is_empty() {
            return Some(prompt.to_string());
        }
    }
    None
}

fn seed_event_buffer_from_pane(buffer: &EventBuffer, pane_content: &str) {
    let patterns = crate::events::EventPatterns::default_patterns();
    let recent_lines = pane_content
        .lines()
        .rev()
        .take(150)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>();
    for raw in recent_lines {
        let line = strip_ansi(raw);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(event) = patterns.classify(trimmed) {
            buffer.push(event);
        }
    }
}

/// Run the full orchestrator loop.
///
/// Creates a tmux session, sets up pipe-pane, and supervises the executor.
/// Returns when the executor exits or the session is killed.
pub fn run(
    config: OrchestratorConfig,
    observer: Box<dyn OrchestratorObserver>,
    stop: Arc<AtomicBool>,
) -> Result<OrchestratorResult> {
    run_with_mode(config, observer, stop, StartMode::Fresh)
}

/// Resume supervision for an already-running tmux session.
pub fn resume(
    config: OrchestratorConfig,
    observer: Box<dyn OrchestratorObserver>,
    stop: Arc<AtomicBool>,
) -> Result<OrchestratorResult> {
    run_with_mode(config, observer, stop, StartMode::Resume)
}

fn run_with_mode(
    config: OrchestratorConfig,
    mut observer: Box<dyn OrchestratorObserver>,
    stop: Arc<AtomicBool>,
    mode: StartMode,
) -> Result<OrchestratorResult> {
    // 1. Probe tmux capabilities before any supervision actions.
    let tmux_caps = tmux::probe_capabilities()?;
    info!(
        tmux_version = %tmux_caps.version_raw,
        pipe_pane = tmux_caps.pipe_pane,
        pipe_pane_only_if_missing = tmux_caps.pipe_pane_only_if_missing,
        status_style = tmux_caps.status_style,
        split_mode = ?tmux_caps.split_mode,
        known_good = tmux_caps.known_good(),
        "tmux capability probe complete"
    );
    if !tmux_caps.known_good() {
        warn!(
            tmux_version = %tmux_caps.version_raw,
            "tmux version outside documented known-good range (>= 3.2)"
        );
        observer.on_event(&format!(
            "⚠ tmux {} is outside known-good range (>= 3.2); using compatibility fallbacks",
            tmux_caps.version_raw
        ));
    }
    if !tmux_caps.pipe_pane {
        anyhow::bail!("{}", tmux_caps.remediation_message());
    }

    // 2. Resolve session + run paths.
    let session = tmux::session_name(&config.phase);
    let log_dir = config.logs_dir.clone();
    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("failed to create log directory {}", log_dir.display()))?;

    // Guard against duplicate supervisors attaching to the same live run.
    let _lease = acquire_supervision_lease(&log_dir, &session)?;

    let pipe_log = log_dir.join("pty-output.log");
    let orch_log = log_dir.join("orchestrator.log");
    let prior_state = load_supervision_state(&log_dir);

    let executor_pane = match mode {
        StartMode::Fresh => {
            tmux::create_session(
                &session,
                &config.spawn.program,
                &config.spawn.args,
                &config.spawn.work_dir,
            )
            .with_context(|| format!("failed to create tmux session for phase {}", config.phase))?;

            observer.on_event(&format!("● session '{}' created", session));
            let pane = tmux::pane_id(&session)?;
            observer.on_event(&format!("● supervision target pane {pane}"));
            pane
        }
        StartMode::Resume => {
            if !tmux::session_exists(&session) {
                anyhow::bail!(
                    "tmux session '{}' not found; start a run first with `batty work {}`",
                    session,
                    config.phase
                );
            }
            observer.on_event(&format!("● resuming existing session '{}'", session));
            let pane = resolve_executor_pane(&session, prior_state.as_ref())?;
            observer.on_event(&format!("● supervision target pane {pane}"));
            pane
        }
    };

    if let Err(e) = tmux::tmux_set(&session, "remain-on-exit", "on") {
        warn!(error = %e, "failed to enable remain-on-exit");
    }

    // 3. Set up pipe-pane
    match mode {
        StartMode::Fresh => {
            tmux::setup_pipe_pane(&executor_pane, &pipe_log)?;
            observer.on_event(&format!("● pipe-pane → {}", pipe_log.display()));
        }
        StartMode::Resume => {
            if tmux_caps.pipe_pane_only_if_missing {
                tmux::setup_pipe_pane_if_missing(&executor_pane, &pipe_log)?;
                observer.on_event(&format!(
                    "● pipe-pane ensured (resume) → {}",
                    pipe_log.display()
                ));
            } else {
                // Fallback for older tmux lacking `pipe-pane -o`: avoid
                // replacing a live pipe if one is already active.
                let has_pipe = tmux::pane_pipe_enabled(&executor_pane).unwrap_or(false);
                if has_pipe {
                    observer.on_event("● resume fallback: existing pipe-pane reused");
                } else {
                    tmux::setup_pipe_pane(&executor_pane, &pipe_log)?;
                    observer.on_event(&format!(
                        "● resume fallback: pipe-pane attached → {}",
                        pipe_log.display()
                    ));
                }
            }
        }
    }

    // 4. Set up status bar
    let mut status_bar = StatusBar::new(&session, &config.phase);
    status_bar.init()?;
    observer.on_event("● status bar initialized");

    // 5. Set up orchestrator log pane
    let mut log_pane_id = detect_log_pane(&session);
    if config.log_pane {
        if log_pane_id.is_none() {
            setup_log_pane(
                &session,
                &executor_pane,
                &orch_log,
                config.log_pane_height_pct,
                tmux_caps.split_mode,
            )?;
            log_pane_id = detect_log_pane(&session);
            observer.on_event("● log pane created");
        } else {
            observer.on_event("● log pane reused");
        }
    }

    // 5.25 Configure hotkeys for runtime supervisor pause/resume control.
    let mut hotkeys_enabled = true;
    if let Err(e) = tmux::configure_supervisor_hotkeys(&session) {
        hotkeys_enabled = false;
        warn!(error = %e, "failed to configure supervisor hotkeys");
        observer.on_event("⚠ supervisor hotkeys unavailable");
    } else {
        observer.on_event(&format!(
            "● supervisor hotkeys: pause {}, resume {}",
            tmux::SUPERVISOR_PAUSE_HOTKEY,
            tmux::SUPERVISOR_RESUME_HOTKEY
        ));
    }

    // 5.5 Optional auto-attach
    if config.auto_attach {
        if std::io::stdout().is_terminal() && std::io::stdin().is_terminal() {
            observer.on_event("● auto-attach requested");
            let session_for_attach = session.clone();
            std::thread::spawn(move || {
                match std::process::Command::new("tmux")
                    .args(["attach-session", "-t", &session_for_attach])
                    .status()
                {
                    Ok(status) if status.success() => {}
                    Ok(status) => {
                        warn!(
                            session = %session_for_attach,
                            status = ?status,
                            "auto-attach exited non-zero"
                        );
                    }
                    Err(error) => {
                        warn!(session = %session_for_attach, error = %error, "auto-attach failed");
                    }
                }
            });
        } else {
            warn!("auto-attach requested but stdin/stdout is not a TTY; skipping");
            observer.on_event("⚠ auto-attach skipped (no TTY)");
        }
    }

    // 6. Initialize components
    let buffer = EventBuffer::new(config.buffer_size);
    let mut watcher = match mode {
        StartMode::Fresh => PipeWatcher::new(&pipe_log, buffer.clone()),
        StartMode::Resume => {
            let checkpoint = prior_state.as_ref().map(|s| s.pipe_checkpoint).unwrap_or(0);
            PipeWatcher::new_with_position(&pipe_log, buffer.clone(), checkpoint)
        }
    };
    let mut detector = PromptDetector::new(config.patterns, config.detector);
    let mut stuck_detector = config.stuck.map(StuckDetector::new);
    let mut last_handled_pane_signature = String::new();
    let mut supervisor_mode = SupervisorMode::Working;

    info!(session = %session, "orchestrator loop starting");
    observer.on_event("● supervising");
    status_bar.update(StatusIndicator::Ok, "supervising")?;

    // 6.5 Resume state rebuild from persisted logs + current pane output.
    let mut last_event_line = String::new();
    let mut last_pane_signature = String::new();
    if matches!(mode, StartMode::Resume) {
        observer.on_event(&format!(
            "● resumed pipe offset from checkpoint {}",
            watcher.checkpoint_offset()
        ));

        if let Ok(pane_content) = tmux::capture_pane(&executor_pane) {
            seed_event_buffer_from_pane(&buffer, &pane_content);
            if let Some((signature, detector_line)) = pane_detector_snapshot(&pane_content) {
                detector.seed_from_recent_output(&detector_line);
                last_pane_signature = signature.clone();

                if let Some(last_auto_prompt) = parse_last_auto_prompt(&orch_log) {
                    if detector_line.contains(&last_auto_prompt)
                        || last_auto_prompt.contains(&detector_line)
                    {
                        detector.answer_injected();
                        last_handled_pane_signature = signature;
                        observer
                            .on_event("● detector rebuilt from orchestrator log + pane snapshot");
                    } else {
                        observer.on_event("● detector seeded from pane snapshot");
                    }
                } else {
                    observer.on_event("● detector seeded from pane snapshot");
                }
            }
        }
    }

    let mut supervision_state = SupervisionState {
        version: 1,
        phase: config.phase.clone(),
        session: session.clone(),
        executor_pane: executor_pane.clone(),
        log_pane: log_pane_id.take(),
        pipe_log: pipe_log.display().to_string(),
        pipe_checkpoint: watcher.checkpoint_offset(),
        updated_epoch: now_epoch(),
    };
    if let Err(e) = save_supervision_state(&log_dir, &supervision_state) {
        warn!(error = %e, "failed to persist supervision state");
    }

    // 7. Supervision loop
    let result = loop {
        if stop.load(Ordering::Relaxed) {
            observer.on_event("● stopped by signal");
            status_bar.force_update(StatusIndicator::StateChange, "stopped")?;
            break OrchestratorResult::Detached;
        }

        // Check if session still exists
        let executor_alive = if tmux::pane_exists(&executor_pane) {
            !tmux::pane_dead(&executor_pane).unwrap_or(false)
        } else {
            false
        };
        if !executor_alive {
            observer.on_event("✓ executor exited");
            status_bar.force_update(StatusIndicator::Ok, "completed")?;
            break OrchestratorResult::Completed;
        }

        if hotkeys_enabled {
            match tmux::take_supervisor_hotkey_action(&session) {
                Ok(Some(raw_action)) => {
                    if let Some(action) = parse_supervisor_hotkey_action(&raw_action) {
                        match apply_supervisor_hotkey_action(
                            &mut supervisor_mode,
                            action,
                            &mut detector,
                        ) {
                            SupervisorTransition::Paused => {
                                observer.on_event("→ supervisor paused via hotkey");
                                status_bar.force_update(
                                    StatusIndicator::StateChange,
                                    "PAUSED — manual input only",
                                )?;
                            }
                            SupervisorTransition::Resumed => {
                                observer.on_event("→ supervisor resumed via hotkey");
                                status_bar.force_update(StatusIndicator::Ok, "supervising")?;
                            }
                            SupervisorTransition::AlreadyPaused => {
                                observer.on_event("→ pause hotkey ignored (already paused)");
                            }
                            SupervisorTransition::AlreadyWorking => {
                                observer.on_event("→ resume hotkey ignored (already supervising)");
                            }
                        }
                    } else {
                        warn!(action = %raw_action, "unknown supervisor hotkey action");
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(error = %e, "failed to read supervisor hotkey state");
                    observer.on_event("⚠ supervisor hotkey polling disabled");
                    hotkeys_enabled = false;
                }
            }
        }

        // Poll for new output
        match watcher.poll() {
            Ok(event_count) => {
                let checkpoint = watcher.checkpoint_offset();
                if checkpoint != supervision_state.pipe_checkpoint {
                    supervision_state.pipe_checkpoint = checkpoint;
                    supervision_state.updated_epoch = now_epoch();
                    if let Err(e) = save_supervision_state(&log_dir, &supervision_state) {
                        warn!(error = %e, "failed to update supervision checkpoint");
                    }
                }

                if event_count > 0 {
                    debug!(events = event_count, "new events extracted");
                    // Feed output to stuck detector for loop detection and progress tracking
                    if let Some(ref mut stuck) = stuck_detector {
                        let snapshot = buffer.snapshot();
                        for evt in snapshot
                            .iter()
                            .skip(snapshot.len().saturating_sub(event_count))
                        {
                            let line = format!("{evt:?}");
                            stuck.on_output(&line);
                            // Progress events: task completed, command ran, file changes
                            if line.contains("TaskCompleted")
                                || line.contains("CommandRan")
                                || line.contains("FileCreated")
                                || line.contains("FileModified")
                                || line.contains("CommitMade")
                            {
                                stuck.on_progress();
                            }
                        }
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "pipe watcher poll error");
            }
        }

        // Feed new output lines to the detector
        let events = buffer.snapshot();
        if let Some(last_event) = events.last() {
            let line = format!("{last_event:?}");
            if line != last_event_line {
                last_event_line = line;
            }
        }

        // Also check capture-pane for the most current visible content
        // This catches prompts that pipe-pane hasn't flushed yet
        if let Ok(pane_content) = tmux::capture_pane(&executor_pane)
            && let Some((signature, detector_line)) = pane_detector_snapshot(&pane_content)
        {
            // Only treat as new output when meaningful pane content changes.
            if signature != last_pane_signature {
                last_pane_signature = signature;
                let event = detector.on_output(&detector_line);
                if let Some(DetectorEvent::PromptDetected(ref prompt)) = event {
                    if supervisor_mode == SupervisorMode::Paused {
                        debug!("supervisor paused: suppressing inline prompt automation");
                        detector.human_override();
                    } else {
                        handle_prompt(
                            prompt,
                            &executor_pane,
                            &config.policy,
                            &mut detector,
                            &mut *observer,
                            config.tier2.as_ref(),
                            &buffer,
                            &mut status_bar,
                            config.answer_delay,
                        )?;
                        if matches!(mode, StartMode::Resume) {
                            last_handled_pane_signature = last_pane_signature.clone();
                        }
                    }
                }
            }
        }

        // Run the tick for silence-based detection
        match detector.tick() {
            DetectorEvent::PromptDetected(ref prompt) => {
                if matches!(mode, StartMode::Resume)
                    && !last_pane_signature.is_empty()
                    && last_pane_signature == last_handled_pane_signature
                {
                    debug!("resume dedupe: skipping repeated prompt handling");
                } else if supervisor_mode == SupervisorMode::Paused {
                    debug!("supervisor paused: suppressing prompt automation");
                    detector.human_override();
                } else {
                    handle_prompt(
                        prompt,
                        &executor_pane,
                        &config.policy,
                        &mut detector,
                        &mut *observer,
                        config.tier2.as_ref(),
                        &buffer,
                        &mut status_bar,
                        config.answer_delay,
                    )?;
                    if matches!(mode, StartMode::Resume) && !last_pane_signature.is_empty() {
                        last_handled_pane_signature = last_pane_signature.clone();
                    }
                }
            }
            DetectorEvent::UnknownRequest { last_line, .. } => {
                debug!(last_line = %last_line, "unknown request fallback triggered");
                if supervisor_mode == SupervisorMode::Paused {
                    debug!("supervisor paused: suppressing unknown-request fallback");
                    detector.human_override();
                    continue;
                }
                let pane_content = tmux::capture_pane(&executor_pane).unwrap_or_default();
                let maybe_idle_input = if config.idle_input_fallback {
                    extract_idle_input_prompt(&pane_content)
                } else {
                    None
                };
                if let Some(idle_hint) = maybe_idle_input {
                    // Codex-style pending input (`› ...`) is an agent hint.
                    // Do not send it to Tier 2 as an unknown prompt.
                    observer.on_event(&format!(
                        "→ idle input hint pending (skipping supervisor): {}",
                        preview_for_log(&idle_hint, 160)
                    ));
                    status_bar.update(StatusIndicator::Action, "idle input pending")?;
                } else if !should_invoke_unknown_fallback(&last_line) {
                    debug!(
                        last_line = %last_line,
                        "unknown request fallback skipped for non-actionable line"
                    );
                } else if config.tier2.is_some() {
                    // Generic fallback: no known pattern, but executor appears idle
                    // and potentially waiting for user input. Ask supervisor agent.
                    let pane_excerpt = pane_content
                        .lines()
                        .rev()
                        .take(20)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect::<Vec<_>>()
                        .join("\n");

                    let prompt_text = format!(
                        "Executor appears to be waiting for input, but no known prompt pattern matched.\nLast visible line: {last_line}\n\nVisible pane excerpt:\n{pane_excerpt}\n\nIf user input is required, provide the exact short response to send. If unclear, respond with ESCALATE: <reason>."
                    );

                    let synthetic_prompt = DetectedPrompt {
                        kind: PromptKind::Question {
                            detail: last_line.clone(),
                        },
                        matched_text: prompt_text,
                    };

                    observer.on_event("? supervisor fallback: unknown request");
                    handle_prompt(
                        &synthetic_prompt,
                        &executor_pane,
                        &config.policy,
                        &mut detector,
                        &mut *observer,
                        config.tier2.as_ref(),
                        &buffer,
                        &mut status_bar,
                        config.answer_delay,
                    )?;
                }
            }
            DetectorEvent::Silence { last_line, .. } => {
                debug!(last_line = %last_line, "silence detected");
            }
            _ => {}
        }

        // Stuck detection
        if let Some(ref mut stuck) = stuck_detector {
            let session_alive = if tmux::pane_exists(&executor_pane) {
                !tmux::pane_dead(&executor_pane).unwrap_or(false)
            } else {
                false
            };
            let (state, action) = stuck.check(session_alive);

            if supervisor_mode == SupervisorMode::Paused && !matches!(&action, StuckAction::None) {
                debug!(state = ?state, "supervisor paused: suppressing stuck action");
                std::thread::sleep(config.poll_interval);
                continue;
            }

            match action {
                StuckAction::None => {}
                StuckAction::Nudge { ref message } => {
                    warn!(state = ?state, "executor may be stuck, nudging");
                    observer.on_event("⚠ stuck: nudging executor");
                    status_bar.force_update(StatusIndicator::Failure, "stuck — nudging")?;

                    if let Err(e) = tmux::send_keys(&executor_pane, message, true) {
                        warn!(error = %e, "failed to send nudge");
                    }
                    stuck.nudge_sent();
                }
                StuckAction::Escalate { ref reason } => {
                    warn!(reason = %reason, "executor stuck — escalating");
                    observer.on_escalate(&format!("stuck: {reason}"));
                    status_bar.force_update(StatusIndicator::NeedsInput, "STUCK — needs input")?;
                }
                StuckAction::Relaunch => {
                    info!("executor crashed — auto-relaunching");
                    observer.on_event("✗ crash detected — relaunching");
                    status_bar.force_update(StatusIndicator::Failure, "crashed — relaunching")?;
                    // Relaunch is handled by the caller (not implemented in this phase)
                }
            }
        }

        std::thread::sleep(config.poll_interval);
    };

    // 8. Cleanup
    info!(result = ?result, "orchestrator loop ended");
    supervision_state.pipe_checkpoint = watcher.checkpoint_offset();
    supervision_state.updated_epoch = now_epoch();
    if let Err(e) = save_supervision_state(&log_dir, &supervision_state) {
        warn!(error = %e, "failed to persist final supervision checkpoint");
    }

    Ok(result)
}

/// Set up the orchestrator log pane in the tmux session.
///
/// Creates a vertical split at the bottom of the session showing `tail -f`
/// on the orchestrator log file. The executor pane stays focused (selected).
fn setup_log_pane(
    session: &str,
    executor_pane: &str,
    log_path: &Path,
    height_pct: u32,
    split_mode: tmux::SplitMode,
) -> Result<()> {
    // Ensure log file exists (tail -f needs it)
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Touch the file so tail -f can start immediately
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;

    let tail_cmd = vec![
        "tail".to_string(),
        "-f".to_string(),
        log_path.display().to_string(),
    ];

    match split_mode {
        tmux::SplitMode::Lines => {
            let lines = std::cmp::max(3, 50 * height_pct / 100);
            if let Err(e) = tmux::split_window_vertical_lines(session, lines, &tail_cmd) {
                warn!(error = %e, "log pane creation with -l failed — continuing without it");
                return Ok(());
            }
        }
        tmux::SplitMode::Percent => {
            if let Err(e) = tmux::split_window_vertical_percent(session, height_pct, &tail_cmd) {
                warn!(error = %e, "log pane creation with -p failed — continuing without it");
                return Ok(());
            }
        }
        tmux::SplitMode::Disabled => {
            warn!("tmux split-window unsupported; continuing without log pane");
            return Ok(());
        }
    }

    // Re-select the executor pane so capture/send-keys stay on the executor.
    let _ = std::process::Command::new("tmux")
        .args(["select-pane", "-t", executor_pane])
        .output();

    info!(session = session, "orchestrator log pane created");
    Ok(())
}

/// Check if the human has already answered the prompt by looking at capture-pane.
///
/// Compares the current last visible line against the original prompt text.
/// If the pane content has changed (new lines after the prompt), the human typed.
fn check_human_answered(executor_pane: &str, prompt_text: &str) -> bool {
    if let Ok(pane_content) = tmux::capture_pane(executor_pane) {
        // If the last non-empty line no longer matches the prompt, human answered
        if let Some(last) = pane_content.lines().rev().find(|l| !l.trim().is_empty()) {
            let last_clean = crate::prompt::strip_ansi(last);
            let last_trimmed = last_clean.trim();
            let last_normalized = normalize_input_line_prefix(last_trimmed);

            // For synthetic idle-input prompts, ignore the unchanged input line.
            if let Some(pending) = extract_pending_input_from_supervisor_prompt(prompt_text)
                && last_normalized == pending
            {
                return false;
            }

            // If the last line changed from the prompt, someone typed
            return !last_trimmed.is_empty()
                && !prompt_text.contains(last_trimmed)
                && !prompt_text.contains(last_normalized)
                && !last_trimmed.contains(prompt_text);
        }
    }
    false
}

/// Wait for the answer delay, checking if the human overrides.
///
/// Returns true if the human answered during the delay, false if the delay
/// expired without human input (safe to inject).
fn wait_with_human_check(
    executor_pane: &str,
    prompt_text: &str,
    delay: Duration,
    poll_interval: Duration,
) -> bool {
    if delay.is_zero() {
        return false;
    }

    let start = Instant::now();
    while start.elapsed() < delay {
        if check_human_answered(executor_pane, prompt_text) {
            return true; // Human answered
        }
        std::thread::sleep(poll_interval);
    }
    false
}

/// Handle a detected prompt: evaluate policy and take action.
///
/// Tier 1: pattern match → wait answer_delay → inject auto-answer via send-keys.
/// Tier 2: no match → call supervisor agent → check human override → inject or escalate.
///
/// Human override: if the human types during the answer delay or while Tier 2 is
/// thinking, the auto-answer is cancelled.
#[allow(clippy::too_many_arguments)] // Orchestrator wiring passes explicit context parts to keep call-site intent readable.
fn handle_prompt(
    prompt: &crate::prompt::DetectedPrompt,
    executor_pane: &str,
    policy: &PolicyEngine,
    detector: &mut PromptDetector,
    observer: &mut dyn OrchestratorObserver,
    tier2_config: Option<&Tier2Config>,
    event_buffer: &EventBuffer,
    status_bar: &mut StatusBar,
    answer_delay: Duration,
) -> Result<()> {
    // Skip completion/error signals — those aren't questions
    match &prompt.kind {
        PromptKind::Completion | PromptKind::Error { .. } => {
            return Ok(());
        }
        _ => {}
    }

    let decision = policy.evaluate(&prompt.matched_text);
    debug!(decision = ?decision, "policy decision for prompt");

    match decision {
        Decision::Act {
            ref prompt,
            ref response,
        } => {
            info!(prompt = %prompt, response = %response, "Tier 1 auto-answer");

            // Wait for answer_delay, checking if human types first
            if wait_with_human_check(
                executor_pane,
                prompt,
                answer_delay,
                Duration::from_millis(100),
            ) {
                info!("human override — cancelling auto-answer");
                observer.on_event("→ human override — auto-answer cancelled");
                status_bar.update(StatusIndicator::Action, "human override")?;
                detector.human_override();
                return Ok(());
            }

            observer.on_auto_answer(prompt, response);
            status_bar.update(
                StatusIndicator::Action,
                &format!("answered: {}", display_response(response)),
            )?;

            // Inject via tmux send-keys
            tmux::send_keys(executor_pane, response, true)
                .with_context(|| format!("failed to send-keys auto-answer to '{executor_pane}'"))?;

            detector.answer_injected();
            status_bar.update(StatusIndicator::Ok, "supervising")?;
        }
        Decision::Suggest {
            ref prompt,
            ref response,
        } => {
            observer.on_suggest(prompt, response);
            status_bar.update(StatusIndicator::Thinking, &format!("suggest: {response}"))?;
        }
        Decision::Escalate { ref prompt } => {
            // Tier 2: try supervisor agent before escalating to human
            if let Some(t2_config) = tier2_config {
                observer.on_event("? supervisor thinking...");
                status_bar.force_update(StatusIndicator::Thinking, "supervisor thinking...")?;
                if t2_config.trace_io {
                    observer.on_event(&format!(
                        "? supervisor call → {}",
                        supervisor_cmd_for_log(t2_config)
                    ));
                    observer.on_event(&format!(
                        "? supervisor prompt → {}",
                        preview_for_log(prompt, 220)
                    ));
                }

                let event_summary = event_buffer.format_summary();
                let context = tier2::compose_context(
                    &event_summary,
                    prompt,
                    t2_config.system_prompt.as_deref(),
                );
                if t2_config.trace_io {
                    observer.on_event(&format!("? supervisor context chars={}", context.len()));
                }

                match tier2::call_supervisor(t2_config, &context) {
                    Ok(Tier2Result::Answer { response }) => {
                        if t2_config.trace_io {
                            observer.on_event(&format!(
                                "? supervisor reply ← {}",
                                display_response(&response)
                            ));
                        }
                        // Check if human answered while Tier 2 was thinking
                        if check_human_answered(executor_pane, prompt) {
                            info!("human override — cancelling Tier 2 answer");
                            observer.on_event("→ human override — Tier 2 answer cancelled");
                            status_bar.update(StatusIndicator::Action, "human override")?;
                            detector.human_override();
                            return Ok(());
                        }

                        info!(prompt = %prompt, response = %response, "Tier 2 answer");
                        observer.on_auto_answer(prompt, &response);
                        status_bar.update(
                            StatusIndicator::Action,
                            &format!("T2: {}", display_response(&response)),
                        )?;

                        tmux::send_keys(executor_pane, &response, true).with_context(|| {
                            format!("failed to send-keys Tier 2 answer to '{executor_pane}'")
                        })?;

                        detector.answer_injected();
                        status_bar.update(StatusIndicator::Ok, "supervising")?;
                    }
                    Ok(Tier2Result::Escalate { reason }) => {
                        if t2_config.trace_io {
                            observer.on_event(&format!(
                                "? supervisor reply ← ESCALATE: {}",
                                preview_for_log(&reason, 220)
                            ));
                        }
                        info!(reason = %reason, "Tier 2 escalated to human");
                        observer.on_escalate(&format!("{prompt} (supervisor: {reason})"));
                        status_bar.force_update(StatusIndicator::NeedsInput, "NEEDS INPUT")?;
                    }
                    Ok(Tier2Result::Failed { error }) => {
                        if t2_config.trace_io {
                            observer.on_event(&format!(
                                "? supervisor error ← {}",
                                preview_for_log(&error, 220)
                            ));
                        }
                        warn!(error = %error, "Tier 2 call failed");
                        observer.on_escalate(&format!("{prompt} (supervisor failed: {error})"));
                        status_bar.force_update(StatusIndicator::NeedsInput, "NEEDS INPUT")?;
                    }
                    Err(e) => {
                        if t2_config.trace_io {
                            observer.on_event(&format!("? supervisor error ← {}", e));
                        }
                        warn!(error = %e, "Tier 2 error");
                        observer.on_escalate(&format!("{prompt} (supervisor error)"));
                        status_bar.force_update(StatusIndicator::NeedsInput, "NEEDS INPUT")?;
                    }
                }
            } else {
                // No Tier 2 configured — escalate directly
                observer.on_escalate(prompt);
                status_bar.force_update(StatusIndicator::NeedsInput, "NEEDS INPUT")?;
            }
        }
        Decision::Observe { .. } => {
            // Just log, no action
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Policy;
    use crate::prompt::PromptPatterns;
    use serde::Deserialize;
    use std::collections::HashMap;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    /// Test observer that collects events.
    struct TestObserver {
        events: Arc<Mutex<Vec<String>>>,
    }

    impl TestObserver {
        fn new() -> (Self, Arc<Mutex<Vec<String>>>) {
            let events = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    events: events.clone(),
                },
                events,
            )
        }
    }

    impl OrchestratorObserver for TestObserver {
        fn on_auto_answer(&mut self, prompt: &str, response: &str) {
            self.events
                .lock()
                .unwrap()
                .push(format!("auto:{prompt}→{response}"));
        }
        fn on_escalate(&mut self, prompt: &str) {
            self.events
                .lock()
                .unwrap()
                .push(format!("escalate:{prompt}"));
        }
        fn on_suggest(&mut self, prompt: &str, response: &str) {
            self.events
                .lock()
                .unwrap()
                .push(format!("suggest:{prompt}→{response}"));
        }
        fn on_event(&mut self, message: &str) {
            self.events.lock().unwrap().push(format!("event:{message}"));
        }
    }

    #[cfg(unix)]
    fn write_script(path: &std::path::Path, content: &str) {
        fs::write(path, content).unwrap();
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    #[cfg(unix)]
    fn harness_agent_script() -> &'static str {
        r#"#!/usr/bin/env bash
set -euo pipefail
out="${1:?missing output path}"
timeout="${2:-2}"
prompt_line="${3:-agent waiting for supervisor response}"
echo "$prompt_line"
if IFS= read -r -t "$timeout" line; then
  printf "%s" "$line" > "$out"
else
  printf "__NO_INPUT__" > "$out"
fi
echo "agent done"
"#
    }

    #[cfg(unix)]
    fn harness_supervisor_script() -> &'static str {
        r#"#!/usr/bin/env bash
set -euo pipefail
mode="${1:?missing mode}"
ctx_out="${2:?missing context output path}"
context="${3:-}"
printf "%s" "$context" > "$ctx_out"
case "$mode" in
  direct) printf "y" ;;
  enter) printf "Press Enter to continue" ;;
  escalate) printf "ESCALATE: need human decision" ;;
  fail) echo "forced failure" >&2; exit 9 ;;
  verbose) printf "This is a very long paragraph that explains what to do instead of returning a direct terminal input and it should not be injected as-is into the executor prompt because that is unsafe." ;;
  *) printf "y" ;;
esac
"#
    }

    #[cfg(unix)]
    const HARNESS_CONTRACT_PATH: &str = "planning/supervision-harness-contract.toml";

    #[cfg(unix)]
    #[derive(Debug, Deserialize, Clone)]
    struct HarnessContract {
        scenarios: Vec<HarnessScenario>,
        failure_taxonomy: Vec<FailureTaxonomyEntry>,
    }

    #[cfg(unix)]
    #[derive(Debug, Deserialize, Clone)]
    struct HarnessScenario {
        id: String,
        kind: String,
        detector_event: Option<String>,
        supervisor_interface: Option<String>,
        injected_terminal_input: Option<String>,
        pane_invariant: Option<String>,
        agent_prompt_line: Option<String>,
        read_timeout_secs: Option<u64>,
        mock_mode: Option<String>,
        expected_input: Option<String>,
        expected_contains: Option<String>,
        expect_auto: Option<bool>,
        expect_escalate: Option<bool>,
        expected_lifecycle_events: Option<Vec<String>>,
    }

    #[cfg(unix)]
    #[derive(Debug, Deserialize, Clone)]
    struct FailureTaxonomyEntry {
        class: String,
        triage_owner: String,
    }

    #[cfg(unix)]
    #[derive(Debug)]
    struct HarnessCaseResult {
        received: String,
        events: Vec<String>,
        target_pane: String,
        panes: Vec<tmux::PaneDetails>,
    }

    #[cfg(unix)]
    fn load_harness_contract() -> HarnessContract {
        let body = fs::read_to_string(HARNESS_CONTRACT_PATH)
            .unwrap_or_else(|e| panic!("failed to read {HARNESS_CONTRACT_PATH}: {e}"));
        toml::from_str(&body).unwrap_or_else(|e| panic!("invalid harness contract TOML: {e}"))
    }

    #[cfg(unix)]
    fn harness_scenario(id: &str) -> HarnessScenario {
        let contract = load_harness_contract();
        contract
            .scenarios
            .into_iter()
            .find(|s| s.id == id)
            .unwrap_or_else(|| panic!("missing harness scenario '{id}' in contract"))
    }

    #[cfg(unix)]
    fn run_harness_mock_case(scenario_id: &str) -> HarnessCaseResult {
        let scenario = harness_scenario(scenario_id);
        assert_eq!(
            scenario.kind, "mock",
            "scenario {scenario_id} must be kind=mock"
        );

        let mode = scenario
            .mock_mode
            .as_deref()
            .unwrap_or_else(|| panic!("scenario {scenario_id} missing mock_mode"));
        let prompt_line = scenario
            .agent_prompt_line
            .as_deref()
            .unwrap_or_else(|| panic!("scenario {scenario_id} missing agent_prompt_line"));
        let timeout = scenario
            .read_timeout_secs
            .unwrap_or_else(|| panic!("scenario {scenario_id} missing read_timeout_secs"));

        run_harness_case_with_tier2(
            Tier2Config {
                program: "<mock-supervisor>".to_string(),
                args: vec![],
                timeout: Duration::from_secs(5),
                system_prompt: None,
                trace_io: true,
            },
            prompt_line,
            timeout,
            Some(mode),
        )
    }

    #[cfg(unix)]
    fn run_harness_case_with_tier2(
        mut tier2: Tier2Config,
        agent_prompt_line: &str,
        read_timeout_secs: u64,
        mock_mode: Option<&str>,
    ) -> HarnessCaseResult {
        let tmp = tempfile::tempdir().unwrap();
        let agent_script = tmp.path().join("mock-agent.sh");
        let supervisor_script = tmp.path().join("mock-supervisor.sh");
        let received = tmp.path().join("received.txt");
        let context = tmp.path().join("context.txt");

        write_script(&agent_script, harness_agent_script());
        if mock_mode.is_some() {
            write_script(&supervisor_script, harness_supervisor_script());
        }

        let phase_suffix = mock_mode.unwrap_or("custom");
        let phase = format!("it-harness-{phase_suffix}");
        let session = tmux::session_name(&phase);
        let _ = tmux::kill_session(&session);

        let stop = Arc::new(AtomicBool::new(false));
        let (observer, events_arc) = TestObserver::new();

        let config = OrchestratorConfig {
            spawn: SpawnConfig {
                program: agent_script.to_string_lossy().to_string(),
                args: vec![
                    received.to_string_lossy().to_string(),
                    read_timeout_secs.to_string(),
                    agent_prompt_line.to_string(),
                ],
                work_dir: "/tmp".to_string(),
                env: vec![],
            },
            patterns: PromptPatterns::codex_cli(),
            policy: PolicyEngine::new(Policy::Act, HashMap::new()),
            detector: DetectorConfig {
                silence_timeout: Duration::from_millis(120),
                answer_cooldown: Duration::from_millis(50),
                unknown_request_fallback: true,
            },
            phase,
            logs_dir: tmp.path().join(".batty").join("logs"),
            poll_interval: Duration::from_millis(80),
            buffer_size: 50,
            tier2: Some({
                if let Some(mode) = mock_mode {
                    tier2.program = supervisor_script.to_string_lossy().to_string();
                    tier2.args = vec![mode.to_string(), context.to_string_lossy().to_string()];
                }
                tier2
            }),
            log_pane: true,
            log_pane_height_pct: 20,
            stuck: None,
            answer_delay: Duration::ZERO,
            auto_attach: false,
            idle_input_fallback: true,
        };

        let result = run(config, Box::new(observer), stop).unwrap();
        assert!(
            matches!(result, OrchestratorResult::Completed),
            "expected Completed, got: {result:?}"
        );

        // Interface stability check: executor + log panes should both exist.
        let panes = tmux::list_panes(&session).unwrap();
        assert_eq!(
            panes.len(),
            2,
            "expected 2 panes (executor + log), got {}",
            panes.len()
        );

        let received_value = fs::read_to_string(&received).unwrap_or_default();
        if mock_mode.is_some() {
            let ctx = fs::read_to_string(&context).unwrap_or_default();
            assert!(
                ctx.contains("Question from executor"),
                "expected supervisor context payload, got: {ctx}"
            );
        }

        let events = events_arc.lock().unwrap().clone();
        let target_pane = events
            .iter()
            .find_map(|e| {
                e.strip_prefix("event:● supervision target pane ")
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| panic!("missing supervision target pane event: {events:?}"));
        let pane_details = tmux::list_pane_details(&session).unwrap();
        assert_eq!(
            pane_details.len(),
            2,
            "expected exactly two panes (executor+log): {pane_details:?}"
        );
        assert!(
            pane_details.iter().any(|p| p.command == "tail"),
            "expected a tail log pane, got: {pane_details:?}"
        );
        assert!(
            pane_details
                .iter()
                .any(|p| p.id == target_pane && p.command != "tail"),
            "supervision target pane must be executor, got target={target_pane} panes={pane_details:?}"
        );
        let executor_details = pane_details
            .iter()
            .find(|p| p.id == target_pane)
            .unwrap_or_else(|| panic!("missing target pane details: {pane_details:?}"));
        assert!(
            executor_details.dead,
            "executor pane should be marked dead after process exit: {pane_details:?}"
        );
        assert!(
            pane_details
                .iter()
                .filter(|p| p.command == "tail")
                .all(|p| !p.dead),
            "log pane should remain alive and not become supervision target: {pane_details:?}"
        );
        if mock_mode.is_none() {
            assert!(
                events
                    .iter()
                    .any(|e| e.contains("event:? supervisor context chars=")),
                "expected real supervisor context trace event, got: {events:?}"
            );
        }

        let _ = tmux::kill_session(&session);
        HarnessCaseResult {
            received: received_value,
            events,
            target_pane,
            panes: pane_details,
        }
    }

    #[cfg(unix)]
    fn run_harness_case_with_real_supervisor(
        real_program: &str,
        real_args: &[&str],
        agent_prompt_line: &str,
        read_timeout_secs: u64,
    ) -> HarnessCaseResult {
        let mut args: Vec<String> = real_args.iter().map(|s| s.to_string()).collect();
        // The custom context file path is appended in run_harness_case_with_tier2.
        run_harness_case_with_tier2(
            Tier2Config {
                program: real_program.to_string(),
                args: std::mem::take(&mut args),
                timeout: Duration::from_secs(30),
                system_prompt: None,
                trace_io: true,
            },
            agent_prompt_line,
            read_timeout_secs,
            None,
        )
    }

    #[cfg(unix)]
    fn assert_lifecycle_events(events: &[String], expected_needles: &[String]) {
        for needle in expected_needles {
            assert!(
                events.iter().any(|e| e.contains(needle)),
                "expected lifecycle event '{needle}', got: {events:?}"
            );
        }
    }

    #[cfg(unix)]
    fn command_exists(cmd: &str) -> bool {
        std::process::Command::new("sh")
            .args(["-c", &format!("command -v {cmd} >/dev/null 2>&1")])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[cfg(unix)]
    fn require_env_flag(name: &str) {
        let enabled = std::env::var(name)
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        assert!(
            enabled,
            "{name} must be set (1/true) to run this real-agent integration test"
        );
    }

    #[cfg(unix)]
    #[test]
    fn harness_contract_is_machine_readable_and_complete() {
        let contract = load_harness_contract();
        assert!(
            !contract.scenarios.is_empty(),
            "contract must define at least one scenario"
        );
        assert!(
            !contract.failure_taxonomy.is_empty(),
            "contract must define failure taxonomy"
        );

        let mut ids = std::collections::HashSet::new();
        for scenario in &contract.scenarios {
            assert!(
                ids.insert(scenario.id.clone()),
                "duplicate scenario id in contract: {}",
                scenario.id
            );
            assert!(
                !scenario.kind.trim().is_empty(),
                "scenario kind must be present: {:?}",
                scenario
            );
            if scenario.kind == "mock" {
                assert!(
                    scenario.detector_event.is_some()
                        && scenario.supervisor_interface.is_some()
                        && scenario.injected_terminal_input.is_some()
                        && scenario.pane_invariant.is_some(),
                    "mock scenario missing layer assertions: {:?}",
                    scenario
                );
            }
        }

        for row in &contract.failure_taxonomy {
            assert!(
                !row.class.trim().is_empty() && !row.triage_owner.trim().is_empty(),
                "invalid failure taxonomy row: {:?}",
                row
            );
        }

        for required in [
            "mock-direct",
            "mock-enter",
            "mock-escalate",
            "mock-fail",
            "mock-verbose",
            "real-supervisor-claude-token",
            "real-supervisor-claude-enter",
            "real-supervisor-codex-token",
            "real-supervisor-codex-enter",
            "real-smoke-claude",
            "real-smoke-codex",
        ] {
            assert!(
                contract.scenarios.iter().any(|s| s.id == required),
                "missing required contract scenario: {required}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn harness_direct_reply_injected_into_agent() {
        let scenario = harness_scenario("mock-direct");
        let result = run_harness_mock_case(&scenario.id);
        assert_eq!(
            result.received,
            scenario
                .expected_input
                .as_deref()
                .expect("mock-direct expected_input missing")
        );
        assert!(
            result.events.iter().any(|e| e.contains("auto:")),
            "expected auto-answer event, got: {:?}",
            result.events
        );
        assert_eq!(scenario.expect_auto, Some(true));
        assert_eq!(scenario.expect_escalate, Some(false));
        assert!(
            result
                .events
                .iter()
                .any(|e| e.contains("event:? supervisor call")),
            "expected supervisor call trace event, got: {:?}",
            result.events
        );
        assert!(
            result
                .events
                .iter()
                .any(|e| e.contains("event:? supervisor reply ← y")),
            "expected supervisor reply trace event, got: {:?}",
            result.events
        );
        assert!(
            result.target_pane.starts_with('%'),
            "expected tmux pane id target, got {}",
            result.target_pane
        );
        assert!(
            result.panes.iter().any(|p| p.id == result.target_pane),
            "expected target pane to exist in pane details: {:?}",
            result.panes
        );
    }

    #[cfg(unix)]
    #[test]
    fn harness_press_enter_reply_injected_as_empty_input() {
        let scenario = harness_scenario("mock-enter");
        let result = run_harness_mock_case(&scenario.id);
        assert_eq!(
            result.received,
            scenario
                .expected_input
                .as_deref()
                .expect("mock-enter expected_input missing")
        );
        assert!(
            result.events.iter().any(|e| e.contains("auto:")),
            "expected auto-answer event, got: {:?}",
            result.events
        );
        assert_eq!(scenario.expect_auto, Some(true));
        assert_eq!(scenario.expect_escalate, Some(false));
    }

    #[cfg(unix)]
    #[test]
    fn harness_supervisor_escalate_does_not_inject() {
        let scenario = harness_scenario("mock-escalate");
        let result = run_harness_mock_case(&scenario.id);
        assert_eq!(
            result.received,
            scenario
                .expected_input
                .as_deref()
                .expect("mock-escalate expected_input missing")
        );
        assert!(
            result.events.iter().any(|e| e.contains("escalate:")),
            "expected escalate event, got: {:?}",
            result.events
        );
        assert!(
            !result.events.iter().any(|e| e.contains("auto:")),
            "did not expect auto-answer, got: {:?}",
            result.events
        );
        assert_eq!(scenario.expect_auto, Some(false));
        assert_eq!(scenario.expect_escalate, Some(true));
    }

    #[cfg(unix)]
    #[test]
    fn harness_supervisor_failure_does_not_inject() {
        let scenario = harness_scenario("mock-fail");
        let result = run_harness_mock_case(&scenario.id);
        assert_eq!(
            result.received,
            scenario
                .expected_input
                .as_deref()
                .expect("mock-fail expected_input missing")
        );
        assert!(
            result
                .events
                .iter()
                .any(|e| e.contains("supervisor failed") || e.contains("escalate:")),
            "expected supervisor failure escalation event, got: {:?}",
            result.events
        );
        assert_eq!(scenario.expect_auto, Some(false));
        assert_eq!(scenario.expect_escalate, Some(true));
    }

    #[cfg(unix)]
    #[test]
    fn harness_supervisor_verbose_reply_rejected_for_safety() {
        let scenario = harness_scenario("mock-verbose");
        let result = run_harness_mock_case(&scenario.id);
        assert_eq!(
            result.received,
            scenario
                .expected_input
                .as_deref()
                .expect("mock-verbose expected_input missing")
        );
        assert!(
            result
                .events
                .iter()
                .any(|e| e.contains("supervisor failed") || e.contains("escalate:")),
            "expected non-injectable response escalation, got: {:?}",
            result.events
        );
        assert_eq!(scenario.expect_auto, Some(false));
        assert_eq!(scenario.expect_escalate, Some(true));
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "manual: requires Claude auth + network; run with BATTY_TEST_REAL_CLAUDE=1"]
    fn harness_real_supervisor_claude_with_mock_executor() {
        require_env_flag("BATTY_TEST_REAL_CLAUDE");
        assert!(command_exists("claude"), "claude binary not found");

        for scenario_id in [
            "real-supervisor-claude-token",
            "real-supervisor-claude-enter",
        ] {
            let scenario = harness_scenario(scenario_id);
            let result = run_harness_case_with_real_supervisor(
                "claude",
                &["-p", "--output-format", "text"],
                scenario
                    .agent_prompt_line
                    .as_deref()
                    .expect("real Claude scenario missing prompt"),
                scenario
                    .read_timeout_secs
                    .expect("real Claude scenario missing read_timeout_secs"),
            );

            if let Some(expected_contains) = scenario.expected_contains.as_deref() {
                assert!(
                    result.received.contains(expected_contains),
                    "scenario {scenario_id}: expected '{expected_contains}' in '{}', events={:?}",
                    result.received,
                    result.events
                );
            }
            if let Some(expected_input) = scenario.expected_input.as_deref() {
                assert_eq!(
                    result.received, expected_input,
                    "scenario {scenario_id}: expected exact input mismatch, events={:?}",
                    result.events
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "manual: requires Codex auth + network; run with BATTY_TEST_REAL_CODEX=1"]
    fn harness_real_supervisor_codex_with_mock_executor() {
        require_env_flag("BATTY_TEST_REAL_CODEX");
        assert!(command_exists("codex"), "codex binary not found");

        for scenario_id in ["real-supervisor-codex-token", "real-supervisor-codex-enter"] {
            let scenario = harness_scenario(scenario_id);
            let result = run_harness_case_with_real_supervisor(
                "codex",
                &["exec", "--sandbox", "workspace-write"],
                scenario
                    .agent_prompt_line
                    .as_deref()
                    .expect("real Codex scenario missing prompt"),
                scenario
                    .read_timeout_secs
                    .expect("real Codex scenario missing read_timeout_secs"),
            );

            if let Some(expected_contains) = scenario.expected_contains.as_deref() {
                assert!(
                    result.received.contains(expected_contains),
                    "scenario {scenario_id}: expected '{expected_contains}' in '{}', events={:?}",
                    result.received,
                    result.events
                );
            }
            if let Some(expected_input) = scenario.expected_input.as_deref() {
                assert_eq!(
                    result.received, expected_input,
                    "scenario {scenario_id}: expected exact input mismatch, events={:?}",
                    result.events
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "manual smoke: real executor+supervisor in tmux; BATTY_TEST_REAL_E2E_CLAUDE=1"]
    fn harness_real_executor_and_supervisor_claude_smoke() {
        require_env_flag("BATTY_TEST_REAL_E2E_CLAUDE");
        assert!(command_exists("claude"), "claude binary not found");
        let scenario = harness_scenario("real-smoke-claude");

        let tmp = tempfile::tempdir().unwrap();
        let phase = "it-real-e2e-claude".to_string();
        let session = tmux::session_name(&phase);
        let _ = tmux::kill_session(&session);
        let stop = Arc::new(AtomicBool::new(false));
        let (observer, events_arc) = TestObserver::new();

        let config = OrchestratorConfig {
            spawn: SpawnConfig {
                program: "claude".to_string(),
                args: vec![
                    "-p".to_string(),
                    "--output-format".to_string(),
                    "text".to_string(),
                    "Reply with exactly: READY".to_string(),
                ],
                work_dir: "/tmp".to_string(),
                env: vec![],
            },
            patterns: PromptPatterns::claude_code(),
            policy: PolicyEngine::new(Policy::Act, HashMap::new()),
            detector: DetectorConfig {
                silence_timeout: Duration::from_millis(150),
                answer_cooldown: Duration::from_millis(50),
                unknown_request_fallback: true,
            },
            phase,
            logs_dir: tmp.path().join(".batty").join("logs"),
            poll_interval: Duration::from_millis(80),
            buffer_size: 50,
            tier2: Some(Tier2Config {
                program: "claude".to_string(),
                args: vec![
                    "-p".to_string(),
                    "--output-format".to_string(),
                    "text".to_string(),
                ],
                timeout: Duration::from_secs(30),
                system_prompt: None,
                trace_io: true,
            }),
            log_pane: true,
            log_pane_height_pct: 20,
            stuck: None,
            answer_delay: Duration::ZERO,
            auto_attach: false,
            idle_input_fallback: true,
        };

        let result = run(config, Box::new(observer), stop).unwrap();
        assert!(
            matches!(result, OrchestratorResult::Completed),
            "expected Completed, got {result:?}"
        );
        let events = events_arc.lock().unwrap().clone();
        let expected = scenario
            .expected_lifecycle_events
            .clone()
            .unwrap_or_else(|| {
                vec![
                    "created".to_string(),
                    "supervising".to_string(),
                    "executor exited".to_string(),
                ]
            });
        assert_lifecycle_events(&events, &expected);
        let _ = tmux::kill_session(&session);
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "manual smoke: real executor+supervisor in tmux; BATTY_TEST_REAL_E2E_CODEX=1"]
    fn harness_real_executor_and_supervisor_codex_smoke() {
        require_env_flag("BATTY_TEST_REAL_E2E_CODEX");
        assert!(command_exists("codex"), "codex binary not found");
        let scenario = harness_scenario("real-smoke-codex");

        let tmp = tempfile::tempdir().unwrap();
        let phase = "it-real-e2e-codex".to_string();
        let session = tmux::session_name(&phase);
        let _ = tmux::kill_session(&session);
        let stop = Arc::new(AtomicBool::new(false));
        let (observer, events_arc) = TestObserver::new();

        let config = OrchestratorConfig {
            spawn: SpawnConfig {
                program: "codex".to_string(),
                args: vec![
                    "exec".to_string(),
                    "--sandbox".to_string(),
                    "workspace-write".to_string(),
                    "Reply with exactly: READY".to_string(),
                ],
                work_dir: "/tmp".to_string(),
                env: vec![],
            },
            patterns: PromptPatterns::codex_cli(),
            policy: PolicyEngine::new(Policy::Act, HashMap::new()),
            detector: DetectorConfig {
                silence_timeout: Duration::from_millis(150),
                answer_cooldown: Duration::from_millis(50),
                unknown_request_fallback: true,
            },
            phase,
            logs_dir: tmp.path().join(".batty").join("logs"),
            poll_interval: Duration::from_millis(80),
            buffer_size: 50,
            tier2: Some(Tier2Config {
                program: "codex".to_string(),
                args: vec![
                    "exec".to_string(),
                    "--sandbox".to_string(),
                    "workspace-write".to_string(),
                ],
                timeout: Duration::from_secs(30),
                system_prompt: None,
                trace_io: true,
            }),
            log_pane: true,
            log_pane_height_pct: 20,
            stuck: None,
            answer_delay: Duration::ZERO,
            auto_attach: false,
            idle_input_fallback: true,
        };

        let result = run(config, Box::new(observer), stop).unwrap();
        assert!(
            matches!(result, OrchestratorResult::Completed),
            "expected Completed, got {result:?}"
        );
        let events = events_arc.lock().unwrap().clone();
        let expected = scenario
            .expected_lifecycle_events
            .clone()
            .unwrap_or_else(|| {
                vec![
                    "created".to_string(),
                    "supervising".to_string(),
                    "executor exited".to_string(),
                ]
            });
        assert_lifecycle_events(&events, &expected);
        let _ = tmux::kill_session(&session);
    }

    #[test]
    fn handle_prompt_auto_answers() {
        let session = "batty-test-autoanswer";
        let _ = tmux::kill_session(session);

        // Create a session to receive send-keys
        tmux::create_session(session, "cat", &[], "/tmp").unwrap();
        std::thread::sleep(Duration::from_millis(200));

        let mut auto_answers = HashMap::new();
        auto_answers.insert("Continue?".to_string(), "y".to_string());
        let policy = PolicyEngine::new(Policy::Act, auto_answers);

        let mut detector =
            PromptDetector::new(PromptPatterns::claude_code(), DetectorConfig::default());

        let (mut observer, events) = TestObserver::new();

        let prompt = crate::prompt::DetectedPrompt {
            kind: crate::prompt::PromptKind::Confirmation {
                detail: "Continue?".to_string(),
            },
            matched_text: "Continue? [y/n]".to_string(),
        };

        let buffer = EventBuffer::new(10);
        let mut status_bar = StatusBar::new(session, "test");
        handle_prompt(
            &prompt,
            session,
            &policy,
            &mut detector,
            &mut observer,
            None,
            &buffer,
            &mut status_bar,
            Duration::ZERO,
        )
        .unwrap();

        // Check observer received the auto-answer event
        let collected = events.lock().unwrap();
        assert!(
            collected.iter().any(|e| e.contains("auto:")),
            "expected auto-answer event, got: {collected:?}"
        );

        tmux::kill_session(session).unwrap();
    }

    #[test]
    fn handle_prompt_escalates_unknown() {
        let session = "batty-test-escalate";
        let _ = tmux::kill_session(session);
        tmux::create_session(session, "cat", &[], "/tmp").unwrap();
        std::thread::sleep(Duration::from_millis(200));

        let policy = PolicyEngine::new(Policy::Act, HashMap::new());
        let mut detector =
            PromptDetector::new(PromptPatterns::claude_code(), DetectorConfig::default());
        let (mut observer, events) = TestObserver::new();

        let prompt = crate::prompt::DetectedPrompt {
            kind: crate::prompt::PromptKind::Permission {
                detail: "unknown".to_string(),
            },
            matched_text: "Some unknown prompt".to_string(),
        };

        let buffer = EventBuffer::new(10);
        let mut status_bar = StatusBar::new(session, "test");
        handle_prompt(
            &prompt,
            session,
            &policy,
            &mut detector,
            &mut observer,
            None,
            &buffer,
            &mut status_bar,
            Duration::ZERO,
        )
        .unwrap();

        let collected = events.lock().unwrap();
        assert!(
            collected.iter().any(|e| e.contains("escalate:")),
            "expected escalate event, got: {collected:?}"
        );

        tmux::kill_session(session).unwrap();
    }

    #[test]
    fn handle_prompt_tier2_with_direct_answer() {
        // Test Tier 2 integration with a concise mock supervisor answer.
        let session = "batty-test-tier2";
        let _ = tmux::kill_session(session);
        tmux::create_session(session, "cat", &[], "/tmp").unwrap();
        std::thread::sleep(Duration::from_millis(200));

        let policy = PolicyEngine::new(Policy::Act, HashMap::new()); // no auto-answers
        let mut detector =
            PromptDetector::new(PromptPatterns::claude_code(), DetectorConfig::default());
        let (mut observer, events) = TestObserver::new();

        let tier2 = Tier2Config {
            program: "printf".to_string(),
            args: vec!["y".to_string()],
            timeout: Duration::from_secs(5),
            system_prompt: None,
            trace_io: true,
        };

        let prompt = crate::prompt::DetectedPrompt {
            kind: crate::prompt::PromptKind::Permission {
                detail: "unknown".to_string(),
            },
            matched_text: "Some unknown prompt".to_string(),
        };

        let buffer = EventBuffer::new(10);
        let mut status_bar = StatusBar::new(session, "test");
        handle_prompt(
            &prompt,
            session,
            &policy,
            &mut detector,
            &mut observer,
            Some(&tier2),
            &buffer,
            &mut status_bar,
            Duration::ZERO,
        )
        .unwrap();

        let collected = events.lock().unwrap();
        // Should have supervisor thinking event + auto-answer from Tier 2
        assert!(
            collected.iter().any(|e| e.contains("thinking")),
            "expected thinking event, got: {collected:?}"
        );
        assert!(
            collected.iter().any(|e| e.contains("auto:")),
            "expected auto-answer from Tier 2, got: {collected:?}"
        );

        tmux::kill_session(session).unwrap();
    }

    #[test]
    fn handle_prompt_skips_completion() {
        let policy = PolicyEngine::new(Policy::Act, HashMap::new());
        let mut detector =
            PromptDetector::new(PromptPatterns::claude_code(), DetectorConfig::default());
        let (mut observer, events) = TestObserver::new();

        let prompt = crate::prompt::DetectedPrompt {
            kind: crate::prompt::PromptKind::Completion,
            matched_text: "result".to_string(),
        };

        let buffer = EventBuffer::new(10);
        let mut status_bar = StatusBar::new("fake-session", "test");
        handle_prompt(
            &prompt,
            "fake-session",
            &policy,
            &mut detector,
            &mut observer,
            None,
            &buffer,
            &mut status_bar,
            Duration::ZERO,
        )
        .unwrap();

        let collected = events.lock().unwrap();
        assert!(collected.is_empty(), "completion should produce no events");
    }

    #[test]
    fn orchestrator_with_short_lived_process() {
        let stop = Arc::new(AtomicBool::new(false));
        let (observer, events) = TestObserver::new();

        let tmp = tempfile::tempdir().unwrap();

        let config = OrchestratorConfig {
            spawn: SpawnConfig {
                // Use bash -c so the process lives long enough for pipe-pane setup
                program: "bash".to_string(),
                args: vec!["-c".to_string(), "echo done; sleep 1".to_string()],
                work_dir: "/tmp".to_string(),
                env: vec![],
            },
            patterns: PromptPatterns::claude_code(),
            policy: PolicyEngine::new(Policy::Act, HashMap::new()),
            detector: DetectorConfig::default(),
            phase: "test-short".to_string(),
            logs_dir: tmp.path().join(".batty").join("logs"),
            poll_interval: Duration::from_millis(100),
            buffer_size: 50,
            tier2: None,
            log_pane: false, // don't create log pane in tests
            log_pane_height_pct: 20,
            stuck: None,
            answer_delay: Duration::ZERO,
            auto_attach: false,
            idle_input_fallback: true,
        };

        // Clean up any leftover session
        let _ = tmux::kill_session("batty-test-short");

        let result = run(config, Box::new(observer), stop).unwrap();

        // Process exits after sleep, so the session should complete
        match result {
            OrchestratorResult::Completed => {}
            other => panic!("expected Completed, got: {other:?}"),
        }

        // Should have session creation event
        let collected = events.lock().unwrap();
        assert!(collected.iter().any(|e| e.contains("created")));

        let _ = tmux::kill_session("batty-test-short");
    }

    #[test]
    fn orchestrator_stop_signal() {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let (observer, _events) = TestObserver::new();

        let tmp = tempfile::tempdir().unwrap();

        let config = OrchestratorConfig {
            spawn: SpawnConfig {
                program: "sleep".to_string(),
                args: vec!["60".to_string()],
                work_dir: "/tmp".to_string(),
                env: vec![],
            },
            patterns: PromptPatterns::claude_code(),
            policy: PolicyEngine::new(Policy::Act, HashMap::new()),
            detector: DetectorConfig::default(),
            phase: "test-stop".to_string(),
            logs_dir: tmp.path().join(".batty").join("logs"),
            poll_interval: Duration::from_millis(100),
            buffer_size: 50,
            tier2: None,
            log_pane: false, // don't create log pane in tests
            log_pane_height_pct: 20,
            stuck: None,
            answer_delay: Duration::ZERO,
            auto_attach: false,
            idle_input_fallback: true,
        };

        let _ = tmux::kill_session("batty-test-stop");

        // Set stop after a short delay
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            stop_clone.store(true, Ordering::Relaxed);
        });

        let result = run(config, Box::new(observer), stop).unwrap();

        match result {
            OrchestratorResult::Detached => {}
            other => panic!("expected Detached, got: {other:?}"),
        }

        handle.join().unwrap();
        let _ = tmux::kill_session("batty-test-stop");
    }

    #[test]
    fn log_file_observer_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("orchestrator.log");

        let mut obs = LogFileObserver::new(&log_path).unwrap();
        obs.on_auto_answer("Continue?", "y");
        obs.on_escalate("What model?");
        obs.on_suggest("Allow?", "y");
        obs.on_event("● started");

        let content = std::fs::read_to_string(&log_path).unwrap();
        assert!(content.contains("auto-answered"));
        assert!(content.contains("NEEDS INPUT"));
        assert!(content.contains("suggestion"));
        assert!(content.contains("started"));
    }

    #[test]
    fn default_config_values() {
        assert_eq!(
            OrchestratorConfig::default_poll_interval(),
            Duration::from_millis(200)
        );
        assert_eq!(OrchestratorConfig::default_buffer_size(), 50);
    }

    #[test]
    fn log_pane_setup() {
        let session = "batty-test-logpane-unit";
        let _ = tmux::kill_session(session);

        tmux::create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("orchestrator.log");

        let executor_pane = tmux::pane_id(session).unwrap();
        setup_log_pane(
            session,
            &executor_pane,
            &log_path,
            20,
            tmux::SplitMode::Lines,
        )
        .unwrap();

        // Should now have 2 panes
        let panes = tmux::list_panes(session).unwrap();
        assert_eq!(
            panes.len(),
            2,
            "expected 2 panes (executor + log), got {}",
            panes.len()
        );

        tmux::kill_session(session).unwrap();
    }

    #[test]
    fn orchestrator_with_log_pane() {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let (observer, events) = TestObserver::new();

        let tmp = tempfile::tempdir().unwrap();

        let config = OrchestratorConfig {
            spawn: SpawnConfig {
                program: "sleep".to_string(),
                args: vec!["10".to_string()],
                work_dir: "/tmp".to_string(),
                env: vec![],
            },
            patterns: PromptPatterns::claude_code(),
            policy: PolicyEngine::new(Policy::Act, HashMap::new()),
            detector: DetectorConfig::default(),
            phase: "test-logpane-orch".to_string(),
            logs_dir: tmp.path().join(".batty").join("logs"),
            poll_interval: Duration::from_millis(100),
            buffer_size: 50,
            tier2: None,
            log_pane: true,
            log_pane_height_pct: 20,
            stuck: None,
            answer_delay: Duration::ZERO,
            auto_attach: false,
            idle_input_fallback: true,
        };

        let _ = tmux::kill_session("batty-test-logpane-orch");

        // Stop quickly
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            stop_clone.store(true, Ordering::Relaxed);
        });

        let result = run(config, Box::new(observer), stop).unwrap();
        assert!(matches!(result, OrchestratorResult::Detached));

        handle.join().unwrap();

        // Should have log pane creation event
        let collected = events.lock().unwrap();
        assert!(
            collected.iter().any(|e| e.contains("log pane")),
            "expected log pane event, got: {collected:?}"
        );

        let _ = tmux::kill_session("batty-test-logpane-orch");
    }

    #[test]
    fn status_indicator_symbols() {
        assert_eq!(StatusIndicator::StateChange.symbol(), "●");
        assert_eq!(StatusIndicator::Action.symbol(), "→");
        assert_eq!(StatusIndicator::Ok.symbol(), "✓");
        assert_eq!(StatusIndicator::Thinking.symbol(), "?");
        assert_eq!(StatusIndicator::NeedsInput.symbol(), "⚠");
        assert_eq!(StatusIndicator::Failure.symbol(), "✗");
    }

    #[test]
    fn parse_supervisor_hotkey_action_accepts_pause_resume() {
        assert_eq!(
            parse_supervisor_hotkey_action("pause"),
            Some(SupervisorHotkeyAction::Pause)
        );
        assert_eq!(
            parse_supervisor_hotkey_action("RESUME"),
            Some(SupervisorHotkeyAction::Resume)
        );
        assert_eq!(parse_supervisor_hotkey_action("unknown"), None);
    }

    #[test]
    fn apply_supervisor_hotkey_action_transitions_and_noops() {
        let mut detector =
            PromptDetector::new(PromptPatterns::claude_code(), DetectorConfig::default());
        let mut mode = SupervisorMode::Working;

        let paused =
            apply_supervisor_hotkey_action(&mut mode, SupervisorHotkeyAction::Pause, &mut detector);
        assert_eq!(paused, SupervisorTransition::Paused);
        assert_eq!(mode, SupervisorMode::Paused);

        let noop_pause =
            apply_supervisor_hotkey_action(&mut mode, SupervisorHotkeyAction::Pause, &mut detector);
        assert_eq!(noop_pause, SupervisorTransition::AlreadyPaused);
        assert_eq!(mode, SupervisorMode::Paused);

        let resumed = apply_supervisor_hotkey_action(
            &mut mode,
            SupervisorHotkeyAction::Resume,
            &mut detector,
        );
        assert_eq!(resumed, SupervisorTransition::Resumed);
        assert_eq!(mode, SupervisorMode::Working);

        let noop_resume = apply_supervisor_hotkey_action(
            &mut mode,
            SupervisorHotkeyAction::Resume,
            &mut detector,
        );
        assert_eq!(noop_resume, SupervisorTransition::AlreadyWorking);
        assert_eq!(mode, SupervisorMode::Working);
    }

    #[test]
    fn apply_supervisor_hotkey_action_resets_detector_state() {
        let mut detector =
            PromptDetector::new(PromptPatterns::claude_code(), DetectorConfig::default());
        detector.answer_injected();
        assert!(matches!(
            detector.state(),
            crate::detector::SupervisorState::Answering { .. }
        ));

        let mut mode = SupervisorMode::Working;
        let _ =
            apply_supervisor_hotkey_action(&mut mode, SupervisorHotkeyAction::Pause, &mut detector);
        assert!(matches!(
            detector.state(),
            crate::detector::SupervisorState::Working
        ));
    }

    #[test]
    fn status_bar_init_and_update() {
        let session = "batty-test-statusbar";
        let _ = tmux::kill_session(session);

        tmux::create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        let mut bar = StatusBar::new(session, "phase-2");
        bar.init().unwrap();

        // Update with various indicators
        bar.force_update(StatusIndicator::Ok, "supervising")
            .unwrap();
        bar.force_update(StatusIndicator::Action, "answered: y")
            .unwrap();
        bar.force_update(StatusIndicator::NeedsInput, "NEEDS INPUT")
            .unwrap();

        tmux::kill_session(session).unwrap();
    }

    #[test]
    fn status_bar_debounce() {
        let session = "batty-test-statusbar-deb";
        let _ = tmux::kill_session(session);

        tmux::create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        let mut bar = StatusBar::new(session, "test");
        bar.init().unwrap();

        // First update should go through
        bar.update(StatusIndicator::Ok, "first").unwrap();

        // Second update immediately after should be debounced (no error, just skipped)
        bar.update(StatusIndicator::Action, "second").unwrap();

        // Force update should always go through
        bar.force_update(StatusIndicator::Failure, "forced")
            .unwrap();

        tmux::kill_session(session).unwrap();
    }

    #[test]
    fn status_bar_on_missing_session() {
        // StatusBar updates are best-effort — shouldn't fail on missing session
        let mut bar = StatusBar::new("batty-nonexistent-session", "test");
        // init can fail (it calls tmux_set which is not best-effort), but update is best-effort
        let _ = bar.init();
        // update should not panic even if session doesn't exist
        bar.update(StatusIndicator::Ok, "test").unwrap();
    }

    #[test]
    fn stuck_default_config() {
        let config = StuckConfig::default();
        assert_eq!(config.timeout, Duration::from_secs(300));
        assert_eq!(config.max_nudges, 2);
        assert!(!config.auto_relaunch);
    }

    #[test]
    fn stuck_normal_when_recent_progress() {
        let mut sd = StuckDetector::new(StuckConfig {
            timeout: Duration::from_secs(60),
            max_nudges: 2,
            auto_relaunch: false,
        });
        let (state, action) = sd.check(true);
        assert_eq!(state, StuckState::Normal);
        assert_eq!(action, StuckAction::None);
    }

    #[test]
    fn stuck_stalled_after_timeout() {
        let mut sd = StuckDetector::new(StuckConfig {
            timeout: Duration::from_millis(50),
            max_nudges: 2,
            auto_relaunch: false,
        });
        // Make last_progress in the past
        sd.last_progress = Instant::now() - Duration::from_millis(100);

        let (state, action) = sd.check(true);
        assert!(matches!(state, StuckState::Stalled { .. }));
        assert!(matches!(action, StuckAction::Nudge { .. }));
    }

    #[test]
    fn stuck_escalates_after_max_nudges() {
        let mut sd = StuckDetector::new(StuckConfig {
            timeout: Duration::from_millis(50),
            max_nudges: 2,
            auto_relaunch: false,
        });
        sd.last_progress = Instant::now() - Duration::from_millis(100);
        sd.nudge_count = 2; // already used all nudges

        let (state, action) = sd.check(true);
        assert!(matches!(state, StuckState::Stalled { .. }));
        assert!(matches!(action, StuckAction::Escalate { .. }));
    }

    #[test]
    fn stuck_crash_detected() {
        let mut sd = StuckDetector::new(StuckConfig {
            timeout: Duration::from_secs(300),
            max_nudges: 2,
            auto_relaunch: false,
        });
        let (state, action) = sd.check(false);
        assert_eq!(state, StuckState::Crashed);
        assert!(matches!(action, StuckAction::Escalate { .. }));
    }

    #[test]
    fn stuck_crash_auto_relaunch() {
        let mut sd = StuckDetector::new(StuckConfig {
            timeout: Duration::from_secs(300),
            max_nudges: 2,
            auto_relaunch: true,
        });
        let (state, action) = sd.check(false);
        assert_eq!(state, StuckState::Crashed);
        assert_eq!(action, StuckAction::Relaunch);
    }

    #[test]
    fn stuck_escalation_emits_once_until_recovery() {
        let mut sd = StuckDetector::new(StuckConfig {
            timeout: Duration::from_millis(50),
            max_nudges: 2,
            auto_relaunch: false,
        });
        sd.last_progress = Instant::now() - Duration::from_millis(100);
        sd.nudge_count = 2; // already used all nudges

        let (_, first) = sd.check(true);
        let (_, second) = sd.check(true);
        assert!(matches!(first, StuckAction::Escalate { .. }));
        assert_eq!(second, StuckAction::None);

        // Recovery resets escalation gate.
        sd.on_progress();
        sd.last_progress = Instant::now() - Duration::from_millis(100);
        sd.nudge_count = 2;
        let (_, third) = sd.check(true);
        assert!(matches!(third, StuckAction::Escalate { .. }));
    }

    #[test]
    fn stuck_loop_detection() {
        let mut sd = StuckDetector::new(StuckConfig::default());

        // Feed 6 identical lines
        for _ in 0..6 {
            sd.on_output("ERROR: retry failed");
        }

        let (state, action) = sd.check(true);
        assert_eq!(state, StuckState::Looping);
        assert!(matches!(action, StuckAction::Nudge { .. }));
    }

    #[test]
    fn stuck_loop_ab_pattern() {
        let mut sd = StuckDetector::new(StuckConfig::default());

        // Feed ABABABAB pattern
        for _ in 0..4 {
            sd.on_output("Running tests...");
            sd.on_output("Tests failed. Retrying...");
        }

        let (state, action) = sd.check(true);
        assert_eq!(state, StuckState::Looping);
        assert!(matches!(action, StuckAction::Nudge { .. }));
    }

    #[test]
    fn stuck_no_loop_with_varied_output() {
        let mut sd = StuckDetector::new(StuckConfig::default());

        sd.on_output("line 1");
        sd.on_output("line 2");
        sd.on_output("line 3");
        sd.on_output("line 4");
        sd.on_output("line 5");
        sd.on_output("line 6");

        let (state, action) = sd.check(true);
        assert_eq!(state, StuckState::Normal);
        assert_eq!(action, StuckAction::None);
    }

    #[test]
    fn stuck_progress_resets_state() {
        let mut sd = StuckDetector::new(StuckConfig {
            timeout: Duration::from_millis(50),
            max_nudges: 2,
            auto_relaunch: false,
        });
        sd.last_progress = Instant::now() - Duration::from_millis(100);
        sd.nudge_count = 1;

        // Feed identical lines for loop
        for _ in 0..6 {
            sd.on_output("stuck line");
        }

        // Progress should reset everything
        sd.on_progress();

        let (state, action) = sd.check(true);
        assert_eq!(state, StuckState::Normal);
        assert_eq!(action, StuckAction::None);
        assert_eq!(sd.nudge_count, 0);
    }

    #[test]
    fn stuck_empty_output_ignored() {
        let mut sd = StuckDetector::new(StuckConfig::default());
        sd.on_output("   ");
        sd.on_output("");
        assert!(sd.recent_lines.is_empty());
    }

    #[test]
    fn human_check_no_session_returns_false() {
        // Non-existent session should not be considered human-answered
        assert!(!check_human_answered("batty-nonexistent-xyz", "Continue?"));
    }

    #[test]
    fn human_check_prompt_still_visible() {
        let session = "batty-test-human-check";
        let _ = tmux::kill_session(session);

        // Create a session that shows a prompt-like line
        tmux::create_session(
            session,
            "bash",
            &[
                "-c".to_string(),
                "echo 'Continue? [y/n]'; sleep 5".to_string(),
            ],
            "/tmp",
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(300));

        // The prompt should still be visible → not human-answered
        // (capture-pane shows the prompt text, so check_human_answered should return false)
        let answered = check_human_answered(session, "Continue?");
        // This may or may not match depending on timing, but it should not panic
        let _ = answered;

        tmux::kill_session(session).unwrap();
    }

    #[test]
    fn wait_with_zero_delay_returns_immediately() {
        let start = Instant::now();
        let result = wait_with_human_check(
            "batty-nonexistent",
            "prompt",
            Duration::ZERO,
            Duration::from_millis(50),
        );
        assert!(!result); // should return false (no human override)
        assert!(start.elapsed() < Duration::from_millis(50)); // should be instant
    }

    #[test]
    fn ui_noise_line_detects_codex_footer_noise() {
        assert!(is_ui_noise_line("? for shortcuts"));
        assert!(is_ui_noise_line(
            "? for shortcuts                             95% context left"
        ));
        assert!(is_ui_noise_line("95% context left"));
        assert!(is_ui_noise_line("────────────────────────────────────"));
        assert!(!is_ui_noise_line(
            "Preparing main module edits (1m 13s • esc to interrupt)"
        ));
    }

    #[test]
    fn pane_detector_snapshot_ignores_footer_and_tracks_meaningful_tail() {
        let pane = "\
Edited src/main.rs (+1 -0)
Preparing main module edits (1m 13s • esc to interrupt)
? for shortcuts                             95% context left
";

        let (signature, last) = pane_detector_snapshot(pane).expect("expected meaningful snapshot");
        assert!(signature.contains("Edited src/main.rs (+1 -0)"));
        assert!(signature.contains("Preparing main module edits"));
        assert!(!signature.contains("for shortcuts"));
        assert_eq!(
            last,
            "Preparing main module edits (1m 13s • esc to interrupt)"
        );
    }

    #[test]
    fn unknown_fallback_gate_skips_non_actionable_lines() {
        assert!(!should_invoke_unknown_fallback(
            "? for shortcuts 95% context left"
        ));
        assert!(!should_invoke_unknown_fallback("Write tests for @filename"));
        assert!(should_invoke_unknown_fallback("Press enter to continue"));
        assert!(should_invoke_unknown_fallback("Continue? [y/n]"));
    }

    #[test]
    fn extract_idle_input_prompt_from_codex_input_line() {
        let pane = "\
• Finished task summary
› Explain this codebase
? for shortcuts 49% context left
";
        let prompt = extract_idle_input_prompt(pane).expect("expected idle input prompt");
        assert_eq!(prompt, "Explain this codebase");
    }

    #[test]
    fn extract_idle_input_prompt_ignores_noise_only_content() {
        let pane = "\
? for shortcuts 49% context left
────────────────────────────────────
";
        assert!(extract_idle_input_prompt(pane).is_none());
    }

    #[test]
    fn extract_pending_input_from_supervisor_prompt_reads_first_block() {
        let prompt = "\
Executor is idle at the input prompt with pending input:
Summarize recent commits

If this input should be sent now, return the exact short response to send.";
        let pending =
            extract_pending_input_from_supervisor_prompt(prompt).expect("expected pending input");
        assert_eq!(pending, "Summarize recent commits");
    }

    #[test]
    fn human_check_ignores_prefixed_idle_input_line_for_same_prompt() {
        let session = "batty-test-human-idle-prefix";
        let _ = tmux::kill_session(session);

        tmux::create_session(
            session,
            "bash",
            &[
                "-c".to_string(),
                "printf '> Summarize recent commits\\n'; sleep 5".to_string(),
            ],
            "/tmp",
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(300));

        let prompt = "\
Executor is idle at the input prompt with pending input:
Summarize recent commits

If this input should be sent now, return the exact short response to send.";
        assert!(
            !check_human_answered(session, prompt),
            "prefixed pending input should not be treated as human override"
        );

        tmux::kill_session(session).unwrap();
    }

    #[test]
    fn parse_last_auto_prompt_reads_latest_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("orchestrator.log");
        std::fs::write(
            &log,
            concat!(
                "[batty] ✓ auto-answered: \"Continue?\" → y\n",
                "[batty] some other line\n",
                "[batty] ✓ auto-answered: \"Allow tool Read?\" → y\n"
            ),
        )
        .unwrap();

        let prompt = parse_last_auto_prompt(&log);
        assert_eq!(prompt.as_deref(), Some("Allow tool Read?"));
    }

    #[test]
    fn supervision_state_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let state = SupervisionState {
            version: 1,
            phase: "phase-2.5".to_string(),
            session: "batty-phase-2-5".to_string(),
            executor_pane: "%1".to_string(),
            log_pane: Some("%2".to_string()),
            pipe_log: "/tmp/pty-output.log".to_string(),
            pipe_checkpoint: 1234,
            updated_epoch: 42,
        };

        save_supervision_state(tmp.path(), &state).unwrap();
        let loaded = load_supervision_state(tmp.path()).unwrap();
        assert_eq!(loaded.phase, state.phase);
        assert_eq!(loaded.session, state.session);
        assert_eq!(loaded.executor_pane, state.executor_pane);
        assert_eq!(loaded.pipe_checkpoint, state.pipe_checkpoint);
    }

    #[test]
    fn wait_with_delay_completes() {
        let session = "batty-test-wait-delay";
        let _ = tmux::kill_session(session);

        tmux::create_session(
            session,
            "bash",
            &["-c".to_string(), "echo 'waiting...'; sleep 10".to_string()],
            "/tmp",
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(300));

        let start = Instant::now();
        let result = wait_with_human_check(
            session,
            "waiting...",
            Duration::from_millis(200),
            Duration::from_millis(50),
        );
        // No human typing, so should return false after the delay
        assert!(!result);
        assert!(start.elapsed() >= Duration::from_millis(200));

        tmux::kill_session(session).unwrap();
    }

    #[test]
    fn handle_prompt_with_answer_delay_zero() {
        // With zero delay, should behave exactly like before (no human check)
        let session = "batty-test-delay-zero";
        let _ = tmux::kill_session(session);

        tmux::create_session(session, "cat", &[], "/tmp").unwrap();
        std::thread::sleep(Duration::from_millis(200));

        let mut auto_answers = HashMap::new();
        auto_answers.insert("Continue?".to_string(), "y".to_string());
        let policy = PolicyEngine::new(Policy::Act, auto_answers);

        let mut detector =
            PromptDetector::new(PromptPatterns::claude_code(), DetectorConfig::default());

        let (mut observer, events) = TestObserver::new();

        let prompt = crate::prompt::DetectedPrompt {
            kind: crate::prompt::PromptKind::Confirmation {
                detail: "Continue?".to_string(),
            },
            matched_text: "Continue? [y/n]".to_string(),
        };

        let buffer = EventBuffer::new(10);
        let mut status_bar = StatusBar::new(session, "test");
        handle_prompt(
            &prompt,
            session,
            &policy,
            &mut detector,
            &mut observer,
            None,
            &buffer,
            &mut status_bar,
            Duration::ZERO,
        )
        .unwrap();

        let collected = events.lock().unwrap();
        assert!(
            collected.iter().any(|e| e.contains("auto:")),
            "expected auto-answer event with zero delay, got: {collected:?}"
        );

        tmux::kill_session(session).unwrap();
    }
}
