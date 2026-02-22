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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use crate::agent::SpawnConfig;
use crate::detector::{DetectorConfig, DetectorEvent, PromptDetector};
use crate::events::{EventBuffer, PipeWatcher};
use crate::policy::{Decision, PolicyEngine};
use crate::prompt::{PromptKind, PromptPatterns};
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
    /// Project root (for log paths).
    pub project_root: PathBuf,
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
}

impl OrchestratorObserver for LogFileObserver {
    fn on_auto_answer(&mut self, prompt: &str, response: &str) {
        self.append(&format!(
            "[batty] ✓ auto-answered: \"{prompt}\" → {response}"
        ));
    }

    fn on_escalate(&mut self, prompt: &str) {
        self.append(&format!("[batty] ⚠ NEEDS INPUT: \"{prompt}\""));
    }

    fn on_suggest(&mut self, prompt: &str, response: &str) {
        self.append(&format!(
            "[batty] ? suggestion: respond to \"{prompt}\" with \"{response}\""
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
        // Style: dark background, amber text
        tmux::set_status_style(&self.session, "bg=colour235,fg=colour136")?;
        // Widen left status to fit our content
        tmux::tmux_set(&self.session, "status-left-length", "80")?;
        tmux::tmux_set(&self.session, "status-right-length", "40")?;
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
        if !force {
            if let Some(last) = self.last_update {
                if last.elapsed() < self.min_interval {
                    return Ok(());
                }
            }
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
}

impl StuckDetector {
    pub fn new(config: StuckConfig) -> Self {
        Self {
            config,
            last_progress: Instant::now(),
            nudge_count: 0,
            recent_lines: Vec::new(),
            loop_window: 20,
        }
    }

    /// Signal that meaningful progress was made (task completed, command ran, file changed, etc.).
    pub fn on_progress(&mut self) {
        self.last_progress = Instant::now();
        self.nudge_count = 0;
        self.recent_lines.clear();
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
    pub fn check(&self, session_alive: bool) -> (StuckState, StuckAction) {
        // Crash detection
        if !session_alive {
            let action = if self.config.auto_relaunch {
                StuckAction::Relaunch
            } else {
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
                return (
                    StuckState::Looping,
                    StuckAction::Escalate {
                        reason: format!(
                            "executor stuck in loop after {} nudges",
                            self.nudge_count
                        ),
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

/// Run the full orchestrator loop.
///
/// Creates a tmux session, sets up pipe-pane, and supervises the executor.
/// Returns when the executor exits or the session is killed.
pub fn run(
    config: OrchestratorConfig,
    mut observer: Box<dyn OrchestratorObserver>,
    stop: Arc<AtomicBool>,
) -> Result<OrchestratorResult> {
    // 1. Check tmux
    let version = tmux::check_tmux()?;
    info!(tmux_version = %version, "tmux available");

    // 2. Create session
    let session = tmux::session_name(&config.phase);
    let log_dir = config.project_root.join(".batty").join("logs");
    let pipe_log = log_dir.join(format!("{}-pty-output.log", config.phase));

    tmux::create_session(
        &session,
        &config.spawn.program,
        &config.spawn.args,
        &config.spawn.work_dir,
    )
    .with_context(|| format!("failed to create tmux session for phase {}", config.phase))?;

    observer.on_event(&format!("● session '{}' created", session));

    // 3. Set up pipe-pane
    tmux::setup_pipe_pane(&session, &pipe_log)?;
    observer.on_event(&format!("● pipe-pane → {}", pipe_log.display()));

    // 4. Set up status bar
    let mut status_bar = StatusBar::new(&session, &config.phase);
    status_bar.init()?;
    observer.on_event("● status bar initialized");

    // 5. Set up orchestrator log pane
    let orch_log = log_dir.join("orchestrator.log");
    if config.log_pane {
        setup_log_pane(&session, &orch_log, config.log_pane_height_pct)?;
        observer.on_event("● log pane created");
    }

    // 6. Initialize components
    let buffer = EventBuffer::new(config.buffer_size);
    let mut watcher = PipeWatcher::new(&pipe_log, buffer.clone());
    let mut detector = PromptDetector::new(config.patterns, config.detector);
    let mut stuck_detector = config.stuck.map(StuckDetector::new);

    info!(session = %session, "orchestrator loop starting");
    observer.on_event("● supervising");
    status_bar.update(StatusIndicator::Ok, "supervising")?;

    // 7. Supervision loop
    let mut last_line = String::new();
    let result = loop {
        if stop.load(Ordering::Relaxed) {
            observer.on_event("● stopped by signal");
            status_bar.force_update(StatusIndicator::StateChange, "stopped")?;
            break OrchestratorResult::Detached;
        }

        // Check if session still exists
        if !tmux::session_exists(&session) {
            observer.on_event("✓ executor exited");
            status_bar.force_update(StatusIndicator::Ok, "completed")?;
            break OrchestratorResult::Completed;
        }

        // Poll for new output
        match watcher.poll() {
            Ok(event_count) => {
                if event_count > 0 {
                    debug!(events = event_count, "new events extracted");
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
            if line != last_line {
                // Feed the raw summary to the detector for prompt matching
                // In practice, we'd feed the actual output lines, but the event
                // buffer gives us the structured view
                last_line = line;
            }
        }

        // Also check capture-pane for the most current visible content
        // This catches prompts that pipe-pane hasn't flushed yet
        if let Ok(pane_content) = tmux::capture_pane(&session) {
            if let Some(last) = pane_content.lines().rev().find(|l| !l.trim().is_empty()) {
                let event = detector.on_output(last);
                if let Some(DetectorEvent::PromptDetected(ref prompt)) = event {
                    handle_prompt(
                        prompt,
                        &session,
                        &config.policy,
                        &mut detector,
                        &mut *observer,
                        config.tier2.as_ref(),
                        &buffer,
                        &mut status_bar,
                    )?;
                }
            }
        }

        // Run the tick for silence-based detection
        match detector.tick() {
            DetectorEvent::PromptDetected(ref prompt) => {
                handle_prompt(
                    prompt,
                    &session,
                    &config.policy,
                    &mut detector,
                    &mut *observer,
                    config.tier2.as_ref(),
                    &buffer,
                    &mut status_bar,
                )?;
            }
            DetectorEvent::Silence { last_line, .. } => {
                debug!(last_line = %last_line, "silence detected");
            }
            _ => {}
        }

        // Stuck detection
        if let Some(ref mut stuck) = stuck_detector {
            let session_alive = tmux::session_exists(&session);
            let (state, action) = stuck.check(session_alive);

            match action {
                StuckAction::None => {}
                StuckAction::Nudge { ref message } => {
                    warn!(state = ?state, "executor may be stuck, nudging");
                    observer.on_event(&format!("⚠ stuck: nudging executor"));
                    status_bar.force_update(StatusIndicator::Failure, "stuck — nudging")?;

                    if let Err(e) = tmux::send_keys(&session, message, true) {
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

    Ok(result)
}

/// Set up the orchestrator log pane in the tmux session.
///
/// Creates a vertical split at the bottom of the session showing `tail -f`
/// on the orchestrator log file. The executor pane stays focused (selected).
fn setup_log_pane(session: &str, log_path: &Path, height_pct: u32) -> Result<()> {
    // Ensure log file exists (tail -f needs it)
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Touch the file so tail -f can start immediately
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;

    // Calculate lines from percentage (use session height or fallback to 50)
    let lines = std::cmp::max(3, (50 * height_pct / 100) as u32);

    // Split the window to create the log pane at the bottom
    // Use -l (lines) instead of -p (percentage) for tmux compatibility
    let output = std::process::Command::new("tmux")
        .args([
            "split-window",
            "-v",
            "-l",
            &lines.to_string(),
            "-t",
            session,
            "tail",
            "-f",
            &log_path.display().to_string(),
        ])
        .output()
        .context("failed to create log pane")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(stderr = %stderr, "log pane creation failed — continuing without it");
        return Ok(()); // Non-fatal — status bar still works
    }

    // Select the executor pane (pane 0) so the user types there, not in the log pane
    let _ = std::process::Command::new("tmux")
        .args(["select-pane", "-t", &format!("{session}:.0")])
        .output();

    info!(session = session, "orchestrator log pane created");
    Ok(())
}

/// Handle a detected prompt: evaluate policy and take action.
///
/// Tier 1: pattern match → auto-answer via send-keys.
/// Tier 2: no match → call supervisor agent → inject answer or escalate to human.
fn handle_prompt(
    prompt: &crate::prompt::DetectedPrompt,
    session: &str,
    policy: &PolicyEngine,
    detector: &mut PromptDetector,
    observer: &mut dyn OrchestratorObserver,
    tier2_config: Option<&Tier2Config>,
    event_buffer: &EventBuffer,
    status_bar: &mut StatusBar,
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
            observer.on_auto_answer(prompt, response);
            status_bar.update(StatusIndicator::Action, &format!("answered: {response}"))?;

            // Inject via tmux send-keys
            tmux::send_keys(session, response, true)
                .with_context(|| format!("failed to send-keys auto-answer to '{session}'"))?;

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

                let event_summary = event_buffer.format_summary();
                let context = tier2::compose_context(
                    &event_summary,
                    prompt,
                    t2_config.system_prompt.as_deref(),
                );

                match tier2::call_supervisor(t2_config, &context) {
                    Ok(Tier2Result::Answer { response }) => {
                        info!(prompt = %prompt, response = %response, "Tier 2 answer");
                        observer.on_auto_answer(prompt, &response);
                        status_bar.update(StatusIndicator::Action, &format!("T2: {response}"))?;

                        tmux::send_keys(session, &response, true).with_context(|| {
                            format!("failed to send-keys Tier 2 answer to '{session}'")
                        })?;

                        detector.answer_injected();
                        status_bar.update(StatusIndicator::Ok, "supervising")?;
                    }
                    Ok(Tier2Result::Escalate { reason }) => {
                        info!(reason = %reason, "Tier 2 escalated to human");
                        observer.on_escalate(&format!("{prompt} (supervisor: {reason})"));
                        status_bar.force_update(StatusIndicator::NeedsInput, "NEEDS INPUT")?;
                    }
                    Ok(Tier2Result::Failed { error }) => {
                        warn!(error = %error, "Tier 2 call failed");
                        observer.on_escalate(&format!("{prompt} (supervisor failed: {error})"));
                        status_bar.force_update(StatusIndicator::NeedsInput, "NEEDS INPUT")?;
                    }
                    Err(e) => {
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
    use std::collections::HashMap;
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

        let mut detector = PromptDetector::new(
            PromptPatterns::claude_code(),
            DetectorConfig::default(),
        );

        let (mut observer, events) = TestObserver::new();

        let prompt = crate::prompt::DetectedPrompt {
            kind: crate::prompt::PromptKind::Confirmation {
                detail: "Continue?".to_string(),
            },
            matched_text: "Continue? [y/n]".to_string(),
        };

        let buffer = EventBuffer::new(10);
        let mut status_bar = StatusBar::new(session, "test");
        handle_prompt(&prompt, session, &policy, &mut detector, &mut observer, None, &buffer, &mut status_bar).unwrap();

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
        let mut detector = PromptDetector::new(
            PromptPatterns::claude_code(),
            DetectorConfig::default(),
        );
        let (mut observer, events) = TestObserver::new();

        let prompt = crate::prompt::DetectedPrompt {
            kind: crate::prompt::PromptKind::Permission {
                detail: "unknown".to_string(),
            },
            matched_text: "Some unknown prompt".to_string(),
        };

        let buffer = EventBuffer::new(10);
        let mut status_bar = StatusBar::new(session, "test");
        handle_prompt(&prompt, session, &policy, &mut detector, &mut observer, None, &buffer, &mut status_bar).unwrap();

        let collected = events.lock().unwrap();
        assert!(
            collected.iter().any(|e| e.contains("escalate:")),
            "expected escalate event, got: {collected:?}"
        );

        tmux::kill_session(session).unwrap();
    }

    #[test]
    fn handle_prompt_tier2_with_echo() {
        // Test Tier 2 integration with echo as a mock supervisor
        let session = "batty-test-tier2";
        let _ = tmux::kill_session(session);
        tmux::create_session(session, "cat", &[], "/tmp").unwrap();
        std::thread::sleep(Duration::from_millis(200));

        let policy = PolicyEngine::new(Policy::Act, HashMap::new()); // no auto-answers
        let mut detector = PromptDetector::new(
            PromptPatterns::claude_code(),
            DetectorConfig::default(),
        );
        let (mut observer, events) = TestObserver::new();

        let tier2 = Tier2Config {
            program: "echo".to_string(),
            args: vec!["yes".to_string()],
            timeout: Duration::from_secs(5),
            system_prompt: None,
        };

        let prompt = crate::prompt::DetectedPrompt {
            kind: crate::prompt::PromptKind::Permission {
                detail: "unknown".to_string(),
            },
            matched_text: "Some unknown prompt".to_string(),
        };

        let buffer = EventBuffer::new(10);
        let mut status_bar = StatusBar::new(session, "test");
        handle_prompt(&prompt, session, &policy, &mut detector, &mut observer, Some(&tier2), &buffer, &mut status_bar).unwrap();

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
        let mut detector = PromptDetector::new(
            PromptPatterns::claude_code(),
            DetectorConfig::default(),
        );
        let (mut observer, events) = TestObserver::new();

        let prompt = crate::prompt::DetectedPrompt {
            kind: crate::prompt::PromptKind::Completion,
            matched_text: "result".to_string(),
        };

        let buffer = EventBuffer::new(10);
        let mut status_bar = StatusBar::new("fake-session", "test");
        handle_prompt(&prompt, "fake-session", &policy, &mut detector, &mut observer, None, &buffer, &mut status_bar).unwrap();

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
            project_root: tmp.path().to_path_buf(),
            poll_interval: Duration::from_millis(100),
            buffer_size: 50,
            tier2: None,
            log_pane: false, // don't create log pane in tests
            log_pane_height_pct: 20,
            stuck: None,
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
            project_root: tmp.path().to_path_buf(),
            poll_interval: Duration::from_millis(100),
            buffer_size: 50,
            tier2: None,
            log_pane: false, // don't create log pane in tests
            log_pane_height_pct: 20,
            stuck: None,
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

        setup_log_pane(session, &log_path, 20).unwrap();

        // Should now have 2 panes
        let panes = tmux::list_panes(session).unwrap();
        assert_eq!(panes.len(), 2, "expected 2 panes (executor + log), got {}", panes.len());

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
            project_root: tmp.path().to_path_buf(),
            poll_interval: Duration::from_millis(100),
            buffer_size: 50,
            tier2: None,
            log_pane: true,
            log_pane_height_pct: 20,
            stuck: None,
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
    fn status_bar_init_and_update() {
        let session = "batty-test-statusbar";
        let _ = tmux::kill_session(session);

        tmux::create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        let mut bar = StatusBar::new(session, "phase-2");
        bar.init().unwrap();

        // Update with various indicators
        bar.force_update(StatusIndicator::Ok, "supervising").unwrap();
        bar.force_update(StatusIndicator::Action, "answered: y").unwrap();
        bar.force_update(StatusIndicator::NeedsInput, "NEEDS INPUT").unwrap();

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
        bar.force_update(StatusIndicator::Failure, "forced").unwrap();

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
        let sd = StuckDetector::new(StuckConfig {
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
        let sd = StuckDetector::new(StuckConfig {
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
        let sd = StuckDetector::new(StuckConfig {
            timeout: Duration::from_secs(300),
            max_nudges: 2,
            auto_relaunch: true,
        });
        let (state, action) = sd.check(false);
        assert_eq!(state, StuckState::Crashed);
        assert_eq!(action, StuckAction::Relaunch);
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
}
