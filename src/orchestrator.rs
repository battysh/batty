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
use std::time::Duration;

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

    // 4. Set up orchestrator log pane
    let orch_log = log_dir.join("orchestrator.log");
    if config.log_pane {
        setup_log_pane(&session, &orch_log, config.log_pane_height_pct)?;
        observer.on_event("● log pane created");
    }

    // 5. Initialize components
    let buffer = EventBuffer::new(config.buffer_size);
    let mut watcher = PipeWatcher::new(&pipe_log, buffer.clone());
    let mut detector = PromptDetector::new(config.patterns, config.detector);

    info!(session = %session, "orchestrator loop starting");
    observer.on_event("● supervising");

    // 6. Supervision loop
    let mut last_line = String::new();
    let result = loop {
        if stop.load(Ordering::Relaxed) {
            observer.on_event("● stopped by signal");
            break OrchestratorResult::Detached;
        }

        // Check if session still exists
        if !tmux::session_exists(&session) {
            observer.on_event("✓ executor exited");
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
                )?;
            }
            DetectorEvent::Silence { last_line, .. } => {
                debug!(last_line = %last_line, "silence detected");
            }
            _ => {}
        }

        std::thread::sleep(config.poll_interval);
    };

    // 7. Cleanup
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

            // Inject via tmux send-keys
            tmux::send_keys(session, response, true)
                .with_context(|| format!("failed to send-keys auto-answer to '{session}'"))?;

            detector.answer_injected();
        }
        Decision::Suggest {
            ref prompt,
            ref response,
        } => {
            observer.on_suggest(prompt, response);
        }
        Decision::Escalate { ref prompt } => {
            // Tier 2: try supervisor agent before escalating to human
            if let Some(t2_config) = tier2_config {
                observer.on_event("? supervisor thinking...");

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

                        tmux::send_keys(session, &response, true).with_context(|| {
                            format!("failed to send-keys Tier 2 answer to '{session}'")
                        })?;

                        detector.answer_injected();
                    }
                    Ok(Tier2Result::Escalate { reason }) => {
                        info!(reason = %reason, "Tier 2 escalated to human");
                        observer.on_escalate(&format!("{prompt} (supervisor: {reason})"));
                    }
                    Ok(Tier2Result::Failed { error }) => {
                        warn!(error = %error, "Tier 2 call failed");
                        observer.on_escalate(&format!("{prompt} (supervisor failed: {error})"));
                    }
                    Err(e) => {
                        warn!(error = %e, "Tier 2 error");
                        observer.on_escalate(&format!("{prompt} (supervisor error)"));
                    }
                }
            } else {
                // No Tier 2 configured — escalate directly
                observer.on_escalate(prompt);
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
        handle_prompt(&prompt, session, &policy, &mut detector, &mut observer, None, &buffer).unwrap();

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
        handle_prompt(&prompt, session, &policy, &mut detector, &mut observer, None, &buffer).unwrap();

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
        handle_prompt(&prompt, session, &policy, &mut detector, &mut observer, Some(&tier2), &buffer).unwrap();

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
        handle_prompt(&prompt, "fake-session", &policy, &mut detector, &mut observer, None, &buffer).unwrap();

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
}
