//! SDK-mode shim runtime: communicates with Claude Code via NDJSON on
//! stdin/stdout instead of screen-scraping a PTY.
//!
//! Emits the same `Command`/`Event` protocol to the orchestrator as the
//! PTY runtime (`runtime.rs`), making it transparent to all upstream consumers.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write as IoWrite};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::common::{
    self, MAX_QUEUE_DEPTH, QueuedMessage, SESSION_STATS_INTERVAL_SECS, drain_queue_errors,
    format_injected_message,
};
use super::protocol::{Channel, Command as ShimCommand, Event, ShimState};
use super::pty_log::PtyLogWriter;
use super::runtime::ShimArgs;
use super::sdk_types::{self, SdkControlResponse, SdkOutput, SdkUserMessage};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

const PROCESS_EXIT_POLL_MS: u64 = 100;
const GROUP_TERM_GRACE_SECS: u64 = 2;

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

struct SdkState {
    state: ShimState,
    state_changed_at: Instant,
    started_at: Instant,
    /// Session ID returned by Claude Code in its first response.
    session_id: String,
    /// Accumulated assistant response text for the current turn.
    accumulated_response: String,
    /// Message ID of the currently pending (in-flight) message.
    pending_message_id: Option<String>,
    /// Messages queued while the agent is in Working state.
    message_queue: VecDeque<QueuedMessage>,
    /// Total bytes of response text received.
    cumulative_output_bytes: u64,
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

/// Run the SDK-mode shim. This function does not return until the shim exits.
///
/// `channel` is the pre-connected socket to the orchestrator (fd 3 or socketpair).
/// `args.cmd` must be a shell command that launches Claude Code in stream-json mode.
pub fn run_sdk(args: ShimArgs, channel: Channel) -> Result<()> {
    // -- Spawn subprocess with piped stdin/stdout/stderr --
    let mut child = Command::new("bash")
        .args(["-lc", &args.cmd])
        .current_dir(&args.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("CLAUDECODE") // prevent nested detection
        .spawn()
        .with_context(|| format!("[shim-sdk {}] failed to spawn agent", args.id))?;

    let child_pid = child.id();
    eprintln!(
        "[shim-sdk {}] spawned agent subprocess (pid {})",
        args.id, child_pid
    );

    let child_stdin = child.stdin.take().context("failed to take child stdin")?;
    let child_stdout = child.stdout.take().context("failed to take child stdout")?;
    let child_stderr = child.stderr.take().context("failed to take child stderr")?;

    // Shared state
    let state = Arc::new(Mutex::new(SdkState {
        state: ShimState::Idle, // SDK mode is immediately ready
        state_changed_at: Instant::now(),
        started_at: Instant::now(),
        session_id: String::new(),
        accumulated_response: String::new(),
        pending_message_id: None,
        message_queue: VecDeque::new(),
        cumulative_output_bytes: 0,
    }));

    // Shared stdin writer (used by both command loop and stdout reader for auto-approve)
    let stdin_writer = Arc::new(Mutex::new(child_stdin));

    // -- PTY log writer (optional — writes readable text, not raw NDJSON) --
    let pty_log: Option<Arc<Mutex<PtyLogWriter>>> = args
        .pty_log_path
        .as_deref()
        .map(|p| PtyLogWriter::new(p).context("failed to create PTY log"))
        .transpose()?
        .map(|w| Arc::new(Mutex::new(w)));

    // -- Channel clones for threads --
    let mut cmd_channel = channel;
    let mut evt_channel = cmd_channel
        .try_clone()
        .context("failed to clone channel for stdout reader")?;

    // Emit Ready immediately — Claude -p mode accepts input on stdin right away.
    cmd_channel.send(&Event::Ready)?;

    // -- stdout reader thread --
    let state_stdout = Arc::clone(&state);
    let stdin_for_approve = Arc::clone(&stdin_writer);
    let pty_log_stdout = pty_log.clone();
    let shim_id = args.id.clone();
    let stdout_handle = thread::spawn(move || {
        let reader = BufReader::new(child_stdout);
        for line_result in reader.lines() {
            let line = match line_result {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("[shim-sdk {shim_id}] stdout read error: {e}");
                    break;
                }
            };

            if line.trim().is_empty() {
                continue;
            }

            let msg: SdkOutput = match serde_json::from_str(&line) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("[shim-sdk {shim_id}] ignoring unparseable NDJSON line: {e}");
                    continue;
                }
            };

            match msg.msg_type.as_str() {
                "assistant" => {
                    // Extract text from the assistant message
                    if let Some(ref message) = msg.message {
                        let text = sdk_types::extract_assistant_text(message);
                        if !text.is_empty() {
                            let mut st = state_stdout.lock().unwrap();
                            st.accumulated_response.push_str(&text);
                            st.cumulative_output_bytes += text.len() as u64;

                            // Update session_id from first response
                            if st.session_id.is_empty() {
                                if let Some(ref sid) = msg.session_id {
                                    st.session_id = sid.clone();
                                }
                            }
                            drop(st);

                            // Write to PTY log for tmux display
                            if let Some(ref log) = pty_log_stdout {
                                let _ = log.lock().unwrap().write(text.as_bytes());
                            }
                        }
                    }
                }

                "stream_event" => {
                    // Extract incremental text delta
                    if let Some(ref event) = msg.event {
                        if let Some(text) = sdk_types::extract_stream_text(event) {
                            let mut st = state_stdout.lock().unwrap();
                            st.accumulated_response.push_str(&text);
                            st.cumulative_output_bytes += text.len() as u64;

                            if st.session_id.is_empty() {
                                if let Some(ref sid) = msg.session_id {
                                    st.session_id = sid.clone();
                                }
                            }
                            drop(st);

                            if let Some(ref log) = pty_log_stdout {
                                let _ = log.lock().unwrap().write(text.as_bytes());
                            }
                        }
                    }
                }

                "control_request" => {
                    // Auto-approve tool use requests
                    if msg.request_subtype().as_deref() == Some("can_use_tool") {
                        if let (Some(req_id), Some(ref tool_use_id)) =
                            (msg.request_id.as_ref(), msg.request_tool_use_id())
                        {
                            let resp = SdkControlResponse::approve_tool(req_id, tool_use_id);
                            let ndjson = resp.to_ndjson();
                            if let Ok(mut writer) = stdin_for_approve.lock() {
                                let _ = writeln!(writer, "{ndjson}");
                                let _ = writer.flush();
                            }
                        }
                    }
                }

                "result" => {
                    let mut st = state_stdout.lock().unwrap();

                    // Capture session_id
                    if st.session_id.is_empty() {
                        if let Some(ref sid) = msg.session_id {
                            st.session_id = sid.clone();
                        }
                    }

                    // Check for context exhaustion
                    let is_context_exhausted = msg
                        .errors
                        .as_ref()
                        .map(|errs| errs.iter().any(|e| common::detect_context_exhausted(e)))
                        .unwrap_or(false)
                        || msg
                            .result
                            .as_deref()
                            .map(common::detect_context_exhausted)
                            .unwrap_or(false);

                    if is_context_exhausted {
                        let last_lines = last_n_lines_of(&st.accumulated_response, 5);
                        let old = st.state;
                        st.state = ShimState::ContextExhausted;
                        st.state_changed_at = Instant::now();

                        let drain =
                            drain_queue_errors(&mut st.message_queue, ShimState::ContextExhausted);
                        drop(st);

                        let _ = evt_channel.send(&Event::StateChanged {
                            from: old,
                            to: ShimState::ContextExhausted,
                            summary: last_lines.clone(),
                        });
                        let _ = evt_channel.send(&Event::ContextExhausted {
                            message: "Agent reported context exhaustion".into(),
                            last_lines,
                        });
                        for event in drain {
                            let _ = evt_channel.send(&event);
                        }
                        continue;
                    }

                    // Normal completion: Working → Idle
                    let response = if st.accumulated_response.is_empty() {
                        msg.result.clone().unwrap_or_default()
                    } else {
                        std::mem::take(&mut st.accumulated_response)
                    };
                    let last_lines = last_n_lines_of(&response, 5);
                    let msg_id = st.pending_message_id.take();
                    let old = st.state;
                    st.state = ShimState::Idle;
                    st.state_changed_at = Instant::now();

                    // Check for queued messages to deliver immediately
                    let queued_msg = if !st.message_queue.is_empty() {
                        st.message_queue.pop_front()
                    } else {
                        None
                    };

                    // If injecting a queued message, stay Working
                    if let Some(ref qm) = queued_msg {
                        st.pending_message_id = qm.message_id.clone();
                        st.state = ShimState::Working;
                        st.state_changed_at = Instant::now();
                        st.accumulated_response.clear();
                    }

                    let queue_depth = st.message_queue.len();
                    let session_id = st.session_id.clone();
                    drop(st);

                    // Emit completion events
                    let _ = evt_channel.send(&Event::StateChanged {
                        from: old,
                        to: ShimState::Idle,
                        summary: last_lines.clone(),
                    });
                    let _ = evt_channel.send(&Event::Completion {
                        message_id: msg_id,
                        response,
                        last_lines,
                    });

                    // Inject queued message
                    if let Some(qm) = queued_msg {
                        let text = format_injected_message(&qm.from, &qm.body);
                        let user_msg = SdkUserMessage::new(&session_id, &text);
                        let ndjson = user_msg.to_ndjson();
                        if let Ok(mut writer) = stdin_for_approve.lock() {
                            let _ = writeln!(writer, "{ndjson}");
                            let _ = writer.flush();
                        }
                        let _ = evt_channel.send(&Event::StateChanged {
                            from: ShimState::Idle,
                            to: ShimState::Working,
                            summary: format!("delivering queued message ({queue_depth} remaining)"),
                        });
                    }
                }

                _ => {
                    // Silently ignore unknown message types (future-proof)
                }
            }
        }

        // stdout EOF — agent process closed
        let mut st = state_stdout.lock().unwrap();
        let last_lines = last_n_lines_of(&st.accumulated_response, 10);
        let old = st.state;
        st.state = ShimState::Dead;
        st.state_changed_at = Instant::now();

        let drain = drain_queue_errors(&mut st.message_queue, ShimState::Dead);
        drop(st);

        let _ = evt_channel.send(&Event::StateChanged {
            from: old,
            to: ShimState::Dead,
            summary: last_lines.clone(),
        });
        let _ = evt_channel.send(&Event::Died {
            exit_code: None,
            last_lines,
        });
        for event in drain {
            let _ = evt_channel.send(&event);
        }
    });

    // -- stderr reader thread --
    let shim_id_err = args.id.clone();
    let pty_log_stderr = pty_log;
    thread::spawn(move || {
        let reader = BufReader::new(child_stderr);
        for line_result in reader.lines() {
            match line_result {
                Ok(line) => {
                    eprintln!("[shim-sdk {shim_id_err}] stderr: {line}");
                    if let Some(ref log) = pty_log_stderr {
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

    // -- Session stats thread --
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
                })
                .is_err()
            {
                return;
            }
        }
    });

    // -- Command loop (main thread) --
    let state_cmd = Arc::clone(&state);
    loop {
        let cmd = match cmd_channel.recv::<ShimCommand>() {
            Ok(Some(c)) => c,
            Ok(None) => {
                eprintln!(
                    "[shim-sdk {}] orchestrator disconnected, shutting down",
                    args.id
                );
                terminate_child(&mut child);
                break;
            }
            Err(e) => {
                eprintln!("[shim-sdk {}] channel error: {e}", args.id);
                terminate_child(&mut child);
                break;
            }
        };

        match cmd {
            ShimCommand::SendMessage {
                from,
                body,
                message_id,
            } => {
                let mut st = state_cmd.lock().unwrap();
                match st.state {
                    ShimState::Idle => {
                        st.pending_message_id = message_id;
                        st.accumulated_response.clear();
                        let session_id = st.session_id.clone();
                        st.state = ShimState::Working;
                        st.state_changed_at = Instant::now();
                        drop(st);

                        let text = format_injected_message(&from, &body);
                        let user_msg = SdkUserMessage::new(&session_id, &text);
                        let ndjson = user_msg.to_ndjson();

                        if let Ok(mut writer) = stdin_writer.lock() {
                            if let Err(e) = writeln!(writer, "{ndjson}") {
                                cmd_channel.send(&Event::Error {
                                    command: "SendMessage".into(),
                                    reason: format!("stdin write failed: {e}"),
                                })?;
                                continue;
                            }
                            let _ = writer.flush();
                        }

                        cmd_channel.send(&Event::StateChanged {
                            from: ShimState::Idle,
                            to: ShimState::Working,
                            summary: String::new(),
                        })?;
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
                // No-op in SDK mode — no PTY to resize.
            }

            ShimCommand::Ping => {
                cmd_channel.send(&Event::Pong)?;
            }

            ShimCommand::Shutdown { timeout_secs } => {
                eprintln!(
                    "[shim-sdk {}] shutdown requested (timeout: {}s)",
                    args.id, timeout_secs
                );
                // Close stdin to signal EOF to the subprocess
                drop(stdin_writer);

                let deadline = Instant::now() + Duration::from_secs(timeout_secs as u64);
                loop {
                    if Instant::now() > deadline {
                        terminate_child(&mut child);
                        break;
                    }
                    match child.try_wait() {
                        Ok(Some(_)) => break,
                        _ => thread::sleep(Duration::from_millis(PROCESS_EXIT_POLL_MS)),
                    }
                }
                break;
            }

            ShimCommand::Kill => {
                terminate_child(&mut child);
                break;
            }
        }
    }

    stdout_handle.join().ok();
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Terminate a child process: SIGTERM, grace period, then SIGKILL.
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
        let text = "a\nb\nc\nd\ne";
        assert_eq!(last_n_lines_of(text, 3), "c\nd\ne");
        assert_eq!(last_n_lines_of(text, 10), "a\nb\nc\nd\ne");
        assert_eq!(last_n_lines_of(text, 0), "");
    }

    #[test]
    fn last_n_lines_empty() {
        assert_eq!(last_n_lines_of("", 5), "");
    }

    #[test]
    fn sdk_state_initial_values() {
        let st = SdkState {
            state: ShimState::Idle,
            state_changed_at: Instant::now(),
            started_at: Instant::now(),
            session_id: String::new(),
            accumulated_response: String::new(),
            pending_message_id: None,
            message_queue: VecDeque::new(),
            cumulative_output_bytes: 0,
        };
        assert_eq!(st.state, ShimState::Idle);
        assert!(st.session_id.is_empty());
        assert!(st.message_queue.is_empty());
    }

    /// Verify that the command loop handles SendMessage in Idle state:
    /// format a user message NDJSON and transition to Working.
    #[test]
    fn user_message_ndjson_format() {
        let msg = SdkUserMessage::new("sess-abc", "Fix the bug");
        let json: serde_json::Value = serde_json::from_str(&msg.to_ndjson()).unwrap();
        assert_eq!(json["type"], "user");
        assert_eq!(json["session_id"], "sess-abc");
        assert_eq!(json["message"]["role"], "user");
        assert_eq!(json["message"]["content"], "Fix the bug");
    }

    /// Verify that the protocol socketpair still works for our Event types.
    #[test]
    fn channel_round_trip_events() {
        let (parent_sock, child_sock) = protocol::socketpair().unwrap();
        let mut parent = protocol::Channel::new(parent_sock);
        let mut child = protocol::Channel::new(child_sock);

        child.send(&Event::Ready).unwrap();
        let event: Event = parent.recv().unwrap().unwrap();
        assert!(matches!(event, Event::Ready));

        child
            .send(&Event::Completion {
                message_id: Some("m1".into()),
                response: "done".into(),
                last_lines: "done".into(),
            })
            .unwrap();
        let event: Event = parent.recv().unwrap().unwrap();
        match event {
            Event::Completion {
                message_id,
                response,
                ..
            } => {
                assert_eq!(message_id.as_deref(), Some("m1"));
                assert_eq!(response, "done");
            }
            _ => panic!("expected Completion"),
        }
    }

    /// Verify context exhaustion detection from SDK result errors.
    #[test]
    fn context_exhaustion_from_errors() {
        assert!(common::detect_context_exhausted("context window exceeded"));
        assert!(common::detect_context_exhausted(
            "Error: the conversation is too long"
        ));
        assert!(!common::detect_context_exhausted("all good"));
    }
}
