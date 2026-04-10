//! SDK-mode shim runtime: communicates with Claude Code via NDJSON on
//! stdin/stdout instead of screen-scraping a PTY.
//!
//! Emits the same `Command`/`Event` protocol to the orchestrator as the
//! PTY runtime (`runtime.rs`), making it transparent to all upstream consumers.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write as IoWrite};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
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
const WORKING_READ_TIMEOUT: Duration = Duration::from_secs(120);
const STALLED_MID_TURN_MARKER: &str = "stalled mid-turn";
const MESSAGE_PREVIEW_LIMIT: usize = 160;
const SDK_COMMAND_POLL_MS: u64 = 1000;
const SDK_KEEPALIVE_IDLE_SECS: u64 = 300;
const SDK_KEEPALIVE_MESSAGE: &str =
    "Continue monitoring. If you have no pending work, reply with 'idle'.";
const PROACTIVE_CONTEXT_WARNING_PCT: u8 = 80;
const DEFAULT_CONTEXT_LIMIT_TOKENS: u64 = 128_000;

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
    /// Role that sent the current in-flight message.
    last_sent_message_from: Option<String>,
    /// Preview of the last in-flight message body.
    last_sent_message_preview: Option<String>,
    /// Most recent model name observed from Claude output.
    last_model_name: Option<String>,
    /// Messages queued while the agent is in Working state.
    message_queue: VecDeque<QueuedMessage>,
    /// Total bytes of response text received.
    cumulative_output_bytes: u64,
    /// Claude model name used by the current session, when reported.
    model: Option<String>,
    /// Consecutive failed test fix/retest loops handled inside the shim.
    test_failure_iterations: u8,
    /// Cumulative input tokens reported by the API.
    cumulative_input_tokens: u64,
    /// Cumulative output tokens reported by the API.
    cumulative_output_tokens: u64,
    /// Total tokens consumed by completed turns in the current session.
    cumulative_context_tokens: u64,
    /// Approximate percent of the model context budget already consumed.
    context_usage_pct: Option<u8>,
    /// Whether a ContextApproaching event has already been emitted this session.
    context_approaching_emitted: bool,
}

#[derive(Debug, Clone)]
struct ForcedCompletion {
    previous_state: ShimState,
    response: String,
    last_lines: String,
    message_id: Option<String>,
    queued_message: Option<QueuedMessage>,
    queue_depth: usize,
    session_id: String,
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
        last_sent_message_from: None,
        last_sent_message_preview: None,
        last_model_name: None,
        message_queue: VecDeque::new(),
        cumulative_output_bytes: 0,
        model: None,
        test_failure_iterations: 0,
        cumulative_input_tokens: 0,
        cumulative_output_tokens: 0,
        cumulative_context_tokens: 0,
        context_usage_pct: None,
        context_approaching_emitted: false,
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

    cmd_channel.set_read_timeout(Some(Duration::from_millis(SDK_COMMAND_POLL_MS)))?;

    // Emit Ready immediately — Claude -p mode accepts input on stdin right away.
    cmd_channel.send(&Event::Ready)?;

    // -- stdout reader thread --
    let state_stdout = Arc::clone(&state);
    let stdin_for_approve = Arc::clone(&stdin_writer);
    let pty_log_stdout = pty_log.clone();
    let shim_id = args.id.clone();
    let stdout_handle = thread::spawn(move || {
        let (line_tx, line_rx) = mpsc::channel();
        thread::spawn(move || {
            let reader = BufReader::new(child_stdout);
            for line_result in reader.lines() {
                if line_tx.send(line_result).is_err() {
                    break;
                }
            }
        });

        loop {
            let line_result = match stdout_read_timeout(&state_stdout) {
                Some(timeout) => match line_rx.recv_timeout(timeout) {
                    Ok(line_result) => Some(line_result),
                    Err(RecvTimeoutError::Timeout) => {
                        if let Some(forced) = force_stalled_completion(&state_stdout, &shim_id) {
                            emit_forced_completion(&mut evt_channel, &stdin_for_approve, forced);
                        }
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => None,
                },
                None => line_rx.recv().ok(),
            };

            let Some(line_result) = line_result else {
                break;
            };
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
                    let model_name = msg.model_name();
                    // Extract text from the assistant message
                    if let Some(ref message) = msg.message {
                        let model_name = msg.model_name();
                        let text = sdk_types::extract_assistant_text(message);
                        if !text.is_empty() {
                            let mut st = state_stdout.lock().unwrap();
                            if !turn_in_flight(&st) {
                                continue;
                            }
                            if st.last_model_name.is_none() {
                                st.last_model_name = model_name;
                            }
                            st.accumulated_response.push_str(&text);
                            st.cumulative_output_bytes += text.len() as u64;

                            // Update session_id from first response
                            if st.session_id.is_empty() {
                                if let Some(ref sid) = msg.session_id {
                                    st.session_id = sid.clone();
                                }
                            }
                            if st.model.is_none() {
                                st.model = model_name.clone();
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
                            if !turn_in_flight(&st) {
                                continue;
                            }
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
                    if !turn_in_flight(&st) {
                        continue;
                    }

                    // Capture session_id
                    if st.session_id.is_empty() {
                        if let Some(ref sid) = msg.session_id {
                            st.session_id = sid.clone();
                        }
                    }
                    if let Some(model_name) = msg.model_name() {
                        st.last_model_name = Some(model_name);
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
                    let context_warning = proactive_context_warning(
                        &msg,
                        st.last_model_name.as_deref(),
                        st.cumulative_output_bytes,
                        st.started_at.elapsed().as_secs(),
                    );

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

                    if let Some(warning) = context_warning.clone() {
                        let _ = evt_channel.send(&Event::ContextWarning {
                            model: warning.model,
                            output_bytes: warning.output_bytes,
                            uptime_secs: warning.uptime_secs,
                            input_tokens: warning.usage.input_tokens,
                            cached_input_tokens: warning.usage.cached_input_tokens,
                            cache_creation_input_tokens: warning.usage.cache_creation_input_tokens,
                            cache_read_input_tokens: warning.usage.cache_read_input_tokens,
                            output_tokens: warning.usage.output_tokens,
                            reasoning_output_tokens: warning.usage.reasoning_output_tokens,
                            used_tokens: warning.used_tokens,
                            context_limit_tokens: warning.context_limit_tokens,
                            usage_pct: warning.usage_pct,
                        });
                    }

                    // Normal completion: Working → Idle
                    let response = if st.accumulated_response.is_empty() {
                        msg.result.clone().unwrap_or_default()
                    } else {
                        std::mem::take(&mut st.accumulated_response)
                    };
                    if let Some(followup) =
                        common::detect_test_failure_followup(&response, st.test_failure_iterations)
                    {
                        st.pending_message_id = None;
                        st.test_failure_iterations = followup.next_iteration_count;
                        st.last_sent_message_from = Some("batty".into());
                        st.last_sent_message_preview = Some(message_preview(&followup.body));
                        st.state = ShimState::Working;
                        st.state_changed_at = Instant::now();
                        let session_id = st.session_id.clone();
                        drop(st);

                        let text = format_injected_message("batty", &followup.body);
                        let user_msg = SdkUserMessage::new(&session_id, &text);
                        let ndjson = user_msg.to_ndjson();
                        if let Ok(mut writer) = stdin_for_approve.lock() {
                            let _ = writeln!(writer, "{ndjson}");
                            let _ = writer.flush();
                        }
                        let _ = evt_channel.send(&Event::Warning {
                            message: followup.notice,
                            idle_secs: None,
                        });
                        continue;
                    }
                    st.test_failure_iterations = 0;
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
                        st.last_sent_message_from = Some(qm.from.clone());
                        st.last_sent_message_preview = Some(message_preview(&qm.body));
                        st.state = ShimState::Working;
                        st.state_changed_at = Instant::now();
                        st.accumulated_response.clear();
                        st.test_failure_iterations = 0;
                    } else {
                        st.last_sent_message_from = None;
                        st.last_sent_message_preview = None;
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
            let input_tokens = st.cumulative_input_tokens;
            let output_tokens = st.cumulative_output_tokens;
            let context_usage_pct = st.context_usage_pct;
            drop(st);

            if stats_channel
                .send(&Event::SessionStats {
                    output_bytes,
                    uptime_secs,
                    input_tokens,
                    output_tokens,
                    context_usage_pct,
                })
                .is_err()
            {
                return;
            }
        }
    });

    // -- Command loop (main thread) --
    let state_cmd = Arc::clone(&state);
    let mut last_keepalive = Instant::now();
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
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io_error| {
                        matches!(
                            io_error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        )
                    }) =>
            {
                maybe_send_keepalive(&state_cmd, &stdin_writer, &mut last_keepalive);
                continue;
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
                let delivery_id = message_id.clone();
                last_keepalive = Instant::now();
                let mut st = state_cmd.lock().unwrap();
                match st.state {
                    ShimState::Idle => {
                        st.pending_message_id = message_id;
                        st.last_sent_message_from = Some(from.clone());
                        st.last_sent_message_preview = Some(message_preview(&body));
                        st.accumulated_response.clear();
                        st.test_failure_iterations = 0;
                        let session_id = st.session_id.clone();
                        st.state = ShimState::Working;
                        st.state_changed_at = Instant::now();
                        drop(st);

                        let text = format_injected_message(&from, &body);
                        let user_msg = SdkUserMessage::new(&session_id, &text);
                        let ndjson = user_msg.to_ndjson();

                        if let Ok(mut writer) = stdin_writer.lock() {
                            if let Err(e) = writeln!(writer, "{ndjson}") {
                                if let Some(id) = delivery_id {
                                    cmd_channel.send(&Event::DeliveryFailed {
                                        id,
                                        reason: format!("stdin write failed: {e}"),
                                    })?;
                                }
                                cmd_channel.send(&Event::Error {
                                    command: "SendMessage".into(),
                                    reason: format!("stdin write failed: {e}"),
                                })?;
                                continue;
                            }
                            let _ = writer.flush();
                        }

                        if let Some(id) = delivery_id {
                            cmd_channel.send(&Event::MessageDelivered { id })?;
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
                last_keepalive = Instant::now();
                cmd_channel.send(&Event::Pong)?;
            }

            ShimCommand::Shutdown {
                timeout_secs,
                reason,
            } => {
                eprintln!(
                    "[shim-sdk {}] shutdown requested ({}, timeout: {}s)",
                    args.id,
                    reason.label(),
                    timeout_secs
                );
                if let Err(error) = super::runtime::preserve_work_before_kill(&args.cwd) {
                    eprintln!("[shim-sdk {}] work preservation failed: {error}", args.id);
                }
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
                if let Err(error) = super::runtime::preserve_work_before_kill(&args.cwd) {
                    eprintln!("[shim-sdk {}] work preservation failed: {error}", args.id);
                }
                terminate_child(&mut child);
                break;
            }
        }
    }

    stdout_handle.join().ok();
    Ok(())
}

fn maybe_send_keepalive<W: IoWrite>(
    state: &Arc<Mutex<SdkState>>,
    stdin_writer: &Arc<Mutex<W>>,
    last_keepalive: &mut Instant,
) {
    if last_keepalive.elapsed() < Duration::from_secs(SDK_KEEPALIVE_IDLE_SECS) {
        return;
    }

    let session_id = {
        let mut st = state.lock().unwrap();
        if st.state != ShimState::Idle
            || st.session_id.is_empty()
            || st.pending_message_id.is_some()
        {
            return;
        }
        st.state = ShimState::Working;
        st.state_changed_at = Instant::now();
        st.accumulated_response.clear();
        st.test_failure_iterations = 0;
        st.session_id.clone()
    };

    let user_msg = SdkUserMessage::new(&session_id, SDK_KEEPALIVE_MESSAGE);
    let ndjson = user_msg.to_ndjson();
    if let Ok(mut writer) = stdin_writer.lock() {
        if writeln!(writer, "{ndjson}").is_ok() {
            let _ = writer.flush();
            *last_keepalive = Instant::now();
            return;
        }
    }

    let mut st = state.lock().unwrap();
    st.state = ShimState::Idle;
    st.state_changed_at = Instant::now();
}

#[derive(Debug, Clone)]
struct ProactiveContextWarning {
    model: Option<String>,
    usage: sdk_types::SdkTokenUsage,
    output_bytes: u64,
    uptime_secs: u64,
    used_tokens: u64,
    context_limit_tokens: u64,
    usage_pct: u8,
}

fn proactive_context_warning(
    msg: &SdkOutput,
    last_model_name: Option<&str>,
    output_bytes: u64,
    uptime_secs: u64,
) -> Option<ProactiveContextWarning> {
    let usage = msg.token_usage()?;
    let used_tokens = usage.total_tokens();
    if used_tokens == 0 {
        return None;
    }

    let model = msg
        .model_name()
        .or_else(|| last_model_name.map(str::to_string));
    let context_limit_tokens = model_context_limit_tokens(model.as_deref());
    let usage_pct = ((used_tokens.saturating_mul(100)) / context_limit_tokens.max(1)) as u8;
    if usage_pct < PROACTIVE_CONTEXT_WARNING_PCT {
        return None;
    }

    Some(ProactiveContextWarning {
        model,
        usage,
        output_bytes,
        uptime_secs,
        used_tokens,
        context_limit_tokens,
        usage_pct,
    })
}

fn model_context_limit_tokens(model: Option<&str>) -> u64 {
    let Some(model) = model else {
        return DEFAULT_CONTEXT_LIMIT_TOKENS;
    };
    let normalized = model.to_ascii_lowercase();

    if normalized.contains("1m") {
        1_000_000
    } else if normalized.starts_with("claude-") || normalized.contains("claude") {
        200_000
    } else {
        DEFAULT_CONTEXT_LIMIT_TOKENS
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn model_context_usage_pct(model: Option<&str>, total_tokens: u64) -> Option<u8> {
    let limit = model_context_limit_tokens(model?)?;
    Some(((total_tokens.saturating_mul(100)) / limit).min(100) as u8)
}

fn model_context_limit_tokens(model: &str) -> Option<u64> {
    let model = model.to_ascii_lowercase();
    if model.contains("1m") {
        Some(1_000_000)
    } else if model.starts_with("claude") {
        Some(200_000)
    } else {
        None
    }
}

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

fn stdout_read_timeout(state: &Arc<Mutex<SdkState>>) -> Option<Duration> {
    let st = state.lock().unwrap();
    (st.state == ShimState::Working).then_some(WORKING_READ_TIMEOUT)
}

fn turn_in_flight(state: &SdkState) -> bool {
    state.state == ShimState::Working || state.pending_message_id.is_some()
}

fn message_preview(body: &str) -> String {
    let normalized = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= MESSAGE_PREVIEW_LIMIT {
        normalized
    } else {
        let preview: String = normalized.chars().take(MESSAGE_PREVIEW_LIMIT).collect();
        format!("{preview}...")
    }
}

fn stalled_mid_turn_response(from: Option<&str>, preview: Option<&str>) -> String {
    let source = from.unwrap_or("unknown");
    let preview = preview.unwrap_or("(unavailable)");
    format!(
        "{STALLED_MID_TURN_MARKER}: no stdout from Claude SDK for {}s while working.\nlast_sent_message_from: {source}\nlast_sent_message_preview: {preview}",
        WORKING_READ_TIMEOUT.as_secs()
    )
}

fn force_stalled_completion(
    state: &Arc<Mutex<SdkState>>,
    shim_id: &str,
) -> Option<ForcedCompletion> {
    let mut st = state.lock().unwrap();
    if st.state != ShimState::Working {
        return None;
    }

    let response = stalled_mid_turn_response(
        st.last_sent_message_from.as_deref(),
        st.last_sent_message_preview.as_deref(),
    );
    let last_lines = last_n_lines_of(&response, 5);
    let message_id = st.pending_message_id.take();
    let previous_state = st.state;
    let queued_message = st.message_queue.pop_front();

    eprintln!(
        "[shim-sdk {shim_id}] STALL DETECTED after {}s while working",
        WORKING_READ_TIMEOUT.as_secs()
    );

    st.state = ShimState::Idle;
    st.state_changed_at = Instant::now();
    st.accumulated_response.clear();

    if let Some(ref queued) = queued_message {
        st.pending_message_id = queued.message_id.clone();
        st.last_sent_message_from = Some(queued.from.clone());
        st.last_sent_message_preview = Some(message_preview(&queued.body));
        st.state = ShimState::Working;
        st.state_changed_at = Instant::now();
    } else {
        st.last_sent_message_from = None;
        st.last_sent_message_preview = None;
    }

    Some(ForcedCompletion {
        previous_state,
        response,
        last_lines,
        message_id,
        queued_message,
        queue_depth: st.message_queue.len(),
        session_id: st.session_id.clone(),
    })
}

fn emit_forced_completion<W: IoWrite>(
    evt_channel: &mut Channel,
    stdin_writer: &Arc<Mutex<W>>,
    forced: ForcedCompletion,
) {
    let _ = evt_channel.send(&Event::StateChanged {
        from: forced.previous_state,
        to: ShimState::Idle,
        summary: forced.last_lines.clone(),
    });
    let _ = evt_channel.send(&Event::Completion {
        message_id: forced.message_id,
        response: forced.response,
        last_lines: forced.last_lines,
    });

    if let Some(qm) = forced.queued_message {
        let text = format_injected_message(&qm.from, &qm.body);
        let user_msg = SdkUserMessage::new(&forced.session_id, &text);
        let ndjson = user_msg.to_ndjson();
        if let Ok(mut writer) = stdin_writer.lock() {
            let _ = writeln!(writer, "{ndjson}");
            let _ = writer.flush();
        }
        let _ = evt_channel.send(&Event::StateChanged {
            from: ShimState::Idle,
            to: ShimState::Working,
            summary: format!(
                "delivering queued message ({} remaining)",
                forced.queue_depth
            ),
        });
    }
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
            last_sent_message_from: None,
            last_sent_message_preview: None,
            last_model_name: None,
            message_queue: VecDeque::new(),
            cumulative_output_bytes: 0,
            model: None,
            test_failure_iterations: 0,
            cumulative_input_tokens: 0,
            cumulative_output_tokens: 0,
            cumulative_context_tokens: 0,
            context_usage_pct: None,
            context_approaching_emitted: false,
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

    #[test]
    fn message_preview_normalizes_and_truncates() {
        let preview = message_preview("hello\n\nthere    world");
        assert_eq!(preview, "hello there world");

        let long = "x".repeat(MESSAGE_PREVIEW_LIMIT + 10);
        let truncated = message_preview(&long);
        assert!(truncated.ends_with("..."));
        assert!(truncated.len() > MESSAGE_PREVIEW_LIMIT);
    }

    #[test]
    fn model_context_limit_tokens_detects_one_million_alias() {
        assert_eq!(
            model_context_limit_tokens("claude-opus-4-6-1m"),
            Some(1_000_000)
        );
        assert_eq!(
            model_context_limit_tokens("claude-sonnet-4-6"),
            Some(200_000)
        );
        assert_eq!(model_context_limit_tokens("gpt-5.4"), None);
    }

    #[test]
    fn model_context_usage_pct_includes_cache_tokens() {
        assert_eq!(
            model_context_usage_pct(Some("claude-sonnet-4-6"), 180_000),
            Some(90)
        );
        assert_eq!(
            model_context_usage_pct(Some("claude-opus-4-6-1m"), 500_000),
            Some(50)
        );
    }

    #[test]
    fn stalled_mid_turn_response_includes_tracked_message_context() {
        let response = stalled_mid_turn_response(Some("manager"), Some("continue task 496"));
        assert!(response.starts_with(STALLED_MID_TURN_MARKER));
        assert!(response.contains("last_sent_message_from: manager"));
        assert!(response.contains("last_sent_message_preview: continue task 496"));
    }

    #[test]
    fn force_stalled_completion_releases_working_turn() {
        let state = Arc::new(Mutex::new(SdkState {
            state: ShimState::Working,
            state_changed_at: Instant::now(),
            started_at: Instant::now(),
            session_id: "sess-1".into(),
            accumulated_response: "partial output".into(),
            pending_message_id: Some("msg-1".into()),
            last_sent_message_from: Some("manager".into()),
            last_sent_message_preview: Some("continue task".into()),
            last_model_name: Some("claude-sonnet-4-5".into()),
            message_queue: VecDeque::new(),
            cumulative_output_bytes: 12,
            model: None,
            test_failure_iterations: 0,
            cumulative_input_tokens: 0,
            cumulative_output_tokens: 0,
            cumulative_context_tokens: 0,
            context_usage_pct: None,
            context_approaching_emitted: false,
        }));

        let forced = force_stalled_completion(&state, "sdk-test").expect("forced completion");
        assert_eq!(forced.previous_state, ShimState::Working);
        assert_eq!(forced.message_id.as_deref(), Some("msg-1"));
        assert!(forced.response.starts_with(STALLED_MID_TURN_MARKER));

        let st = state.lock().unwrap();
        assert_eq!(st.state, ShimState::Idle);
        assert!(st.pending_message_id.is_none());
        assert!(st.accumulated_response.is_empty());
        assert!(st.last_sent_message_from.is_none());
        assert!(st.last_sent_message_preview.is_none());
    }

    #[test]
    fn force_stalled_completion_promotes_queued_message() {
        let state = Arc::new(Mutex::new(SdkState {
            state: ShimState::Working,
            state_changed_at: Instant::now(),
            started_at: Instant::now(),
            session_id: "sess-2".into(),
            accumulated_response: String::new(),
            pending_message_id: Some("msg-1".into()),
            last_sent_message_from: Some("manager".into()),
            last_sent_message_preview: Some("first".into()),
            last_model_name: Some("claude-sonnet-4-5".into()),
            message_queue: VecDeque::from([QueuedMessage {
                from: "architect".into(),
                body: "second message".into(),
                message_id: Some("msg-2".into()),
            }]),
            cumulative_output_bytes: 0,
            model: None,
            test_failure_iterations: 0,
            cumulative_input_tokens: 0,
            cumulative_output_tokens: 0,
            cumulative_context_tokens: 0,
            context_usage_pct: None,
            context_approaching_emitted: false,
        }));

        let forced = force_stalled_completion(&state, "sdk-test").expect("forced completion");
        assert!(forced.queued_message.is_some());
        assert_eq!(forced.queue_depth, 0);

        let st = state.lock().unwrap();
        assert_eq!(st.state, ShimState::Working);
        assert_eq!(st.pending_message_id.as_deref(), Some("msg-2"));
        assert_eq!(st.last_sent_message_from.as_deref(), Some("architect"));
        assert_eq!(
            st.last_sent_message_preview.as_deref(),
            Some("second message")
        );
    }

    #[test]
    fn keepalive_is_skipped_before_interval() {
        let state = Arc::new(Mutex::new(SdkState {
            state: ShimState::Idle,
            state_changed_at: Instant::now(),
            started_at: Instant::now(),
            session_id: "sess-1".into(),
            accumulated_response: String::new(),
            pending_message_id: None,
            last_sent_message_from: None,
            last_sent_message_preview: None,
            last_model_name: None,
            message_queue: VecDeque::new(),
            cumulative_output_bytes: 0,
            model: None,
            test_failure_iterations: 0,
            cumulative_input_tokens: 0,
            cumulative_output_tokens: 0,
            cumulative_context_tokens: 0,
            context_usage_pct: None,
            context_approaching_emitted: false,
        }));
        let writer = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut last_keepalive = Instant::now();

        maybe_send_keepalive(&state, &writer, &mut last_keepalive);

        assert!(writer.lock().unwrap().is_empty());
        assert_eq!(state.lock().unwrap().state, ShimState::Idle);
    }

    #[test]
    fn keepalive_sends_message_after_interval() {
        let state = Arc::new(Mutex::new(SdkState {
            state: ShimState::Idle,
            state_changed_at: Instant::now(),
            started_at: Instant::now(),
            session_id: "sess-1".into(),
            accumulated_response: "stale output".into(),
            pending_message_id: None,
            last_sent_message_from: None,
            last_sent_message_preview: None,
            last_model_name: None,
            message_queue: VecDeque::new(),
            cumulative_output_bytes: 0,
            model: None,
            test_failure_iterations: 0,
            cumulative_input_tokens: 0,
            cumulative_output_tokens: 0,
            cumulative_context_tokens: 0,
            context_usage_pct: None,
            context_approaching_emitted: false,
        }));
        let writer = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut last_keepalive = Instant::now() - Duration::from_secs(SDK_KEEPALIVE_IDLE_SECS + 1);

        maybe_send_keepalive(&state, &writer, &mut last_keepalive);

        let output = String::from_utf8(writer.lock().unwrap().clone()).unwrap();
        assert!(output.contains("\"type\":\"user\""));
        assert!(output.contains("\"session_id\":\"sess-1\""));
        assert!(output.contains(SDK_KEEPALIVE_MESSAGE));

        let st = state.lock().unwrap();
        assert_eq!(st.state, ShimState::Working);
        assert!(st.accumulated_response.is_empty());
    }

    #[test]
    fn keepalive_is_skipped_without_session() {
        let state = Arc::new(Mutex::new(SdkState {
            state: ShimState::Idle,
            state_changed_at: Instant::now(),
            started_at: Instant::now(),
            session_id: String::new(),
            accumulated_response: String::new(),
            pending_message_id: None,
            last_sent_message_from: None,
            last_sent_message_preview: None,
            last_model_name: None,
            message_queue: VecDeque::new(),
            cumulative_output_bytes: 0,
            model: None,
            test_failure_iterations: 0,
            cumulative_input_tokens: 0,
            cumulative_output_tokens: 0,
            cumulative_context_tokens: 0,
            context_usage_pct: None,
            context_approaching_emitted: false,
        }));
        let writer = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut last_keepalive = Instant::now() - Duration::from_secs(SDK_KEEPALIVE_IDLE_SECS + 1);

        maybe_send_keepalive(&state, &writer, &mut last_keepalive);

        assert!(writer.lock().unwrap().is_empty());
        assert_eq!(state.lock().unwrap().state, ShimState::Idle);
    }

    #[test]
    fn proactive_context_warning_uses_model_aware_limits_and_cache_tokens() {
        let line = r#"{"type":"result","usage":{"input_tokens":100000,"cached_input_tokens":15000,"cache_creation_input_tokens":10000,"cache_read_input_tokens":5000,"output_tokens":20000,"reasoning_output_tokens":10000}}"#;
        let msg: SdkOutput = serde_json::from_str(line).unwrap();
        let warning =
            proactive_context_warning(&msg, Some("claude-sonnet-4-5"), 42_000, 900).unwrap();

        assert_eq!(warning.context_limit_tokens, 200_000);
        assert_eq!(warning.used_tokens, 160_000);
        assert_eq!(warning.usage_pct, 80);
        assert_eq!(warning.model.as_deref(), Some("claude-sonnet-4-5"));
    }

    #[test]
    fn proactive_context_warning_uses_one_million_limit_for_opus_1m() {
        let line = r#"{"type":"result","usage":{"input_tokens":700000,"cached_input_tokens":50000,"cache_creation_input_tokens":20000,"cache_read_input_tokens":10000,"output_tokens":10000,"reasoning_output_tokens":10000}}"#;
        let msg: SdkOutput = serde_json::from_str(line).unwrap();
        let warning =
            proactive_context_warning(&msg, Some("claude-opus-4.6-1m"), 42_000, 900).unwrap();

        assert_eq!(warning.context_limit_tokens, 1_000_000);
        assert_eq!(warning.used_tokens, 800_000);
        assert_eq!(warning.usage_pct, 80);
    }

    #[test]
    fn proactive_context_warning_skips_usage_below_threshold() {
        let line = r#"{"type":"result","usage":{"input_tokens":20000,"cached_input_tokens":1000,"output_tokens":4000}}"#;
        let msg: SdkOutput = serde_json::from_str(line).unwrap();
        assert!(proactive_context_warning(&msg, Some("claude-sonnet-4-5"), 10_000, 120).is_none());
    }
}
