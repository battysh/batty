//! Kiro ACP-mode shim runtime: communicates with Kiro CLI via JSON-RPC 2.0
//! on stdin/stdout using the Agent Client Protocol (ACP).
//!
//! Like the Claude SDK runtime (`runtime_sdk.rs`), this uses a persistent
//! subprocess with bidirectional NDJSON. The protocol differs: ACP requires
//! an initialization handshake (`initialize` + `session/new`) before prompts
//! can be sent, and uses JSON-RPC 2.0 framing.
//!
//! Emits the same `Command`/`Event` protocol to the orchestrator as all other
//! runtimes, making it transparent to upstream consumers.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write as IoWrite};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::common::{
    self, MAX_QUEUE_DEPTH, QueuedMessage, SESSION_STATS_INTERVAL_SECS, drain_queue_errors,
    format_injected_message,
};
use super::kiro_types::{self, AcpMessage};
use super::protocol::{Channel, Command as ShimCommand, Event, ShimState};
use super::pty_log::PtyLogWriter;
use super::runtime::ShimArgs;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

const PROCESS_EXIT_POLL_MS: u64 = 100;
const GROUP_TERM_GRACE_SECS: u64 = 2;

/// Timeout for the initialization handshake (initialize + session/new).
const INIT_TIMEOUT_SECS: u64 = 30;

/// Context usage percentage threshold to consider context exhausted.
const CONTEXT_EXHAUSTION_THRESHOLD: f64 = 98.0;

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

struct KiroState {
    state: ShimState,
    state_changed_at: Instant,
    started_at: Instant,
    /// ACP session ID from `session/new` response.
    session_id: String,
    /// Accumulated assistant response text for the current turn.
    accumulated_response: String,
    /// Message ID of the currently pending (in-flight) message.
    pending_message_id: Option<String>,
    /// Messages queued while the agent is in Working state.
    message_queue: VecDeque<QueuedMessage>,
    /// Total bytes of response text received.
    cumulative_output_bytes: u64,
    /// Whether the initialization handshake is complete.
    initialized: bool,
    /// Whether we've sent the session/new request (awaiting its response).
    sent_session_new: bool,
    /// Pending prompt request ID (to match result).
    pending_prompt_request_id: Option<u64>,
}

/// Monotonically increasing JSON-RPC request ID counter.
static REQUEST_ID: AtomicU64 = AtomicU64::new(0);

fn next_request_id() -> u64 {
    REQUEST_ID.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

/// Run the Kiro ACP-mode shim. Does not return until the shim exits.
///
/// `channel` is the pre-connected socket to the orchestrator (fd 3 or socketpair).
/// `args.cmd` must launch `kiro-cli acp --trust-all-tools`.
pub fn run_kiro_acp(args: ShimArgs, channel: Channel) -> Result<()> {
    // -- Spawn subprocess with piped stdin/stdout/stderr --
    let mut child = Command::new("bash")
        .args(["-lc", &args.cmd])
        .current_dir(&args.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("CLAUDECODE")
        .spawn()
        .with_context(|| format!("[shim-kiro {}] failed to spawn kiro-cli acp", args.id))?;

    let child_pid = child.id();
    eprintln!(
        "[shim-kiro {}] spawned kiro-cli acp (pid {})",
        args.id, child_pid
    );

    let child_stdin = child.stdin.take().context("failed to take child stdin")?;
    let child_stdout = child.stdout.take().context("failed to take child stdout")?;
    let child_stderr = child.stderr.take().context("failed to take child stderr")?;

    // Shared state
    let state = Arc::new(Mutex::new(KiroState {
        state: ShimState::Starting,
        state_changed_at: Instant::now(),
        started_at: Instant::now(),
        session_id: String::new(),
        accumulated_response: String::new(),
        pending_message_id: None,
        message_queue: VecDeque::new(),
        cumulative_output_bytes: 0,
        initialized: false,
        sent_session_new: false,
        pending_prompt_request_id: None,
    }));

    // Shared stdin writer — wrapped in Option so Shutdown can take and close it.
    let stdin_writer = Arc::new(Mutex::new(Some(child_stdin)));

    // PTY log writer (optional)
    let pty_log: Option<Arc<Mutex<PtyLogWriter>>> = args
        .pty_log_path
        .as_deref()
        .map(|p| PtyLogWriter::new(p).context("failed to create PTY log"))
        .transpose()?
        .map(|w| Arc::new(Mutex::new(w)));

    // Channel clones
    let mut cmd_channel = channel;
    let mut evt_channel = cmd_channel
        .try_clone()
        .context("failed to clone channel for stdout reader")?;

    // -- Send initialization handshake --
    {
        let init_req = kiro_types::initialize_request(next_request_id());
        let ndjson = init_req.to_ndjson();
        write_stdin(&stdin_writer, &ndjson);
        eprintln!("[shim-kiro {}] sent initialize request", args.id);
    }

    // -- stdout reader thread --
    let state_stdout = Arc::clone(&state);
    let stdin_for_approve = Arc::clone(&stdin_writer);
    let pty_log_stdout = pty_log.clone();
    let shim_id = args.id.clone();
    let cwd_for_init = args.cwd.to_string_lossy().to_string();
    let stdout_handle = thread::spawn(move || {
        let reader = BufReader::new(child_stdout);
        for line_result in reader.lines() {
            let line = match line_result {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("[shim-kiro {shim_id}] stdout read error: {e}");
                    break;
                }
            };

            if line.trim().is_empty() {
                continue;
            }

            let msg: AcpMessage = match serde_json::from_str(&line) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("[shim-kiro {shim_id}] ignoring unparseable NDJSON: {e}");
                    continue;
                }
            };

            // -- Handle responses to our requests --
            if msg.is_response() {
                let msg_id = msg.id.unwrap();

                if let Some(ref error) = msg.error {
                    eprintln!("[shim-kiro {shim_id}] JSON-RPC error (id={msg_id}): {error}");
                    // Check if this was a prompt request that failed
                    let mut st = state_stdout.lock().unwrap();
                    if st.pending_prompt_request_id == Some(msg_id) {
                        st.pending_prompt_request_id = None;
                        let error_text = error
                            .get("message")
                            .and_then(|m| m.as_str())
                            .unwrap_or("unknown error");

                        if common::detect_context_exhausted(error_text) {
                            let last_lines = last_n_lines_of(&st.accumulated_response, 5);
                            let old = st.state;
                            st.state = ShimState::ContextExhausted;
                            st.state_changed_at = Instant::now();
                            let drain = drain_queue_errors(
                                &mut st.message_queue,
                                ShimState::ContextExhausted,
                            );
                            drop(st);
                            let _ = evt_channel.send(&Event::StateChanged {
                                from: old,
                                to: ShimState::ContextExhausted,
                                summary: last_lines.clone(),
                            });
                            let _ = evt_channel.send(&Event::ContextExhausted {
                                message: error_text.to_string(),
                                last_lines,
                            });
                            for event in drain {
                                let _ = evt_channel.send(&event);
                            }
                        } else {
                            // Non-exhaustion error on prompt — complete with error
                            let response = std::mem::take(&mut st.accumulated_response);
                            let last_lines = last_n_lines_of(&response, 5);
                            let msg_id_out = st.pending_message_id.take();
                            st.state = ShimState::Idle;
                            st.state_changed_at = Instant::now();
                            drop(st);
                            let _ = evt_channel.send(&Event::StateChanged {
                                from: ShimState::Working,
                                to: ShimState::Idle,
                                summary: last_lines.clone(),
                            });
                            let _ = evt_channel.send(&Event::Completion {
                                message_id: msg_id_out,
                                response: format!("[error] {error_text}"),
                                last_lines,
                            });
                        }
                    }
                    continue;
                }

                if let Some(ref result) = msg.result {
                    let mut st = state_stdout.lock().unwrap();

                    // Check if this is the initialize response (before session/new was sent)
                    if !st.initialized && !st.sent_session_new {
                        // This is the initialize response — now send session/new
                        st.sent_session_new = true;
                        drop(st);
                        let session_req =
                            kiro_types::session_new_request(next_request_id(), &cwd_for_init);
                        let ndjson = session_req.to_ndjson();
                        write_stdin(&stdin_for_approve, &ndjson);
                        eprintln!("[shim-kiro {shim_id}] sent session/new request");
                        continue;
                    }

                    // Check if this is the session/new response
                    if !st.initialized {
                        if let Some(sid) = kiro_types::extract_session_id(result) {
                            st.session_id = sid.to_string();
                            st.initialized = true;
                            st.state = ShimState::Idle;
                            st.state_changed_at = Instant::now();
                            eprintln!("[shim-kiro {shim_id}] session created: {}", st.session_id);
                            drop(st);

                            // Now emit Ready — agent is ready for messages
                            let _ = evt_channel.send(&Event::Ready);
                        }
                        continue;
                    }

                    // Check if this is a prompt result (turn completed)
                    if st.pending_prompt_request_id == Some(msg_id) {
                        st.pending_prompt_request_id = None;
                        let response = if st.accumulated_response.is_empty() {
                            result
                                .get("result")
                                .and_then(|r| r.as_str())
                                .unwrap_or("")
                                .to_string()
                        } else {
                            std::mem::take(&mut st.accumulated_response)
                        };
                        let last_lines = last_n_lines_of(&response, 5);
                        let completed_msg_id = st.pending_message_id.take();
                        let old = st.state;
                        st.state = ShimState::Idle;
                        st.state_changed_at = Instant::now();

                        // Drain queue
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
                        let session_id = st.session_id.clone();
                        let queue_depth = st.message_queue.len();
                        drop(st);

                        let _ = evt_channel.send(&Event::StateChanged {
                            from: old,
                            to: ShimState::Idle,
                            summary: last_lines.clone(),
                        });
                        let _ = evt_channel.send(&Event::Completion {
                            message_id: completed_msg_id,
                            response,
                            last_lines,
                        });

                        // Inject queued message
                        if let Some(qm) = queued_msg {
                            let text = format_injected_message(&qm.from, &qm.body);
                            let req_id = next_request_id();
                            let prompt_req =
                                kiro_types::session_prompt_request(req_id, &session_id, &text);
                            let ndjson = prompt_req.to_ndjson();
                            write_stdin(&stdin_for_approve, &ndjson);
                            let mut st = state_stdout.lock().unwrap();
                            st.pending_prompt_request_id = Some(req_id);
                            drop(st);

                            let _ = evt_channel.send(&Event::StateChanged {
                                from: ShimState::Idle,
                                to: ShimState::Working,
                                summary: format!(
                                    "delivering queued message ({queue_depth} remaining)"
                                ),
                            });
                        }
                    }
                }
                continue;
            }

            // -- Handle notifications --
            if msg.is_notification() {
                let method = msg.method.as_deref().unwrap_or("");
                let params = msg.params.as_ref();

                match method {
                    "session/update" => {
                        if let Some(params) = params {
                            let update_type = kiro_types::extract_update_type(params).unwrap_or("");

                            match update_type {
                                "agent_message_chunk" | "AgentMessageChunk" => {
                                    if let Some(text) =
                                        kiro_types::extract_message_chunk_text(params)
                                    {
                                        if !text.is_empty() {
                                            let mut st = state_stdout.lock().unwrap();
                                            st.accumulated_response.push_str(text);
                                            st.cumulative_output_bytes += text.len() as u64;
                                            drop(st);

                                            if let Some(ref log) = pty_log_stdout {
                                                let _ = log.lock().unwrap().write(text.as_bytes());
                                            }
                                        }
                                    }
                                }

                                "agent_thought_chunk" => {
                                    // Agent thinking — log but don't accumulate
                                    if let Some(text) =
                                        kiro_types::extract_message_chunk_text(params)
                                    {
                                        if let Some(ref log) = pty_log_stdout {
                                            let _ = log
                                                .lock()
                                                .unwrap()
                                                .write(format!("[thought] {text}").as_bytes());
                                        }
                                    }
                                }

                                "tool_call" | "ToolCall" => {
                                    // Log tool calls for visibility
                                    let title = params
                                        .get("update")
                                        .and_then(|u| u.get("title"))
                                        .and_then(|t| t.as_str())
                                        .unwrap_or("unknown tool");
                                    if let Some(ref log) = pty_log_stdout {
                                        let _ = log
                                            .lock()
                                            .unwrap()
                                            .write(format!("[tool] {title}\n").as_bytes());
                                    }
                                }

                                "tool_call_update" | "ToolCallUpdate" => {
                                    // Tool progress — no action needed
                                }

                                "TurnEnd" | "turn_end" => {
                                    // Turn ended via notification.
                                    // The prompt result response will handle the state transition,
                                    // but some ACP agents send TurnEnd before the result.
                                    // We do nothing here — the result handler above transitions state.
                                }

                                _ => {
                                    // Unknown update type — silently ignore (future-proof)
                                }
                            }
                        }
                    }

                    "_kiro.dev/metadata" => {
                        // Check for context exhaustion via high usage percentage
                        if let Some(params) = params {
                            if let Some(usage) = kiro_types::extract_context_usage(params) {
                                if usage >= CONTEXT_EXHAUSTION_THRESHOLD {
                                    let mut st = state_stdout.lock().unwrap();
                                    let last_lines = last_n_lines_of(&st.accumulated_response, 5);
                                    let old = st.state;
                                    st.state = ShimState::ContextExhausted;
                                    st.state_changed_at = Instant::now();
                                    let drain = drain_queue_errors(
                                        &mut st.message_queue,
                                        ShimState::ContextExhausted,
                                    );
                                    drop(st);

                                    let _ = evt_channel.send(&Event::StateChanged {
                                        from: old,
                                        to: ShimState::ContextExhausted,
                                        summary: last_lines.clone(),
                                    });
                                    let _ = evt_channel.send(&Event::ContextExhausted {
                                        message: format!("context usage at {usage:.1}%"),
                                        last_lines,
                                    });
                                    for event in drain {
                                        let _ = evt_channel.send(&event);
                                    }
                                }
                            }
                        }
                    }

                    "_kiro.dev/compaction/status" | "_kiro.dev/clear/status" => {
                        // Informational — log only
                        eprintln!("[shim-kiro {shim_id}] {method}: {params:?}");
                    }

                    _ => {
                        // Unknown notification — silently ignore
                    }
                }
                continue;
            }

            // -- Handle agent-initiated requests --
            if msg.is_agent_request() {
                let method = msg.method.as_deref().unwrap_or("");
                let request_id = msg.id.unwrap();

                match method {
                    "session/request_permission" => {
                        // Auto-approve all permission requests
                        let resp = kiro_types::permission_approve_response(request_id);
                        let ndjson = resp.to_ndjson();
                        write_stdin(&stdin_for_approve, &ndjson);
                        eprintln!(
                            "[shim-kiro {shim_id}] auto-approved permission request {request_id}"
                        );
                    }

                    "fs/read_text_file" | "fs/write_text_file" | "terminal/create"
                    | "terminal/kill" => {
                        // Client-side operations — we don't support these, respond with error
                        let error_resp = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": request_id,
                            "error": {
                                "code": -32601,
                                "message": format!("method not supported by batty shim: {method}")
                            }
                        });
                        let ndjson = serde_json::to_string(&error_resp).unwrap();
                        write_stdin(&stdin_for_approve, &ndjson);
                    }

                    _ => {
                        // Unknown agent request — respond with method not found
                        let error_resp = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": request_id,
                            "error": {
                                "code": -32601,
                                "message": format!("unknown method: {method}")
                            }
                        });
                        let ndjson = serde_json::to_string(&error_resp).unwrap();
                        write_stdin(&stdin_for_approve, &ndjson);
                    }
                }
                continue;
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
                    eprintln!("[shim-kiro {shim_id_err}] stderr: {line}");
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

    // -- Wait for initialization to complete before accepting commands --
    {
        let deadline = Instant::now() + Duration::from_secs(INIT_TIMEOUT_SECS);
        loop {
            let st = state.lock().unwrap();
            if st.initialized {
                break;
            }
            if st.state == ShimState::Dead {
                eprintln!("[shim-kiro {}] agent died during initialization", args.id);
                return Ok(());
            }
            drop(st);

            if Instant::now() > deadline {
                eprintln!(
                    "[shim-kiro {}] initialization timed out after {}s",
                    args.id, INIT_TIMEOUT_SECS
                );
                terminate_child(&mut child);
                return Ok(());
            }
            thread::sleep(Duration::from_millis(PROCESS_EXIT_POLL_MS));
        }
    }

    // -- Command loop (main thread) --
    let state_cmd = Arc::clone(&state);
    loop {
        let cmd = match cmd_channel.recv::<ShimCommand>() {
            Ok(Some(c)) => c,
            Ok(None) => {
                eprintln!(
                    "[shim-kiro {}] orchestrator disconnected, shutting down",
                    args.id
                );
                terminate_child(&mut child);
                break;
            }
            Err(e) => {
                eprintln!("[shim-kiro {}] channel error: {e}", args.id);
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
                let mut st = state_cmd.lock().unwrap();
                match st.state {
                    ShimState::Idle => {
                        st.pending_message_id = message_id;
                        st.accumulated_response.clear();
                        let session_id = st.session_id.clone();
                        st.state = ShimState::Working;
                        st.state_changed_at = Instant::now();

                        let req_id = next_request_id();
                        st.pending_prompt_request_id = Some(req_id);
                        drop(st);

                        let text = format_injected_message(&from, &body);
                        let prompt_req =
                            kiro_types::session_prompt_request(req_id, &session_id, &text);
                        let ndjson = prompt_req.to_ndjson();

                        if !write_stdin(&stdin_writer, &ndjson) {
                            if let Some(id) = delivery_id {
                                cmd_channel.send(&Event::DeliveryFailed {
                                    id,
                                    reason: "stdin write failed (closed)".into(),
                                })?;
                            }
                            cmd_channel.send(&Event::Error {
                                command: "SendMessage".into(),
                                reason: "stdin write failed (closed)".into(),
                            })?;
                            continue;
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
                // No-op in ACP mode — no PTY.
            }

            ShimCommand::Ping => {
                cmd_channel.send(&Event::Pong)?;
            }

            ShimCommand::Shutdown {
                timeout_secs,
                reason,
            } => {
                eprintln!(
                    "[shim-kiro {}] shutdown requested ({}, timeout: {}s)",
                    args.id,
                    reason.label(),
                    timeout_secs
                );
                if let Err(error) = super::runtime::preserve_work_before_kill(&args.cwd) {
                    eprintln!("[shim-kiro {}] work preservation failed: {error}", args.id);
                }
                // Take stdin out of the shared Option to truly close it.
                // The stdout reader thread also holds an Arc clone, but taking
                // from the Option means both sides see None.
                if let Ok(mut guard) = stdin_writer.lock() {
                    guard.take(); // closes ChildStdin
                }
                terminate_child(&mut child);

                // Give the child a moment to fully exit.
                let deadline = Instant::now() + Duration::from_secs(timeout_secs as u64);
                loop {
                    if Instant::now() > deadline {
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
                    eprintln!("[shim-kiro {}] work preservation failed: {error}", args.id);
                }
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

/// Write an NDJSON line to the shared stdin, if it's still open.
fn write_stdin(stdin: &Arc<Mutex<Option<std::process::ChildStdin>>>, line: &str) -> bool {
    if let Ok(mut guard) = stdin.lock() {
        if let Some(ref mut writer) = *guard {
            if writeln!(writer, "{line}").is_ok() {
                let _ = writer.flush();
                return true;
            }
        }
    }
    false
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
    fn kiro_state_initial_values() {
        let st = KiroState {
            state: ShimState::Starting,
            state_changed_at: Instant::now(),
            started_at: Instant::now(),
            session_id: String::new(),
            accumulated_response: String::new(),
            pending_message_id: None,
            message_queue: VecDeque::new(),
            cumulative_output_bytes: 0,
            initialized: false,
            sent_session_new: false,
            pending_prompt_request_id: None,
        };
        assert_eq!(st.state, ShimState::Starting);
        assert!(st.session_id.is_empty());
        assert!(!st.initialized);
        assert!(!st.sent_session_new);
        assert!(st.message_queue.is_empty());
    }

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

    #[test]
    fn context_exhaustion_threshold() {
        let threshold = CONTEXT_EXHAUSTION_THRESHOLD;
        assert!(threshold >= 95.0);
        assert!(threshold <= 100.0);
    }

    #[test]
    fn next_request_id_increments() {
        let a = next_request_id();
        let b = next_request_id();
        assert!(b > a);
    }
}
