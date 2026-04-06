//! The shim process: owns a PTY, runs an agent CLI, classifies state,
//! communicates with the orchestrator via a Channel on fd 3.
//!
//! Raw PTY output is also streamed to a log file so tmux display panes can
//! `tail -F` it and render agent output in real time.

use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use std::time::Instant;

use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, PtySize};

use super::classifier::{self, AgentType, ScreenVerdict};
use super::common::{self, QueuedMessage};
use super::protocol::{Channel, Command, Event, ShimState};
use super::pty_log::PtyLogWriter;
use crate::prompt::strip_ansi;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

const DEFAULT_ROWS: u16 = 50;
const DEFAULT_COLS: u16 = 220;
const SCROLLBACK_LINES: usize = 5000;

/// How often to check for state changes when no PTY output arrives (ms).
const POLL_INTERVAL_MS: u64 = 250;

/// Minimum time to stay in Working state before allowing transition to Idle (ms).
/// Prevents false Working→Idle from the message echo appearing before the agent
/// starts processing. Kept short (300ms) to avoid missing fast responses from
/// agents like Kiro-cli whose idle prompt disappears quickly during processing.
const WORKING_DWELL_MS: u64 = 300;

/// Additional quiet period required before Kiro is considered Idle.
/// Kiro can redraw its idle prompt before the final response bytes land.
const KIRO_IDLE_SETTLE_MS: u64 = 1200;

/// Max time to wait for agent to show its first prompt (secs).
const READY_TIMEOUT_SECS: u64 = 120;
use common::MAX_QUEUE_DEPTH;
use common::SESSION_STATS_INTERVAL_SECS;

const PROCESS_EXIT_POLL_MS: u64 = 100;
const PARENT_DEATH_POLL_SECS: u64 = 1;
const GROUP_TERM_GRACE_SECS: u64 = 2;
pub(crate) const HANDOFF_FILE_NAME: &str = "handoff.md";
const AUTO_COMMIT_MESSAGE: &str = "wip: auto-save before restart [batty]";
const AUTO_COMMIT_TIMEOUT_SECS: u64 = 5;

/// Capture a work summary (git diff + recent commits) and write it to
/// a handoff file in the given worktree. Called before an agent restart
/// so the new session can pick up where the old one left off.
pub(crate) fn preserve_handoff(
    worktree: &Path,
    task: &crate::task::Task,
    recent_output: Option<&str>,
) -> Result<()> {
    let changed_files = summarize_changed_files(worktree);
    let recent_commits = git_capture(worktree, &["log", "--oneline", "-5"]).unwrap_or_default();
    let tests_run = recent_output
        .map(extract_test_commands)
        .unwrap_or_default()
        .join("\n");
    let recent_activity = recent_output
        .map(summarize_recent_activity)
        .unwrap_or_default();

    let handoff = format!(
        "# Carry-Forward Summary\n## Task Spec\nTask #{}: {}\n\n{}\n\n## Work Completed So Far\n### Changed Files\n{}\n\n### Tests Run\n{}\n\n### Recent Activity\n{}\n\n### Recent Commits\n{}\n\n## What Remains\n{}\n",
        task.id,
        task.title,
        empty_section_fallback(&task.description),
        empty_section_fallback(&changed_files),
        empty_section_fallback(&tests_run),
        empty_section_fallback(&recent_activity),
        empty_section_fallback(&recent_commits),
        handoff_remaining_work(task)
    );
    fs::write(worktree.join(HANDOFF_FILE_NAME), handoff)
        .with_context(|| format!("failed to write handoff file in {}", worktree.display()))?;
    Ok(())
}

fn git_capture(worktree: &Path, args: &[&str]) -> Result<String> {
    let output = ProcessCommand::new("git")
        .args(args)
        .current_dir(worktree)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .with_context(|| {
            format!(
                "failed to run `git {}` in {}",
                args.join(" "),
                worktree.display()
            )
        })?;
    if !output.status.success() {
        anyhow::bail!(
            "`git {}` failed in {}: {}",
            args.join(" "),
            worktree.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn empty_section_fallback(content: &str) -> &str {
    if content.trim().is_empty() {
        "(none)"
    } else {
        content
    }
}

fn summarize_recent_activity(output: &str) -> String {
    let cleaned = strip_ansi(output);
    let lines: Vec<&str> = cleaned
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect();
    let start = lines.len().saturating_sub(40);
    lines[start..].join("\n")
}

fn summarize_changed_files(worktree: &Path) -> String {
    let mut files = Vec::new();
    for args in [
        &["diff", "--name-only"] as &[&str],
        &["diff", "--cached", "--name-only"],
        &["ls-files", "--others", "--exclude-standard"],
    ] {
        let Ok(output) = git_capture(worktree, args) else {
            continue;
        };
        for line in output.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() && !files.iter().any(|existing| existing == trimmed) {
                files.push(trimmed.to_string());
            }
        }
    }
    files.join("\n")
}

fn handoff_remaining_work(task: &crate::task::Task) -> &str {
    task.next_action
        .as_deref()
        .filter(|next| !next.trim().is_empty())
        .unwrap_or("Continue from the current worktree state, verify acceptance criteria, and finish the task without redoing completed work.")
}

fn extract_test_commands(output: &str) -> Vec<String> {
    let cleaned = strip_ansi(output);
    let mut commands = Vec::new();

    for line in cleaned.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();
        if lower.contains("cargo test")
            || lower.contains("cargo nextest")
            || lower.contains("pytest")
            || lower.contains("npm test")
            || lower.contains("pnpm test")
            || lower.contains("yarn test")
            || lower.contains("go test")
            || lower.contains("bundle exec rspec")
            || lower.contains("mix test")
        {
            if !commands.iter().any(|existing| existing == trimmed) {
                commands.push(trimmed.to_string());
            }
        }
    }

    commands
}

fn format_injected_message(sender: &str, body: &str) -> String {
    common::format_injected_message(sender, body)
}

fn shell_single_quote(input: &str) -> String {
    input.replace('\'', "'\\''")
}

fn build_supervised_agent_command(command: &str, shim_pid: u32) -> String {
    let escaped_command = shell_single_quote(command);
    format!(
        "shim_pid={shim_pid}; \
         agent_root_pid=$$; \
         agent_pgid=$$; \
         setsid sh -c ' \
           shim_pid=\"$1\"; \
           agent_pgid=\"$2\"; \
           agent_root_pid=\"$3\"; \
           collect_descendants() {{ \
             parent_pid=\"$1\"; \
             for child_pid in $(pgrep -P \"$parent_pid\" 2>/dev/null); do \
               printf \"%s\\n\" \"$child_pid\"; \
               collect_descendants \"$child_pid\"; \
             done; \
           }}; \
           while kill -0 \"$shim_pid\" 2>/dev/null; do sleep {PARENT_DEATH_POLL_SECS}; done; \
           descendant_pids=$(collect_descendants \"$agent_root_pid\"); \
           kill -TERM -- -\"$agent_pgid\" >/dev/null 2>&1 || true; \
           for descendant_pid in $descendant_pids; do kill -TERM \"$descendant_pid\" >/dev/null 2>&1 || true; done; \
           sleep {GROUP_TERM_GRACE_SECS}; \
           kill -KILL -- -\"$agent_pgid\" >/dev/null 2>&1 || true; \
           for descendant_pid in $descendant_pids; do kill -KILL \"$descendant_pid\" >/dev/null 2>&1 || true; done \
         ' _ \"$shim_pid\" \"$agent_pgid\" \"$agent_root_pid\" >/dev/null 2>&1 < /dev/null & \
         exec bash -lc '{escaped_command}'"
    )
}

#[cfg(unix)]
fn signal_process_group(child: &dyn Child, signal: libc::c_int) -> std::io::Result<()> {
    let pid = child
        .process_id()
        .ok_or_else(|| std::io::Error::other("child process id unavailable"))?;
    let result = unsafe { libc::killpg(pid as libc::pid_t, signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn terminate_agent_group(
    child: &mut Box<dyn Child + Send + Sync>,
    sigterm_grace: Duration,
) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        signal_process_group(child.as_ref(), libc::SIGTERM)?;
        let deadline = Instant::now() + sigterm_grace;
        while Instant::now() <= deadline {
            if let Ok(Some(_)) = child.try_wait() {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(PROCESS_EXIT_POLL_MS));
        }

        signal_process_group(child.as_ref(), libc::SIGKILL)?;
        return Ok(());
    }

    #[allow(unreachable_code)]
    child.kill()
}

fn graceful_shutdown_timeout() -> Duration {
    let secs = std::env::var("BATTY_GRACEFUL_SHUTDOWN_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(AUTO_COMMIT_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

fn auto_commit_on_restart_enabled() -> bool {
    std::env::var("BATTY_AUTO_COMMIT_ON_RESTART")
        .map(|value| !matches!(value.as_str(), "0" | "false" | "FALSE"))
        .unwrap_or(true)
}

fn preserve_work_before_kill_with<F>(
    worktree_path: &Path,
    timeout: Duration,
    enabled: bool,
    commit_fn: F,
) -> Result<bool>
where
    F: FnOnce(PathBuf) -> Result<bool> + Send + 'static,
{
    if !enabled {
        return Ok(false);
    }

    let (tx, rx) = mpsc::channel();
    let path = worktree_path.to_path_buf();
    thread::spawn(move || {
        let _ = tx.send(commit_fn(path));
    });

    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Ok(false),
        Err(mpsc::RecvTimeoutError::Disconnected) => Ok(false),
    }
}

pub(crate) fn preserve_work_before_kill(worktree_path: &Path) -> Result<bool> {
    let timeout = graceful_shutdown_timeout();
    preserve_work_before_kill_with(
        worktree_path,
        timeout,
        auto_commit_on_restart_enabled(),
        move |path| {
            crate::team::git_cmd::auto_commit_if_dirty(&path, AUTO_COMMIT_MESSAGE, timeout)
                .map_err(anyhow::Error::from)
        },
    )
}

/// Write body bytes to the PTY in small chunks with micro-delays, then
/// send the Enter sequence. This prevents TUI agents with synchronized
/// output from losing characters during screen redraw cycles.
fn pty_write_paced(
    pty_writer: &Arc<Mutex<Box<dyn std::io::Write + Send>>>,
    agent_type: AgentType,
    body: &[u8],
    enter: &[u8],
) -> std::io::Result<()> {
    // Use bracketed paste for TUI agents (Claude, Kiro, Codex).
    // This is the standard terminal protocol for pasting text — the agent
    // receives the complete body atomically between \x1b[200~ and \x1b[201~
    // markers, then we send Enter to submit.
    // Character-by-character injection loses keystrokes in TUI agents that
    // use synchronized output, causing "pasted text" indicators without
    // the Enter being processed.
    match agent_type {
        AgentType::Generic => {
            // Generic/bash: write directly, no paste mode needed
            let mut writer = pty_writer.lock().unwrap();
            writer.write_all(body)?;
            writer.write_all(enter)?;
            writer.flush()?;
        }
        _ => {
            // TUI agents: bracketed paste + pause + Enter
            let mut writer = pty_writer.lock().unwrap();
            writer.write_all(b"\x1b[200~")?;
            writer.write_all(body)?;
            writer.write_all(b"\x1b[201~")?;
            writer.flush()?;
            drop(writer);

            // Pause to let the TUI process the paste before sending Enter
            std::thread::sleep(std::time::Duration::from_millis(200));

            let mut writer = pty_writer.lock().unwrap();
            writer.write_all(enter)?;
            writer.flush()?;
        }
    }
    Ok(())
}

/// Returns the Enter key sequence for the given agent type.
/// Most TUI agents run in raw mode and need \r (CR) for Enter.
/// Generic/bash uses canonical mode and needs \n (LF).
fn enter_seq(agent_type: AgentType) -> &'static str {
    match agent_type {
        AgentType::Generic => "\n",
        _ => "\r", // Claude, Codex, Kiro — raw-mode TUIs
    }
}

// ---------------------------------------------------------------------------
// Args (parsed from CLI in main.rs, passed here)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ShimArgs {
    pub id: String,
    pub agent_type: AgentType,
    pub cmd: String,
    pub cwd: PathBuf,
    pub rows: u16,
    pub cols: u16,
    /// Optional path for the PTY log file. When set, raw PTY output is
    /// streamed to this file so tmux display panes can `tail -F` it.
    pub pty_log_path: Option<PathBuf>,
    pub graceful_shutdown_timeout_secs: u64,
    pub auto_commit_on_restart: bool,
}

impl ShimArgs {
    fn preserve_work_before_kill(&self, worktree_path: &Path) -> Result<bool> {
        if !self.auto_commit_on_restart {
            return Ok(false);
        }

        let status = ProcessCommand::new("git")
            .arg("-C")
            .arg(worktree_path)
            .args(["status", "--porcelain"])
            .output()
            .with_context(|| {
                format!(
                    "failed to inspect git status in {}",
                    worktree_path.display()
                )
            })?;
        if !status.status.success() {
            anyhow::bail!("git status failed in {}", worktree_path.display());
        }

        let dirty = String::from_utf8_lossy(&status.stdout)
            .lines()
            .any(|line| !line.starts_with("?? .batty/"));
        if !dirty {
            return Ok(false);
        }

        let timeout = Duration::from_secs(self.graceful_shutdown_timeout_secs);
        run_git_preserve_with_timeout(worktree_path, &["add", "-A"], timeout)?;
        run_git_preserve_with_timeout(
            worktree_path,
            &["commit", "-m", "wip: auto-save before restart [batty]"],
            timeout,
        )?;
        Ok(true)
    }
}

fn run_git_preserve_with_timeout(
    worktree_path: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<()> {
    let mut child = ProcessCommand::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(args)
        .spawn()
        .with_context(|| {
            format!(
                "failed to launch `git {}` in {}",
                args.join(" "),
                worktree_path.display()
            )
        })?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(());
            }
            anyhow::bail!(
                "`git {}` failed in {} with status {}",
                args.join(" "),
                worktree_path.display(),
                status
            );
        }

        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!(
                "`git {}` timed out after {}s in {}",
                args.join(" "),
                timeout.as_secs(),
                worktree_path.display()
            );
        }

        thread::sleep(Duration::from_millis(50));
    }
}

// QueuedMessage is imported from super::common

// ---------------------------------------------------------------------------
// Shared state between PTY reader thread and command handler thread
// ---------------------------------------------------------------------------

struct ShimInner {
    parser: vt100::Parser,
    state: ShimState,
    state_changed_at: Instant,
    last_screen_hash: u64,
    last_pty_output_at: Instant,
    started_at: Instant,
    cumulative_output_bytes: u64,
    pre_injection_content: String,
    pending_message_id: Option<String>,
    agent_type: AgentType,
    /// Messages queued while the agent is in Working state.
    /// Drained FIFO on Working→Idle transitions.
    message_queue: VecDeque<QueuedMessage>,
    /// Number of dialogs auto-dismissed during startup (capped to prevent loops).
    dialogs_dismissed: u8,
    /// Last screen content captured while the agent was in Working state.
    /// Used for response extraction when TUI agents redraw the screen
    /// before the Working→Idle transition is detected.
    last_working_screen: String,
}

impl ShimInner {
    fn screen_contents(&self) -> String {
        self.parser.screen().contents()
    }

    fn last_n_lines(&self, n: usize) -> String {
        let content = self.parser.screen().contents();
        let lines: Vec<&str> = content.lines().collect();
        let start = lines.len().saturating_sub(n);
        lines[start..].join("\n")
    }

    fn cursor_position(&self) -> (u16, u16) {
        self.parser.screen().cursor_position()
    }
}

// ---------------------------------------------------------------------------
// FNV-1a hash for change detection
// ---------------------------------------------------------------------------

fn content_hash(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in s.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

// ---------------------------------------------------------------------------
// Main shim entry point
// ---------------------------------------------------------------------------

/// Run the shim. This function does not return until the shim exits.
/// `channel` is the pre-connected socket to the orchestrator (fd 3 or
/// from a socketpair).
pub fn run(args: ShimArgs, channel: Channel) -> Result<()> {
    let rows = if args.rows > 0 {
        args.rows
    } else {
        DEFAULT_ROWS
    };
    let cols = if args.cols > 0 {
        args.cols
    } else {
        DEFAULT_COLS
    };

    // -- Create PTY --
    let pty_system = portable_pty::native_pty_system();
    let pty_pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("failed to create PTY")?;

    // -- Spawn agent CLI on slave side --
    let shim_pid = std::process::id();
    let supervised_cmd = build_supervised_agent_command(&args.cmd, shim_pid);

    let mut cmd = CommandBuilder::new("bash");
    cmd.args(["-lc", &supervised_cmd]);
    cmd.cwd(&args.cwd);
    cmd.env_remove("CLAUDECODE"); // prevent nested detection
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");

    let mut child = pty_pair
        .slave
        .spawn_command(cmd)
        .context("failed to spawn agent CLI")?;

    // Close slave in parent (agent has its own copy)
    drop(pty_pair.slave);

    let mut pty_reader = pty_pair
        .master
        .try_clone_reader()
        .context("failed to clone PTY reader")?;

    let pty_writer = pty_pair
        .master
        .take_writer()
        .context("failed to take PTY writer")?;

    // -- Shared state --
    let inner = Arc::new(Mutex::new(ShimInner {
        parser: vt100::Parser::new(rows, cols, SCROLLBACK_LINES),
        state: ShimState::Starting,
        state_changed_at: Instant::now(),
        last_screen_hash: 0,
        last_pty_output_at: Instant::now(),
        started_at: Instant::now(),
        cumulative_output_bytes: 0,
        pre_injection_content: String::new(),
        pending_message_id: None,
        agent_type: args.agent_type,
        message_queue: VecDeque::new(),
        dialogs_dismissed: 0,
        last_working_screen: String::new(),
    }));

    // -- PTY log writer (optional) --
    let pty_log: Option<Mutex<PtyLogWriter>> = args
        .pty_log_path
        .as_deref()
        .map(|p| PtyLogWriter::new(p).context("failed to create PTY log"))
        .transpose()?
        .map(Mutex::new);
    let pty_log = pty_log.map(Arc::new);

    // Wrap PTY writer in Arc<Mutex> so both threads can write
    let pty_writer = Arc::new(Mutex::new(pty_writer));

    // Channel for sending events (cloned for PTY reader thread)
    let mut cmd_channel = channel;
    let mut evt_channel = cmd_channel.try_clone().context("failed to clone channel")?;

    // -- PTY reader thread: reads agent output, feeds vt100, detects state --
    let inner_pty = Arc::clone(&inner);
    let log_handle = pty_log.clone();
    let pty_writer_pty = Arc::clone(&pty_writer);
    let pty_handle = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match pty_reader.read(&mut buf) {
                Ok(0) => break, // EOF — agent closed PTY
                Ok(n) => {
                    // Stream raw bytes to PTY log for tmux display panes
                    if let Some(ref log) = log_handle {
                        let _ = log.lock().unwrap().write(&buf[..n]);
                    }

                    let mut inner = inner_pty.lock().unwrap();
                    inner.last_pty_output_at = Instant::now();
                    inner.cumulative_output_bytes =
                        inner.cumulative_output_bytes.saturating_add(n as u64);
                    inner.parser.process(&buf[..n]);

                    // Classify when the screen content actually changes.
                    // The content hash avoids redundant classifications —
                    // no time-based debounce because it causes the PTY
                    // reader to block on the next read and miss state
                    // transitions when the prompt arrives shortly after
                    // preceding output.
                    let content = inner.parser.screen().contents();
                    let hash = content_hash(&content);
                    if hash == inner.last_screen_hash {
                        continue; // no visual change
                    }
                    inner.last_screen_hash = hash;

                    let verdict = classifier::classify(inner.agent_type, inner.parser.screen());
                    let old_state = inner.state;

                    // Track screen content during Working state for response
                    // extraction. TUI agents may redraw the screen before the
                    // Working→Idle transition, wiping the response content.
                    if old_state == ShimState::Working {
                        inner.last_working_screen = content.clone();
                    }

                    // Enforce minimum dwell time in Working state to avoid
                    // false Working→Idle from the message echo before the
                    // agent starts processing.
                    let working_too_short = old_state == ShimState::Working
                        && inner.state_changed_at.elapsed().as_millis() < WORKING_DWELL_MS as u128;
                    let new_state = match (old_state, verdict) {
                        (ShimState::Starting, ScreenVerdict::AgentIdle) => Some(ShimState::Idle),
                        (ShimState::Idle, ScreenVerdict::AgentIdle) => None,
                        (ShimState::Working, ScreenVerdict::AgentIdle) if working_too_short => None,
                        (ShimState::Working, ScreenVerdict::AgentIdle)
                            if inner.agent_type == AgentType::Kiro =>
                        {
                            None
                        }
                        (ShimState::Working, ScreenVerdict::AgentIdle) => Some(ShimState::Idle),
                        (ShimState::Working, ScreenVerdict::AgentWorking) => None,
                        (_, ScreenVerdict::ContextExhausted) => Some(ShimState::ContextExhausted),
                        (_, ScreenVerdict::Unknown) => None,
                        (ShimState::Idle, ScreenVerdict::AgentWorking) => Some(ShimState::Working),
                        (ShimState::Starting, ScreenVerdict::AgentWorking) => {
                            Some(ShimState::Working)
                        }
                        _ => None,
                    };

                    if let Some(new) = new_state {
                        let summary = inner.last_n_lines(5);
                        inner.state = new;
                        inner.state_changed_at = Instant::now();

                        let pre_content = inner.pre_injection_content.clone();
                        let current_content = inner.screen_contents();
                        let working_screen = inner.last_working_screen.clone();
                        let msg_id = inner.pending_message_id.take();

                        // On terminal states, drain the queue
                        let drain_errors =
                            if new == ShimState::Dead || new == ShimState::ContextExhausted {
                                drain_queue_errors(&mut inner.message_queue, new)
                            } else {
                                Vec::new()
                            };

                        // On Working→Idle, check for queued messages to inject
                        let queued_msg = if old_state == ShimState::Working
                            && new == ShimState::Idle
                            && !inner.message_queue.is_empty()
                        {
                            inner.message_queue.pop_front()
                        } else {
                            None
                        };

                        // If we're injecting a queued message, stay in Working
                        if let Some(ref msg) = queued_msg {
                            inner.pre_injection_content = inner.screen_contents();
                            inner.pending_message_id = msg.message_id.clone();
                            inner.state = ShimState::Working;
                            inner.state_changed_at = Instant::now();
                        }

                        let queue_depth = inner.message_queue.len();
                        let agent_type_for_enter = inner.agent_type;
                        let queued_injected = queued_msg
                            .as_ref()
                            .map(|msg| format_injected_message(&msg.from, &msg.body));

                        drop(inner); // release lock before I/O

                        let events = build_transition_events(
                            old_state,
                            new,
                            &summary,
                            &pre_content,
                            &current_content,
                            &working_screen,
                            msg_id,
                        );

                        for event in events {
                            if evt_channel.send(&event).is_err() {
                                return; // orchestrator disconnected
                            }
                        }

                        // Send drain errors for terminal states
                        for event in drain_errors {
                            if evt_channel.send(&event).is_err() {
                                return;
                            }
                        }

                        // Inject queued message into PTY
                        if let Some(msg) = queued_msg {
                            let enter = enter_seq(agent_type_for_enter);
                            let injected = queued_injected.as_deref().unwrap_or(msg.body.as_str());
                            if let Err(e) = pty_write_paced(
                                &pty_writer_pty,
                                agent_type_for_enter,
                                injected.as_bytes(),
                                enter.as_bytes(),
                            ) {
                                let _ = evt_channel.send(&Event::Error {
                                    command: "SendMessage".into(),
                                    reason: format!("PTY write failed for queued message: {e}"),
                                });
                            }

                            // Emit StateChanged Idle→Working for the queued message
                            let _ = evt_channel.send(&Event::StateChanged {
                                from: ShimState::Idle,
                                to: ShimState::Working,
                                summary: format!(
                                    "delivering queued message ({} remaining)",
                                    queue_depth
                                ),
                            });
                        }
                    }
                }
                Err(_) => break, // PTY error — agent likely exited
            }
        }

        // Agent PTY closed — mark as dead
        let mut inner = inner_pty.lock().unwrap();
        let last_lines = inner.last_n_lines(10);
        let old = inner.state;
        inner.state = ShimState::Dead;

        // Drain any remaining queued messages
        let drain_errors = drain_queue_errors(&mut inner.message_queue, ShimState::Dead);
        drop(inner);

        let _ = evt_channel.send(&Event::StateChanged {
            from: old,
            to: ShimState::Dead,
            summary: last_lines.clone(),
        });

        let _ = evt_channel.send(&Event::Died {
            exit_code: None,
            last_lines,
        });

        for event in drain_errors {
            let _ = evt_channel.send(&event);
        }
    });

    // Kiro can repaint its idle prompt before its final response bytes land.
    // Poll for a stable idle screen after PTY output has been quiet for long
    // enough, then emit the Working -> Idle completion transition.
    let inner_idle = Arc::clone(&inner);
    let pty_writer_idle = Arc::clone(&pty_writer);
    let mut idle_channel = cmd_channel.try_clone().context("failed to clone channel")?;
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));

            let mut inner = inner_idle.lock().unwrap();
            if inner.agent_type != AgentType::Kiro || inner.state != ShimState::Working {
                continue;
            }
            if inner.last_pty_output_at.elapsed().as_millis() < KIRO_IDLE_SETTLE_MS as u128 {
                continue;
            }
            if classifier::classify(inner.agent_type, inner.parser.screen())
                != ScreenVerdict::AgentIdle
            {
                continue;
            }

            let summary = inner.last_n_lines(5);
            let pre_content = inner.pre_injection_content.clone();
            let current_content = inner.screen_contents();
            let working_screen = inner.last_working_screen.clone();
            let msg_id = inner.pending_message_id.take();

            inner.state = ShimState::Idle;
            inner.state_changed_at = Instant::now();

            let queued_msg = if !inner.message_queue.is_empty() {
                inner.message_queue.pop_front()
            } else {
                None
            };

            if let Some(ref msg) = queued_msg {
                inner.pre_injection_content = inner.screen_contents();
                inner.pending_message_id = msg.message_id.clone();
                inner.state = ShimState::Working;
                inner.state_changed_at = Instant::now();
            }

            let queue_depth = inner.message_queue.len();
            let agent_type_for_enter = inner.agent_type;
            let queued_injected = queued_msg
                .as_ref()
                .map(|msg| format_injected_message(&msg.from, &msg.body));
            drop(inner);

            for event in build_transition_events(
                ShimState::Working,
                ShimState::Idle,
                &summary,
                &pre_content,
                &current_content,
                &working_screen,
                msg_id,
            ) {
                if idle_channel.send(&event).is_err() {
                    return;
                }
            }

            if let Some(msg) = queued_msg {
                let enter = enter_seq(agent_type_for_enter);
                let injected = queued_injected.as_deref().unwrap_or(msg.body.as_str());
                if let Err(e) = pty_write_paced(
                    &pty_writer_idle,
                    agent_type_for_enter,
                    injected.as_bytes(),
                    enter.as_bytes(),
                ) {
                    let _ = idle_channel.send(&Event::Error {
                        command: "SendMessage".into(),
                        reason: format!("PTY write failed for queued message: {e}"),
                    });
                    continue;
                }

                let _ = idle_channel.send(&Event::StateChanged {
                    from: ShimState::Idle,
                    to: ShimState::Working,
                    summary: format!("delivering queued message ({} remaining)", queue_depth),
                });
            }
        }
    });

    // -- Periodic screen poll thread: re-classify even when PTY is quiet --
    // The PTY reader thread only classifies when new output arrives. If the
    // agent finishes and shows the idle prompt but produces no further output,
    // the reader blocks on read() and the state stays Working forever.
    // This thread polls the screen every 5 seconds to catch that case.
    let inner_poll = Arc::clone(&inner);
    let mut poll_channel = cmd_channel
        .try_clone()
        .context("failed to clone channel for poll thread")?;
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(5));
            let mut inner = inner_poll.lock().unwrap();
            if inner.state != ShimState::Working {
                continue;
            }
            // Only re-classify if PTY has been quiet for at least 2 seconds
            if inner.last_pty_output_at.elapsed().as_secs() < 2 {
                continue;
            }
            let verdict = classifier::classify(inner.agent_type, inner.parser.screen());
            if verdict == classifier::ScreenVerdict::AgentIdle {
                let summary = inner.last_n_lines(5);
                inner.state = ShimState::Idle;
                inner.state_changed_at = Instant::now();
                drop(inner);

                // Emit the transition — the daemon will handle message
                // queue draining and completion processing.
                let _ = poll_channel.send(&Event::StateChanged {
                    from: ShimState::Working,
                    to: ShimState::Idle,
                    summary,
                });
            }
        }
    });

    let inner_stats = Arc::clone(&inner);
    let mut stats_channel = cmd_channel
        .try_clone()
        .context("failed to clone channel for stats thread")?;
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(SESSION_STATS_INTERVAL_SECS));
            let inner = inner_stats.lock().unwrap();
            if inner.state == ShimState::Dead {
                return;
            }
            let output_bytes = inner.cumulative_output_bytes;
            let uptime_secs = inner.started_at.elapsed().as_secs();
            drop(inner);

            if stats_channel
                .send(&Event::SessionStats {
                    output_bytes,
                    uptime_secs,
                })
                .is_err()
            {
                return;
            }
        }
    });

    // -- Main thread: handle commands from orchestrator --
    let inner_cmd = Arc::clone(&inner);

    // Wait for Ready (Starting → Idle transition) with timeout.
    // During startup, auto-dismiss known dialogs (e.g., Claude's trust prompt)
    // by sending Enter (\r) to the PTY.
    let start = Instant::now();
    loop {
        let mut inner = inner_cmd.lock().unwrap();
        let state = inner.state;
        match state {
            ShimState::Starting => {
                // Auto-dismiss known startup dialogs (trust prompts, etc.)
                if inner.dialogs_dismissed < 10 {
                    let content = inner.screen_contents();
                    if classifier::detect_startup_dialog(&content) {
                        let attempt = inner.dialogs_dismissed + 1;
                        let enter = enter_seq(inner.agent_type);
                        inner.dialogs_dismissed = attempt;
                        drop(inner);
                        eprintln!(
                            "[shim {}] auto-dismissing startup dialog (attempt {attempt})",
                            args.id
                        );
                        let mut writer = pty_writer.lock().unwrap();
                        writer.write_all(enter.as_bytes()).ok();
                        writer.flush().ok();
                        std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
                        continue;
                    }
                }
                drop(inner);

                if start.elapsed().as_secs() > READY_TIMEOUT_SECS {
                    let last = inner_cmd.lock().unwrap().last_n_lines(10);
                    cmd_channel.send(&Event::Error {
                        command: "startup".into(),
                        reason: format!(
                            "agent did not show prompt within {}s. Last lines:\n{}",
                            READY_TIMEOUT_SECS, last,
                        ),
                    })?;
                    terminate_agent_group(&mut child, Duration::from_secs(GROUP_TERM_GRACE_SECS))
                        .ok();
                    return Ok(());
                }
                thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
            }
            ShimState::Dead => {
                drop(inner);
                return Ok(());
            }
            ShimState::Idle => {
                drop(inner);
                cmd_channel.send(&Event::Ready)?;
                break;
            }
            _ => {
                // Working or other transitional state during startup —
                // agent is still loading/initializing, keep waiting.
                drop(inner);
                if start.elapsed().as_secs() > READY_TIMEOUT_SECS {
                    let last = inner_cmd.lock().unwrap().last_n_lines(10);
                    cmd_channel.send(&Event::Error {
                        command: "startup".into(),
                        reason: format!(
                            "agent did not reach idle within {}s (state: {}). Last lines:\n{}",
                            READY_TIMEOUT_SECS, state, last,
                        ),
                    })?;
                    terminate_agent_group(&mut child, Duration::from_secs(GROUP_TERM_GRACE_SECS))
                        .ok();
                    return Ok(());
                }
                thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
            }
        }
    }

    // -- Command loop --
    loop {
        let cmd = match cmd_channel.recv::<Command>() {
            Ok(Some(c)) => c,
            Ok(None) => {
                eprintln!(
                    "[shim {}] orchestrator disconnected, shutting down",
                    args.id
                );
                terminate_agent_group(&mut child, Duration::from_secs(GROUP_TERM_GRACE_SECS)).ok();
                break;
            }
            Err(e) => {
                eprintln!("[shim {}] channel error: {e}", args.id);
                terminate_agent_group(&mut child, Duration::from_secs(GROUP_TERM_GRACE_SECS)).ok();
                break;
            }
        };

        match cmd {
            Command::SendMessage {
                from,
                body,
                message_id,
            } => {
                let mut inner = inner_cmd.lock().unwrap();
                match inner.state {
                    ShimState::Idle => {
                        inner.pre_injection_content = inner.screen_contents();
                        inner.pending_message_id = message_id;
                        let agent_type = inner.agent_type;
                        let enter = enter_seq(agent_type);
                        let injected = format_injected_message(&from, &body);
                        drop(inner);
                        // Write body char-by-char with micro-delays for TUI
                        // agents that use synchronized output. Bulk writes
                        // get interleaved with screen redraws, losing chars.
                        if let Err(e) = pty_write_paced(
                            &pty_writer,
                            agent_type,
                            injected.as_bytes(),
                            enter.as_bytes(),
                        ) {
                            cmd_channel.send(&Event::Error {
                                command: "SendMessage".into(),
                                reason: format!("PTY write failed: {e}"),
                            })?;
                            // Restore state on failure
                            continue;
                        }
                        let mut inner = inner_cmd.lock().unwrap();

                        let old = inner.state;
                        inner.state = ShimState::Working;
                        inner.state_changed_at = Instant::now();
                        let summary = inner.last_n_lines(3);
                        drop(inner);

                        cmd_channel.send(&Event::StateChanged {
                            from: old,
                            to: ShimState::Working,
                            summary,
                        })?;
                    }
                    ShimState::Working => {
                        // Queue the message for delivery when agent returns to Idle
                        if inner.message_queue.len() >= MAX_QUEUE_DEPTH {
                            let dropped = inner.message_queue.pop_front();
                            let dropped_id = dropped.as_ref().and_then(|m| m.message_id.clone());
                            inner.message_queue.push_back(QueuedMessage {
                                from,
                                body,
                                message_id,
                            });
                            let depth = inner.message_queue.len();
                            drop(inner);

                            cmd_channel.send(&Event::Error {
                                command: "SendMessage".into(),
                                reason: format!(
                                    "message queue full ({MAX_QUEUE_DEPTH}), dropped oldest message{}",
                                    dropped_id
                                        .map(|id| format!(" (id: {id})"))
                                        .unwrap_or_default(),
                                ),
                            })?;
                            cmd_channel.send(&Event::Warning {
                                message: format!(
                                    "message queued while agent working (depth: {depth})"
                                ),
                                idle_secs: None,
                            })?;
                        } else {
                            inner.message_queue.push_back(QueuedMessage {
                                from,
                                body,
                                message_id,
                            });
                            let depth = inner.message_queue.len();
                            drop(inner);

                            cmd_channel.send(&Event::Warning {
                                message: format!(
                                    "message queued while agent working (depth: {depth})"
                                ),
                                idle_secs: None,
                            })?;
                        }
                    }
                    other => {
                        cmd_channel.send(&Event::Error {
                            command: "SendMessage".into(),
                            reason: format!("agent in {other} state, cannot accept message"),
                        })?;
                    }
                }
            }

            Command::CaptureScreen { last_n_lines } => {
                let inner = inner_cmd.lock().unwrap();
                let content = match last_n_lines {
                    Some(n) => inner.last_n_lines(n),
                    None => inner.screen_contents(),
                };
                let (row, col) = inner.cursor_position();
                drop(inner);
                cmd_channel.send(&Event::ScreenCapture {
                    content,
                    cursor_row: row,
                    cursor_col: col,
                })?;
            }

            Command::GetState => {
                let inner = inner_cmd.lock().unwrap();
                let since = inner.state_changed_at.elapsed().as_secs();
                let state = inner.state;
                drop(inner);
                cmd_channel.send(&Event::State {
                    state,
                    since_secs: since,
                })?;
            }

            Command::Resize { rows, cols } => {
                pty_pair
                    .master
                    .resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    })
                    .ok();
                let mut inner = inner_cmd.lock().unwrap();
                inner.parser.set_size(rows, cols);
            }

            Command::Ping => {
                cmd_channel.send(&Event::Pong)?;
            }

            Command::Shutdown { timeout_secs } => {
                eprintln!(
                    "[shim {}] shutdown requested (timeout: {}s)",
                    args.id, timeout_secs
                );
                if let Err(error) = args.preserve_work_before_kill(&args.cwd) {
                    eprintln!(
                        "[shim {}] auto-save before shutdown failed: {}",
                        args.id, error
                    );
                }
                {
                    let mut writer = pty_writer.lock().unwrap();
                    writer.write_all(b"\x03").ok(); // Ctrl-C
                    writer.flush().ok();
                }
                let deadline = Instant::now() + Duration::from_secs(timeout_secs as u64);
                loop {
                    if Instant::now() > deadline {
                        terminate_agent_group(
                            &mut child,
                            Duration::from_secs(GROUP_TERM_GRACE_SECS),
                        )
                        .ok();
                        break;
                    }
                    if let Ok(Some(_)) = child.try_wait() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(PROCESS_EXIT_POLL_MS));
                }
                break;
            }

            Command::Kill => {
                if let Err(error) = args.preserve_work_before_kill(&args.cwd) {
                    eprintln!("[shim {}] auto-save before kill failed: {}", args.id, error);
                }
                terminate_agent_group(&mut child, Duration::from_secs(GROUP_TERM_GRACE_SECS)).ok();
                break;
            }
        }
    }

    pty_handle.join().ok();
    Ok(())
}

fn drain_queue_errors(
    queue: &mut VecDeque<QueuedMessage>,
    terminal_state: ShimState,
) -> Vec<Event> {
    common::drain_queue_errors(queue, terminal_state)
}

// ---------------------------------------------------------------------------
// Build events for a state transition
// ---------------------------------------------------------------------------

fn build_transition_events(
    from: ShimState,
    to: ShimState,
    summary: &str,
    pre_injection_content: &str,
    current_content: &str,
    last_working_screen: &str,
    message_id: Option<String>,
) -> Vec<Event> {
    let summary = sanitize_summary(summary);
    let mut events = vec![Event::StateChanged {
        from,
        to,
        summary: summary.clone(),
    }];

    // Working → Idle = completion, but only if a message was actually pending.
    // Skip Completion for transitions caused by agent startup/loading (e.g.,
    // MCP server init) where no user message was injected.
    if from == ShimState::Working && to == ShimState::Idle && !pre_injection_content.is_empty() {
        // Try diffing against current screen first; if empty (TUI agents
        // redraw to idle before we capture), fall back to the last screen
        // seen during Working state.
        let mut response = extract_response(pre_injection_content, current_content);
        if response.is_empty() && !last_working_screen.is_empty() {
            response = extract_response(pre_injection_content, last_working_screen);
        }
        events.push(Event::Completion {
            message_id,
            response,
            last_lines: summary.clone(),
        });
    }

    // Any → ContextExhausted
    if to == ShimState::ContextExhausted {
        events.push(Event::ContextExhausted {
            message: "Agent reported context exhaustion".to_string(),
            last_lines: summary,
        });
    }

    events
}

fn sanitize_summary(summary: &str) -> String {
    let cleaned: Vec<String> = summary
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() || is_tui_chrome(line) || is_prompt_line(trimmed) {
                return None;
            }
            Some(strip_claude_bullets(trimmed))
        })
        .collect();

    if cleaned.is_empty() {
        String::new()
    } else {
        cleaned.join("\n")
    }
}

/// Extract the agent's response by diffing pre-injection and post-completion
/// screen content. Strips known TUI chrome (horizontal rules, status bars,
/// prompt lines) so callers get clean response text.
fn extract_response(pre: &str, current: &str) -> String {
    let pre_lines: Vec<&str> = pre.lines().collect();
    let cur_lines: Vec<&str> = current.lines().collect();

    let overlap = pre_lines.len().min(cur_lines.len());
    let mut diverge_at = 0;
    for i in 0..overlap {
        if pre_lines[i] != cur_lines[i] {
            break;
        }
        diverge_at = i + 1;
    }

    let response_lines = &cur_lines[diverge_at..];
    if response_lines.is_empty() {
        return String::new();
    }

    // Filter out TUI chrome, then strip trailing empty/prompt lines
    let filtered: Vec<&str> = response_lines
        .iter()
        .filter(|line| !is_tui_chrome(line))
        .copied()
        .collect();

    if filtered.is_empty() {
        return String::new();
    }

    // Strip trailing empty lines and prompt lines
    let mut end = filtered.len();
    while end > 0 && filtered[end - 1].trim().is_empty() {
        end -= 1;
    }
    while end > 0 && is_prompt_line(filtered[end - 1].trim()) {
        end -= 1;
    }
    while end > 0 && filtered[end - 1].trim().is_empty() {
        end -= 1;
    }

    // Strip leading lines that echo the user's input (❯ followed by text)
    let mut start = 0;
    while start < end {
        let trimmed = filtered[start].trim();
        if trimmed.is_empty() {
            start += 1;
        } else if trimmed.starts_with('\u{276F}')
            && !trimmed['\u{276F}'.len_utf8()..].trim().is_empty()
        {
            // Echoed user input line
            start += 1;
        } else {
            break;
        }
    }

    // Strip Claude's output bullet markers (⏺) from the start of lines
    let cleaned: Vec<String> = filtered[start..end]
        .iter()
        .map(|line| strip_claude_bullets(line))
        .collect();

    cleaned.join("\n")
}

/// Strip Claude's ⏺ (U+23FA) output bullet marker from the start of a line.
fn strip_claude_bullets(line: &str) -> String {
    let trimmed = line.trim_start();
    if trimmed.starts_with('\u{23FA}') {
        let after = &trimmed['\u{23FA}'.len_utf8()..];
        // Preserve original leading whitespace minus the bullet
        let leading = line.len() - line.trim_start().len();
        format!("{}{}", &" ".repeat(leading), after.trim_start())
    } else {
        line.to_string()
    }
}

/// Detect TUI chrome lines that should be stripped from responses.
/// Matches horizontal rules, status bars, and other decorative elements
/// common in Claude, Codex, and Kiro TUI output.
fn is_tui_chrome(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false; // keep empty lines (stripped separately)
    }

    // Horizontal rules: lines made entirely of box-drawing characters
    if trimmed.chars().all(|c| {
        matches!(
            c,
            '─' | '━'
                | '═'
                | '╌'
                | '╍'
                | '┄'
                | '┅'
                | '╶'
                | '╴'
                | '╸'
                | '╺'
                | '│'
                | '┃'
                | '╎'
                | '╏'
                | '┊'
                | '┋'
        )
    }) {
        return true;
    }

    // Claude status bar: ⏵⏵ bypass permissions, shift+tab, model info
    if trimmed.contains("\u{23F5}\u{23F5}") || trimmed.contains("bypass permissions") {
        return true;
    }
    if trimmed.contains("shift+tab") && trimmed.len() < 80 {
        return true;
    }

    // Claude cost/token summary line
    if trimmed.starts_with('$') && trimmed.contains("token") {
        return true;
    }

    // Braille art (Kiro logo, Codex box drawings) — lines with mostly braille chars
    let braille_count = trimmed
        .chars()
        .filter(|c| ('\u{2800}'..='\u{28FF}').contains(c))
        .count();
    if braille_count > 5 {
        return true;
    }

    // Kiro welcome/status text
    let lower = trimmed.to_lowercase();
    if lower.contains("welcome to the new kiro") || lower.contains("/feedback command") {
        return true;
    }

    // Kiro status bar
    if lower.starts_with("kiro") && lower.contains('\u{25D4}') {
        // "Kiro · auto · ◔ 0%"
        return true;
    }

    // Codex welcome box
    if trimmed.starts_with('╭') || trimmed.starts_with('╰') || trimmed.starts_with('│') {
        return true;
    }

    // Codex tips/warnings
    if lower.starts_with("tip:") || (trimmed.starts_with('⚠') && lower.contains("limit")) {
        return true;
    }

    // Kiro/Codex prompt placeholders
    if lower.contains("ask a question") || lower.contains("describe a task") {
        return true;
    }

    false
}

fn is_prompt_line(line: &str) -> bool {
    line == "\u{276F}"
        || line.starts_with("\u{276F} ")
        || line == "\u{203A}"
        || line.starts_with("\u{203A} ")
        || line.ends_with("$ ")
        || line.ends_with('$')
        || line.ends_with("% ")
        || line.ends_with('%')
        || line == ">"
        || line.starts_with("Kiro>")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_response_basic() {
        let pre = "line1\nline2\n$ ";
        let cur = "line1\nline2\nhello world\n$ ";
        assert_eq!(extract_response(pre, cur), "hello world");
    }

    #[test]
    fn extract_response_multiline() {
        let pre = "$ ";
        let cur = "$ echo hi\nhi\n$ ";
        let resp = extract_response(pre, cur);
        assert!(resp.contains("echo hi"));
        assert!(resp.contains("hi"));
    }

    #[test]
    fn extract_response_empty() {
        let pre = "$ ";
        let cur = "$ ";
        assert_eq!(extract_response(pre, cur), "");
    }

    #[test]
    fn content_hash_deterministic() {
        assert_eq!(content_hash("hello"), content_hash("hello"));
        assert_ne!(content_hash("hello"), content_hash("world"));
    }

    #[test]
    fn shell_single_quote_escapes_embedded_quote() {
        assert_eq!(shell_single_quote("fix user's bug"), "fix user'\\''s bug");
    }

    #[test]
    fn supervised_command_contains_watchdog_and_exec() {
        let command = build_supervised_agent_command("kiro-cli chat 'hello'", 4242);
        assert!(command.contains("shim_pid=4242"));
        assert!(command.contains("agent_root_pid=$$"));
        assert!(command.contains("agent_pgid=$$"));
        assert!(command.contains("setsid sh -c"));
        assert!(command.contains("shim_pid=\"$1\""));
        assert!(command.contains("agent_pgid=\"$2\""));
        assert!(command.contains("agent_root_pid=\"$3\""));
        assert!(command.contains("collect_descendants()"));
        assert!(command.contains("pgrep -P \"$parent_pid\""));
        assert!(command.contains("descendant_pids=$(collect_descendants \"$agent_root_pid\")"));
        assert!(command.contains("kill -TERM -- -\"$agent_pgid\""));
        assert!(command.contains("kill -TERM \"$descendant_pid\""));
        assert!(command.contains("kill -KILL -- -\"$agent_pgid\""));
        assert!(command.contains("kill -KILL \"$descendant_pid\""));
        assert!(command.contains("' _ \"$shim_pid\" \"$agent_pgid\" \"$agent_root_pid\""));
        assert!(command.contains("exec bash -lc 'kiro-cli chat '\\''hello'\\'''"));
    }

    #[test]
    fn is_prompt_line_shell_dollar() {
        assert!(is_prompt_line("user@host:~$ "));
        assert!(is_prompt_line("$"));
    }

    #[test]
    fn is_prompt_line_claude() {
        assert!(is_prompt_line("\u{276F}"));
        assert!(is_prompt_line("\u{276F} "));
    }

    #[test]
    fn is_prompt_line_codex() {
        assert!(is_prompt_line("\u{203A}"));
        assert!(is_prompt_line("\u{203A} "));
    }

    #[test]
    fn is_prompt_line_kiro() {
        assert!(is_prompt_line("Kiro>"));
        assert!(is_prompt_line(">"));
    }

    #[test]
    fn is_prompt_line_not_prompt() {
        assert!(!is_prompt_line("hello world"));
        assert!(!is_prompt_line("some output here"));
    }

    #[test]
    fn build_transition_events_working_to_idle() {
        let events = build_transition_events(
            ShimState::Working,
            ShimState::Idle,
            "summary",
            "pre\n$ ",
            "pre\nhello\n$ ",
            "",
            Some("msg-1".into()),
        );
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], Event::StateChanged { .. }));
        assert!(matches!(&events[1], Event::Completion { .. }));
    }

    #[test]
    fn build_transition_events_to_context_exhausted() {
        let events = build_transition_events(
            ShimState::Working,
            ShimState::ContextExhausted,
            "summary",
            "",
            "",
            "",
            None,
        );
        // StateChanged + ContextExhausted (no Completion since it's not Idle)
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[1], Event::ContextExhausted { .. }));
    }

    #[test]
    fn build_transition_events_starting_to_idle() {
        let events = build_transition_events(
            ShimState::Starting,
            ShimState::Idle,
            "summary",
            "",
            "",
            "",
            None,
        );
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], Event::StateChanged { .. }));
    }

    // -----------------------------------------------------------------------
    // Message queue tests
    // -----------------------------------------------------------------------

    fn make_queued_msg(id: &str, body: &str) -> QueuedMessage {
        QueuedMessage {
            from: "user".into(),
            body: body.into(),
            message_id: Some(id.into()),
        }
    }

    #[test]
    fn queue_enqueue_basic() {
        let mut queue: VecDeque<QueuedMessage> = VecDeque::new();
        queue.push_back(make_queued_msg("m1", "hello"));
        queue.push_back(make_queued_msg("m2", "world"));
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn queue_fifo_order() {
        let mut queue: VecDeque<QueuedMessage> = VecDeque::new();
        queue.push_back(make_queued_msg("m1", "first"));
        queue.push_back(make_queued_msg("m2", "second"));
        queue.push_back(make_queued_msg("m3", "third"));

        let msg = queue.pop_front().unwrap();
        assert_eq!(msg.message_id.as_deref(), Some("m1"));
        assert_eq!(msg.body, "first");

        let msg = queue.pop_front().unwrap();
        assert_eq!(msg.message_id.as_deref(), Some("m2"));
        assert_eq!(msg.body, "second");

        let msg = queue.pop_front().unwrap();
        assert_eq!(msg.message_id.as_deref(), Some("m3"));
        assert_eq!(msg.body, "third");

        assert!(queue.is_empty());
    }

    #[test]
    fn queue_overflow_drops_oldest() {
        let mut queue: VecDeque<QueuedMessage> = VecDeque::new();

        // Fill to MAX_QUEUE_DEPTH
        for i in 0..MAX_QUEUE_DEPTH {
            queue.push_back(make_queued_msg(&format!("m{i}"), &format!("msg {i}")));
        }
        assert_eq!(queue.len(), MAX_QUEUE_DEPTH);

        // Overflow: drop oldest, add new
        assert!(queue.len() >= MAX_QUEUE_DEPTH);
        let dropped = queue.pop_front().unwrap();
        assert_eq!(dropped.message_id.as_deref(), Some("m0")); // oldest dropped
        queue.push_back(make_queued_msg("m_new", "new message"));
        assert_eq!(queue.len(), MAX_QUEUE_DEPTH);

        // First item should now be m1 (m0 was dropped)
        let first = queue.pop_front().unwrap();
        assert_eq!(first.message_id.as_deref(), Some("m1"));
    }

    #[test]
    fn drain_queue_errors_empty() {
        let mut queue: VecDeque<QueuedMessage> = VecDeque::new();
        let events = drain_queue_errors(&mut queue, ShimState::Dead);
        assert!(events.is_empty());
    }

    #[test]
    fn drain_queue_errors_with_messages() {
        let mut queue: VecDeque<QueuedMessage> = VecDeque::new();
        queue.push_back(make_queued_msg("m1", "hello"));
        queue.push_back(make_queued_msg("m2", "world"));
        queue.push_back(QueuedMessage {
            from: "user".into(),
            body: "no id".into(),
            message_id: None,
        });

        let events = drain_queue_errors(&mut queue, ShimState::Dead);
        assert_eq!(events.len(), 3);
        assert!(queue.is_empty());

        // All should be Error events
        for event in &events {
            assert!(matches!(event, Event::Error { .. }));
        }

        // First error should mention the message_id
        if let Event::Error { reason, .. } = &events[0] {
            assert!(reason.contains("dead"));
            assert!(reason.contains("m1"));
        }

        // Third error (no message_id) should not contain "(id:"
        if let Event::Error { reason, .. } = &events[2] {
            assert!(!reason.contains("(id:"));
        }
    }

    #[test]
    fn drain_queue_errors_context_exhausted() {
        let mut queue: VecDeque<QueuedMessage> = VecDeque::new();
        queue.push_back(make_queued_msg("m1", "hello"));

        let events = drain_queue_errors(&mut queue, ShimState::ContextExhausted);
        assert_eq!(events.len(), 1);
        if let Event::Error { reason, .. } = &events[0] {
            assert!(reason.contains("context_exhausted"));
        }
    }

    #[test]
    fn queued_message_preserves_fields() {
        let msg = QueuedMessage {
            from: "manager".into(),
            body: "do this task".into(),
            message_id: Some("msg-42".into()),
        };
        assert_eq!(msg.from, "manager");
        assert_eq!(msg.body, "do this task");
        assert_eq!(msg.message_id.as_deref(), Some("msg-42"));
    }

    #[test]
    fn queued_message_none_id() {
        let msg = QueuedMessage {
            from: "user".into(),
            body: "anonymous".into(),
            message_id: None,
        };
        assert!(msg.message_id.is_none());
    }

    #[test]
    fn max_queue_depth_is_16() {
        assert_eq!(MAX_QUEUE_DEPTH, 16);
    }

    #[test]
    fn format_injected_message_includes_sender_and_reply_target() {
        let formatted = format_injected_message("human", "what is 2+2?");
        assert!(formatted.contains("--- Message from human ---"));
        assert!(formatted.contains("Reply-To: human"));
        assert!(formatted.contains("batty send human"));
        assert!(formatted.ends_with("what is 2+2?"));
    }

    #[test]
    fn format_injected_message_uses_sender_as_reply_target() {
        let formatted = format_injected_message("manager", "status?");
        assert!(formatted.contains("Reply-To: manager"));
        assert!(formatted.contains("batty send manager"));
    }

    #[test]
    fn sanitize_summary_strips_tui_chrome_and_prompt_lines() {
        let summary = "────────────────────\n❯ \n  ⏵⏵ bypass permissions on\nThe answer is 4\n";
        assert_eq!(sanitize_summary(summary), "The answer is 4");
    }

    #[test]
    fn sanitize_summary_keeps_multiline_meaningful_content() {
        let summary = "  Root cause: stale resume id\n\n  Fix: retry with fresh start\n";
        assert_eq!(
            sanitize_summary(summary),
            "Root cause: stale resume id\nFix: retry with fresh start"
        );
    }

    // -----------------------------------------------------------------------
    // TUI chrome stripping tests
    // -----------------------------------------------------------------------

    #[test]
    fn is_tui_chrome_horizontal_rule() {
        assert!(is_tui_chrome("────────────────────────────────────"));
        assert!(is_tui_chrome("  ─────────  "));
        assert!(is_tui_chrome("━━━━━━━━━━━━━━━━━━━━"));
    }

    #[test]
    fn is_tui_chrome_status_bar() {
        assert!(is_tui_chrome(
            "  \u{23F5}\u{23F5} bypass permissions on (shift+tab to toggle)"
        ));
        assert!(is_tui_chrome("  bypass permissions on"));
        assert!(is_tui_chrome("  shift+tab"));
    }

    #[test]
    fn is_tui_chrome_cost_line() {
        assert!(is_tui_chrome("$0.01 · 2.3k tokens"));
    }

    #[test]
    fn is_tui_chrome_not_content() {
        assert!(!is_tui_chrome("Hello, world!"));
        assert!(!is_tui_chrome("The answer is 4"));
        assert!(!is_tui_chrome("")); // empty lines are not chrome
        assert!(!is_tui_chrome("  some output  "));
    }

    #[test]
    fn extract_response_strips_chrome() {
        let pre = "idle screen\n\u{276F} ";
        let cur = "\u{276F} Hello\n\nThe answer is 42\n\n\
                   ────────────────────\n\
                   \u{23F5}\u{23F5} bypass permissions on\n\
                   \u{276F} ";
        let resp = extract_response(pre, cur);
        assert!(resp.contains("42"), "should contain the answer: {resp}");
        assert!(
            !resp.contains("────"),
            "should strip horizontal rule: {resp}"
        );
        assert!(!resp.contains("bypass"), "should strip status bar: {resp}");
    }

    #[test]
    fn extract_response_strips_echoed_input() {
        let pre = "\u{276F} ";
        let cur = "\u{276F} What is 2+2?\n\n4\n\n\u{276F} ";
        let resp = extract_response(pre, cur);
        assert!(resp.contains('4'), "should contain answer: {resp}");
        assert!(
            !resp.contains("What is 2+2"),
            "should strip echoed input: {resp}"
        );
    }

    #[test]
    fn extract_response_tui_full_rewrite() {
        // Simulate Claude TUI where entire screen changes
        let pre = "Welcome to Claude\n\n\u{276F} ";
        let cur = "\u{276F} Hello\n\nHello! How can I help?\n\n\
                   ────────────────────\n\
                   \u{276F} ";
        let resp = extract_response(pre, cur);
        assert!(
            resp.contains("Hello! How can I help?"),
            "should extract response from TUI rewrite: {resp}"
        );
    }

    #[test]
    fn strip_claude_bullets_removes_marker() {
        assert_eq!(strip_claude_bullets("\u{23FA} 4"), "4");
        assert_eq!(
            strip_claude_bullets("  \u{23FA} hello world"),
            "  hello world"
        );
        assert_eq!(strip_claude_bullets("no bullet here"), "no bullet here");
        assert_eq!(strip_claude_bullets(""), "");
    }

    #[test]
    fn extract_response_strips_claude_bullets() {
        let pre = "\u{276F} ";
        let cur = "\u{276F} question\n\n\u{23FA} 42\n\n\u{276F} ";
        let resp = extract_response(pre, cur);
        assert!(resp.contains("42"), "should contain answer: {resp}");
        assert!(
            !resp.contains('\u{23FA}'),
            "should strip bullet marker: {resp}"
        );
    }

    #[test]
    fn preserve_handoff_writes_diff_and_commit_summary() {
        let repo = tempfile::tempdir().unwrap();
        init_test_git_repo(repo.path());

        std::fs::write(repo.path().join("tracked.txt"), "one\n").unwrap();
        run_test_git(repo.path(), &["add", "tracked.txt"]);
        run_test_git(repo.path(), &["commit", "-m", "initial commit"]);
        std::fs::write(repo.path().join("tracked.txt"), "one\ntwo\n").unwrap();

        let recent_output = "\
running cargo test --lib\n\
test result: ok\n\
editing src/lib.rs\n";
        let task = crate::task::Task {
            id: 42,
            title: "resume widget".to_string(),
            status: "in-progress".to_string(),
            priority: "high".to_string(),
            claimed_by: Some("eng-1".to_string()),
            blocked: None,
            tags: Vec::new(),
            depends_on: Vec::new(),
            review_owner: None,
            blocked_on: None,
            worktree_path: None,
            branch: None,
            commit: None,
            artifacts: Vec::new(),
            next_action: Some("Run the Rust tests and finish the restart handoff.".to_string()),
            scheduled_for: None,
            cron_schedule: None,
            cron_last_run: None,
            completed: None,
            description: "Continue widget implementation.".to_string(),
            batty_config: None,
            source_path: repo.path().join("task-42.md"),
        };
        preserve_handoff(repo.path(), &task, Some(recent_output)).unwrap();

        let handoff = std::fs::read_to_string(repo.path().join(HANDOFF_FILE_NAME)).unwrap();
        assert!(handoff.contains("# Carry-Forward Summary"));
        assert!(handoff.contains("## Task Spec"));
        assert!(handoff.contains("Task #42: resume widget"));
        assert!(handoff.contains("## Work Completed So Far"));
        assert!(handoff.contains("### Changed Files"));
        assert!(handoff.contains("tracked.txt"));
        assert!(handoff.contains("### Tests Run"));
        assert!(handoff.contains("cargo test --lib"));
        assert!(handoff.contains("### Recent Activity"));
        assert!(handoff.contains("editing src/lib.rs"));
        assert!(handoff.contains("### Recent Commits"));
        assert!(handoff.contains("initial commit"));
        assert!(handoff.contains("## What Remains"));
        assert!(handoff.contains("Run the Rust tests and finish the restart handoff."));
    }

    #[test]
    fn preserve_handoff_uses_none_when_repo_has_no_changes_or_commits() {
        let repo = tempfile::tempdir().unwrap();
        init_test_git_repo(repo.path());

        let task = crate::task::Task {
            id: 7,
            title: "empty repo".to_string(),
            status: "in-progress".to_string(),
            priority: "low".to_string(),
            claimed_by: Some("eng-1".to_string()),
            blocked: None,
            tags: Vec::new(),
            depends_on: Vec::new(),
            review_owner: None,
            blocked_on: None,
            worktree_path: None,
            branch: None,
            commit: None,
            artifacts: Vec::new(),
            next_action: None,
            scheduled_for: None,
            cron_schedule: None,
            cron_last_run: None,
            completed: None,
            description: "No changes yet.".to_string(),
            batty_config: None,
            source_path: repo.path().join("task-7.md"),
        };
        preserve_handoff(repo.path(), &task, None).unwrap();

        let handoff = std::fs::read_to_string(repo.path().join(HANDOFF_FILE_NAME)).unwrap();
        assert!(handoff.contains("### Changed Files\n(none)"));
        assert!(handoff.contains("### Tests Run\n(none)"));
        assert!(handoff.contains("### Recent Activity\n(none)"));
        assert!(handoff.contains("### Recent Commits\n(none)"));
        assert!(handoff.contains("## What Remains"));
    }

    #[test]
    fn extract_test_commands_deduplicates_known_test_invocations() {
        let output = "\
\u{1b}[31mcargo test --lib\u{1b}[0m\n\
pytest tests/test_api.py\n\
cargo test --lib\n\
plain output\n";
        let tests = extract_test_commands(output);
        assert_eq!(
            tests,
            vec![
                "cargo test --lib".to_string(),
                "pytest tests/test_api.py".to_string()
            ]
        );
    }

    #[test]
    fn preserve_work_before_kill_respects_config_toggle() {
        let tmp = tempfile::tempdir().unwrap();
        let preserved =
            preserve_work_before_kill_with(tmp.path(), Duration::from_millis(10), false, |_path| {
                panic!("commit should not run when disabled")
            })
            .unwrap();

        assert!(!preserved);
    }

    #[test]
    fn preserve_work_before_kill_times_out() {
        let tmp = tempfile::tempdir().unwrap();
        let preserved =
            preserve_work_before_kill_with(tmp.path(), Duration::from_millis(10), true, |_path| {
                std::thread::sleep(Duration::from_millis(50));
                Ok(true)
            })
            .unwrap();

        assert!(!preserved);
    }

    fn init_test_git_repo(path: &Path) {
        run_test_git(path, &["init"]);
        run_test_git(path, &["config", "user.name", "Batty Tests"]);
        run_test_git(path, &["config", "user.email", "batty-tests@example.com"]);
    }

    fn run_test_git(path: &Path, args: &[&str]) {
        use std::process::Command;
        let output = Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
