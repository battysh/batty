//! Codex SDK-mode shim runtime: communicates with Codex via JSONL events
//! from `codex exec --json` instead of screen-scraping a PTY.
//!
//! Unlike the Claude SDK runtime (persistent subprocess with stdin NDJSON),
//! Codex uses a **spawn-per-message** model: each `SendMessage` launches a
//! new `codex exec --json` subprocess. Multi-turn context is preserved via
//! `codex exec resume <thread_id>`.
//!
//! Emits the same `Command`/`Event` protocol to the orchestrator as the
//! PTY runtime, making it transparent to all upstream consumers.

use std::collections::VecDeque;
use std::io::Write;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::codex_types::{self, CodexEvent};
use super::common::{
    self, MAX_QUEUE_DEPTH, QueuedMessage, SESSION_STATS_INTERVAL_SECS, drain_queue_errors,
    format_injected_message,
};
use super::protocol::{Channel, Command as ShimCommand, Event, ShimState};
use super::pty_log::PtyLogWriter;
use super::runtime::ShimArgs;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

const PROCESS_EXIT_POLL_MS: u64 = 100;
const GROUP_TERM_GRACE_SECS: u64 = 2;

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

struct CodexState {
    state: ShimState,
    state_changed_at: Instant,
    started_at: Instant,
    /// Thread ID from the first `thread.started` event, used for resume.
    thread_id: Option<String>,
    /// Accumulated agent response text for the current turn.
    accumulated_response: String,
    /// Message ID of the currently pending (in-flight) message.
    pending_message_id: Option<String>,
    /// Messages queued while the agent is in Working state.
    message_queue: VecDeque<QueuedMessage>,
    /// Total bytes of response text received.
    cumulative_output_bytes: u64,
    /// The codex binary name/path.
    program: String,
    /// Working directory for spawning subprocesses.
    cwd: std::path::PathBuf,
    /// Whether a ContextApproaching event has already been emitted this session.
    context_approaching_emitted: bool,
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

/// Run the Codex SDK-mode shim. This function does not return until the shim exits.
///
/// Unlike the Claude SDK runtime, each message spawns a new `codex exec --json`
/// subprocess. The shim manages the lifecycle per-message and emits the same
/// `Command`/`Event` protocol to the orchestrator.
pub fn run_codex_sdk(args: ShimArgs, channel: Channel) -> Result<()> {
    eprintln!("[shim-codex {}] started (spawn-per-message mode)", args.id);

    // Shared state
    let state = Arc::new(Mutex::new(CodexState {
        state: ShimState::Idle,
        state_changed_at: Instant::now(),
        started_at: Instant::now(),
        thread_id: None,
        accumulated_response: String::new(),
        pending_message_id: None,
        message_queue: VecDeque::new(),
        cumulative_output_bytes: 0,
        program: "codex".to_string(),
        cwd: args.cwd.clone(),
        context_approaching_emitted: false,
    }));

    // PTY log writer (optional — writes readable text for tmux display)
    let pty_log: Option<Arc<Mutex<PtyLogWriter>>> = args
        .pty_log_path
        .as_deref()
        .map(|p| PtyLogWriter::new(p).context("failed to create PTY log"))
        .transpose()?
        .map(|w| Arc::new(Mutex::new(w)));

    // Channel clones
    let mut cmd_channel = channel;

    // Emit Ready immediately — no persistent subprocess to wait for.
    cmd_channel.send(&Event::Ready)?;

    // Session stats thread
    let state_stats = Arc::clone(&state);
    let mut stats_channel = cmd_channel
        .try_clone()
        .context("failed to clone channel for stats")?;
    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_secs(SESSION_STATS_INTERVAL_SECS));
            let st = state_stats.lock().unwrap();
            if st.state == ShimState::Dead {
                return;
            }
            let output_bytes = st.cumulative_output_bytes;
            let uptime_secs = st.started_at.elapsed().as_secs();
            drop(st);

            if stats_channel
                .send(&Event::SessionStats {
                    output_bytes,
                    uptime_secs,
                    input_tokens: 0,
                    output_tokens: 0,
                })
                .is_err()
            {
                return;
            }
        }
    });

    // Command loop (main thread)
    let state_cmd = Arc::clone(&state);
    let shim_id = args.id.clone();
    loop {
        let cmd = match cmd_channel.recv::<ShimCommand>() {
            Ok(Some(c)) => c,
            Ok(None) => {
                eprintln!("[shim-codex {shim_id}] orchestrator disconnected");
                break;
            }
            Err(e) => {
                eprintln!("[shim-codex {shim_id}] channel error: {e}");
                break;
            }
        };

        match cmd {
            ShimCommand::SendMessage {
                from,
                body,
                message_id,
            } => {
                let delivery_id = message_id.clone();
                let mut st = state_cmd.lock().unwrap();
                match st.state {
                    ShimState::Idle => {
                        st.pending_message_id = message_id;
                        st.accumulated_response.clear();
                        st.state = ShimState::Working;
                        st.state_changed_at = Instant::now();
                        let thread_id = st.thread_id.clone();
                        let program = st.program.clone();
                        let cwd = st.cwd.clone();
                        drop(st);

                        cmd_channel.send(&Event::StateChanged {
                            from: ShimState::Idle,
                            to: ShimState::Working,
                            summary: String::new(),
                        })?;
                        if let Some(id) = delivery_id {
                            cmd_channel.send(&Event::MessageDelivered { id })?;
                        }

                        // Spawn codex exec subprocess for this message
                        let text = format_injected_message(&from, &body);
                        let (exec_program, exec_args) =
                            codex_types::codex_sdk_args(&program, thread_id.as_deref());

                        let mut evt_channel = cmd_channel
                            .try_clone()
                            .context("failed to clone channel for codex exec")?;
                        let state_exec = Arc::clone(&state_cmd);
                        let pty_log_exec = pty_log.clone();
                        let shim_id_exec = shim_id.clone();

                        // Run the codex exec subprocess in a background thread
                        thread::spawn(move || {
                            run_codex_exec(
                                &shim_id_exec,
                                &exec_program,
                                &exec_args,
                                &text,
                                &cwd,
                                &state_exec,
                                &mut evt_channel,
                                pty_log_exec.as_ref(),
                            );
                        });
                    }
                    ShimState::Working => {
                        // Queue the message
                        if st.message_queue.len() >= MAX_QUEUE_DEPTH {
                            let dropped = st.message_queue.pop_front();
                            let dropped_id = dropped.as_ref().and_then(|m| m.message_id.clone());
                            st.message_queue.push_back(QueuedMessage {
                                from,
                                body,
                                message_id,
                            });
                            let depth = st.message_queue.len();
                            drop(st);

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
                            st.message_queue.push_back(QueuedMessage {
                                from,
                                body,
                                message_id,
                            });
                            let depth = st.message_queue.len();
                            drop(st);

                            cmd_channel.send(&Event::Warning {
                                message: format!(
                                    "message queued while agent working (depth: {depth})"
                                ),
                                idle_secs: None,
                            })?;
                        }
                    }
                    other => {
                        drop(st);
                        cmd_channel.send(&Event::Error {
                            command: "SendMessage".into(),
                            reason: format!("agent in {other} state, cannot accept message"),
                        })?;
                    }
                }
            }

            ShimCommand::CaptureScreen { last_n_lines } => {
                let st = state_cmd.lock().unwrap();
                let content = match last_n_lines {
                    Some(n) => last_n_lines_of(&st.accumulated_response, n),
                    None => st.accumulated_response.clone(),
                };
                drop(st);
                cmd_channel.send(&Event::ScreenCapture {
                    content,
                    cursor_row: 0,
                    cursor_col: 0,
                })?;
            }

            ShimCommand::GetState => {
                let st = state_cmd.lock().unwrap();
                let since = st.state_changed_at.elapsed().as_secs();
                let state = st.state;
                drop(st);
                cmd_channel.send(&Event::State {
                    state,
                    since_secs: since,
                })?;
            }

            ShimCommand::Resize { .. } => {
                // No-op — no PTY.
            }

            ShimCommand::Ping => {
                cmd_channel.send(&Event::Pong)?;
            }

            ShimCommand::Shutdown { reason, .. } => {
                eprintln!(
                    "[shim-codex {shim_id}] shutdown requested ({})",
                    reason.label()
                );
                if let Err(error) = super::runtime::preserve_work_before_kill(&args.cwd) {
                    eprintln!(
                        "[shim-codex {shim_id}] failed to preserve work before shutdown: {error:#}"
                    );
                }
                let mut st = state_cmd.lock().unwrap();
                st.state = ShimState::Dead;
                st.state_changed_at = Instant::now();
                drop(st);
                break;
            }

            ShimCommand::Kill => {
                if let Err(error) = super::runtime::preserve_work_before_kill(&args.cwd) {
                    eprintln!(
                        "[shim-codex {shim_id}] failed to preserve work before kill: {error:#}"
                    );
                }
                let mut st = state_cmd.lock().unwrap();
                st.state = ShimState::Dead;
                st.state_changed_at = Instant::now();
                drop(st);
                break;
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Per-message subprocess execution
// ---------------------------------------------------------------------------

/// Spawn `codex exec --json ...` and process its JSONL output.
/// When the subprocess exits, transition back to Idle and drain the queue.
#[allow(clippy::too_many_arguments)]
fn run_codex_exec(
    shim_id: &str,
    program: &str,
    args: &[String],
    prompt: &str,
    cwd: &std::path::Path,
    state: &Arc<Mutex<CodexState>>,
    evt_channel: &mut Channel,
    pty_log: Option<&Arc<Mutex<PtyLogWriter>>>,
) {
    // Spawn the subprocess
    let mut child = match Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("CLAUDECODE")
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[shim-codex {shim_id}] failed to spawn codex exec: {e}");
            let mut st = state.lock().unwrap();
            let msg_id = st.pending_message_id.take();
            st.state = ShimState::Idle;
            st.state_changed_at = Instant::now();
            drop(st);
            let _ = evt_channel.send(&Event::Error {
                command: "SendMessage".into(),
                reason: format!("codex exec spawn failed: {e}"),
            });
            let _ = evt_channel.send(&Event::StateChanged {
                from: ShimState::Working,
                to: ShimState::Idle,
                summary: format!("spawn failed: {e}"),
            });
            let _ = evt_channel.send(&Event::Completion {
                message_id: msg_id,
                response: String::new(),
                last_lines: format!("spawn failed: {e}"),
            });
            return;
        }
    };

    let child_pid = child.id();
    eprintln!("[shim-codex {shim_id}] codex exec spawned (pid {child_pid})");

    if let Some(mut stdin) = child.stdin.take() {
        if let Err(e) = stdin.write_all(prompt.as_bytes()) {
            eprintln!("[shim-codex {shim_id}] failed to write prompt to stdin: {e}");
        }
    }

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    // stderr reader (log only)
    let shim_id_err = shim_id.to_string();
    let pty_log_err = pty_log.map(Arc::clone);
    thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line_result in reader.lines() {
            match line_result {
                Ok(line) => {
                    eprintln!("[shim-codex {shim_id_err}] stderr: {line}");
                    if let Some(ref log) = pty_log_err {
                        let _ = log
                            .lock()
                            .unwrap()
                            .write(format!("[stderr] {line}\n").as_bytes());
                    }
                }
                Err(_) => break,
            }
        }
    });

    // stdout JSONL reader
    let reader = BufReader::new(stdout);
    for line_result in reader.lines() {
        let line = match line_result {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[shim-codex {shim_id}] stdout read error: {e}");
                break;
            }
        };

        if line.trim().is_empty() {
            continue;
        }

        let evt: CodexEvent = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("[shim-codex {shim_id}] ignoring unparseable JSONL: {e}");
                continue;
            }
        };

        match evt.event_type.as_str() {
            "thread.started" => {
                if let Some(tid) = evt.thread_id {
                    let mut st = state.lock().unwrap();
                    st.thread_id = Some(tid.clone());
                    eprintln!("[shim-codex {shim_id}] thread started: {tid}");
                }
            }

            "item.completed" | "item.updated" => {
                if let Some(ref item) = evt.item {
                    if let Some(text) = item.agent_text() {
                        if !text.is_empty() {
                            let mut st = state.lock().unwrap();
                            // Replace accumulated response on each complete agent_message
                            // (Codex sends the full text each time, not deltas)
                            if evt.event_type == "item.completed" {
                                st.accumulated_response = text.to_string();
                            }
                            st.cumulative_output_bytes += text.len() as u64;

                            // Check for context approaching limit (proactive).
                            if !st.context_approaching_emitted
                                && common::detect_context_approaching_limit(text)
                            {
                                st.context_approaching_emitted = true;
                                drop(st);
                                let _ = evt_channel.send(&Event::ContextApproaching {
                                    message: "Agent output contains context-pressure signals"
                                        .into(),
                                    input_tokens: 0,
                                    output_tokens: 0,
                                });
                            } else {
                                drop(st);
                            }

                            if let Some(log) = pty_log {
                                let _ = log.lock().unwrap().write(text.as_bytes());
                                let _ = log.lock().unwrap().write(b"\n");
                            }
                        }
                    }
                }
            }

            "turn.failed" => {
                let error_msg = evt
                    .error
                    .as_ref()
                    .map(|e| e.message.clone())
                    .unwrap_or_else(|| "unknown error".to_string());
                eprintln!("[shim-codex {shim_id}] turn failed: {error_msg}");

                // Check for context exhaustion
                if common::detect_context_exhausted(&error_msg) {
                    let mut st = state.lock().unwrap();
                    let last_lines = last_n_lines_of(&st.accumulated_response, 5);
                    st.state = ShimState::ContextExhausted;
                    st.state_changed_at = Instant::now();
                    let drain =
                        drain_queue_errors(&mut st.message_queue, ShimState::ContextExhausted);
                    drop(st);

                    let _ = evt_channel.send(&Event::StateChanged {
                        from: ShimState::Working,
                        to: ShimState::ContextExhausted,
                        summary: last_lines.clone(),
                    });
                    let _ = evt_channel.send(&Event::ContextExhausted {
                        message: error_msg,
                        last_lines,
                    });
                    for event in drain {
                        let _ = evt_channel.send(&event);
                    }
                    return;
                }
            }

            "error" => {
                let error_msg = evt
                    .error
                    .as_ref()
                    .map(|e| e.message.clone())
                    .unwrap_or_else(|| "stream error".to_string());
                eprintln!("[shim-codex {shim_id}] error event: {error_msg}");

                // Detect quota/billing exhaustion — emit a specific event so
                // the daemon can pause dispatch and alert the human.
                let lower = error_msg.to_ascii_lowercase();
                if lower.contains("usage limit")
                    || lower.contains("quota")
                    || lower.contains("billing")
                    || lower.contains("purchase more credits")
                {
                    eprintln!("[shim-codex {shim_id}] QUOTA EXHAUSTED: {error_msg}");
                    let _ = evt_channel.send(&Event::Error {
                        command: "QuotaExhausted".into(),
                        reason: error_msg.clone(),
                    });
                }
            }

            // turn.started, turn.completed, item.started — informational, no action
            _ => {}
        }
    }

    // stdout closed — subprocess finished. Wait for exit.
    let exit_code = child.wait().ok().and_then(|s| s.code());
    eprintln!("[shim-codex {shim_id}] codex exec exited (code: {exit_code:?})");

    // Transition Working → Idle, emit Completion
    let mut st = state.lock().unwrap();
    let response = std::mem::take(&mut st.accumulated_response);
    let last_lines = last_n_lines_of(&response, 5);
    let msg_id = st.pending_message_id.take();
    st.state = ShimState::Idle;
    st.state_changed_at = Instant::now();

    // Check for queued messages
    let queued_msg = if !st.message_queue.is_empty() {
        st.message_queue.pop_front()
    } else {
        None
    };

    if let Some(ref qm) = queued_msg {
        st.pending_message_id = qm.message_id.clone();
        st.state = ShimState::Working;
        st.state_changed_at = Instant::now();
        st.accumulated_response.clear();
    }

    let thread_id = st.thread_id.clone();
    let program = st.program.clone();
    let cwd_owned = st.cwd.clone();
    let queue_depth = st.message_queue.len();
    drop(st);

    let _ = evt_channel.send(&Event::StateChanged {
        from: ShimState::Working,
        to: ShimState::Idle,
        summary: last_lines.clone(),
    });
    let _ = evt_channel.send(&Event::Completion {
        message_id: msg_id,
        response,
        last_lines,
    });

    // Drain queued message by spawning another codex exec
    if let Some(qm) = queued_msg {
        let _ = evt_channel.send(&Event::StateChanged {
            from: ShimState::Idle,
            to: ShimState::Working,
            summary: format!("delivering queued message ({queue_depth} remaining)"),
        });

        let text = format_injected_message(&qm.from, &qm.body);
        let (exec_program, exec_args) = codex_types::codex_sdk_args(&program, thread_id.as_deref());

        // Recursive call for queued message (same thread)
        run_codex_exec(
            shim_id,
            &exec_program,
            &exec_args,
            &text,
            &cwd_owned,
            state,
            evt_channel,
            pty_log,
        );
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Terminate a child process: SIGTERM, grace period, then SIGKILL.
#[allow(dead_code)]
fn terminate_child(child: &mut Child) {
    let pid = child.id();
    #[cfg(unix)]
    {
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(GROUP_TERM_GRACE_SECS);
        loop {
            if Instant::now() > deadline {
                break;
            }
            match child.try_wait() {
                Ok(Some(_)) => return,
                _ => thread::sleep(Duration::from_millis(PROCESS_EXIT_POLL_MS)),
            }
        }
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
    #[allow(unreachable_code)]
    {
        let _ = child.kill();
    }
}

/// Extract the last N lines from a string.
fn last_n_lines_of(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shim::protocol;

    #[test]
    fn last_n_lines_basic() {
        assert_eq!(last_n_lines_of("a\nb\nc", 2), "b\nc");
        assert_eq!(last_n_lines_of("a\nb\nc", 10), "a\nb\nc");
        assert_eq!(last_n_lines_of("", 5), "");
    }

    #[test]
    fn codex_state_initial() {
        let st = CodexState {
            state: ShimState::Idle,
            state_changed_at: Instant::now(),
            started_at: Instant::now(),
            thread_id: None,
            accumulated_response: String::new(),
            pending_message_id: None,
            message_queue: VecDeque::new(),
            cumulative_output_bytes: 0,
            program: "codex".into(),
            cwd: std::path::PathBuf::from("/tmp"),
            context_approaching_emitted: false,
        };
        assert_eq!(st.state, ShimState::Idle);
        assert!(st.thread_id.is_none());
    }

    #[test]
    fn channel_events_roundtrip() {
        let (parent_sock, child_sock) = protocol::socketpair().unwrap();
        let mut parent = protocol::Channel::new(parent_sock);
        let mut child = protocol::Channel::new(child_sock);

        child.send(&Event::Ready).unwrap();
        let event: Event = parent.recv().unwrap().unwrap();
        assert!(matches!(event, Event::Ready));
    }
}
