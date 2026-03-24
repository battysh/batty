//! The shim process: owns a PTY, runs an agent CLI, classifies state,
//! communicates with the orchestrator via a Channel on fd 3.
//!
//! Raw PTY output is also streamed to a log file so tmux display panes can
//! `tail -F` it and render agent output in real time.

use std::collections::VecDeque;
use std::io::{Read, Write as IoWrite};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, PtySize};

use super::classifier::{self, AgentType, ScreenVerdict};
use super::protocol::{Channel, Command, Event, ShimState};
use super::pty_log::PtyLogWriter;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

const DEFAULT_ROWS: u16 = 50;
const DEFAULT_COLS: u16 = 220;
const SCROLLBACK_LINES: usize = 5000;

/// How often to check for state changes when no PTY output arrives (ms).
const POLL_INTERVAL_MS: u64 = 250;

/// Max time to wait for agent to show its first prompt (secs).
const READY_TIMEOUT_SECS: u64 = 120;

/// Maximum number of messages that can be queued while the agent is working.
const MAX_QUEUE_DEPTH: usize = 16;

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
}

// ---------------------------------------------------------------------------
// Queued message (buffered while agent is Working)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct QueuedMessage {
    from: String,
    body: String,
    message_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Shared state between PTY reader thread and command handler thread
// ---------------------------------------------------------------------------

struct ShimInner {
    parser: vt100::Parser,
    state: ShimState,
    state_changed_at: Instant,
    last_screen_hash: u64,
    pre_injection_content: String,
    pending_message_id: Option<String>,
    agent_type: AgentType,
    /// Messages queued while the agent is in Working state.
    /// Drained FIFO on Working→Idle transitions.
    message_queue: VecDeque<QueuedMessage>,
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
    let mut cmd = CommandBuilder::new("bash");
    cmd.args(["-c", &args.cmd]);
    cmd.cwd(&args.cwd);
    cmd.env_remove("CLAUDECODE"); // prevent nested detection

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
        pre_injection_content: String::new(),
        pending_message_id: None,
        agent_type: args.agent_type,
        message_queue: VecDeque::new(),
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

                    let new_state = match (old_state, verdict) {
                        (ShimState::Starting, ScreenVerdict::AgentIdle) => Some(ShimState::Idle),
                        (ShimState::Idle, ScreenVerdict::AgentIdle) => None,
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

                        drop(inner); // release lock before I/O

                        let events = build_transition_events(
                            old_state,
                            new,
                            &summary,
                            &pre_content,
                            &current_content,
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
                            let formatted = format!("{}\n", msg.body);
                            let mut writer = pty_writer_pty.lock().unwrap();
                            if let Err(e) = writer.write_all(formatted.as_bytes()) {
                                let _ = evt_channel.send(&Event::Error {
                                    command: "SendMessage".into(),
                                    reason: format!("PTY write failed for queued message: {e}"),
                                });
                            } else {
                                writer.flush().ok();
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

    // -- Main thread: handle commands from orchestrator --
    let inner_cmd = Arc::clone(&inner);

    // Wait for Ready (Starting → Idle transition) with timeout
    let start = Instant::now();
    loop {
        let state = inner_cmd.lock().unwrap().state;
        match state {
            ShimState::Starting => {
                if start.elapsed().as_secs() > READY_TIMEOUT_SECS {
                    let last = inner_cmd.lock().unwrap().last_n_lines(10);
                    cmd_channel.send(&Event::Error {
                        command: "startup".into(),
                        reason: format!(
                            "agent did not show prompt within {}s. Last lines:\n{}",
                            READY_TIMEOUT_SECS, last,
                        ),
                    })?;
                    child.kill().ok();
                    return Ok(());
                }
                std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
            }
            ShimState::Dead => {
                return Ok(());
            }
            _ => {
                cmd_channel.send(&Event::Ready)?;
                break;
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
                child.kill().ok();
                break;
            }
            Err(e) => {
                eprintln!("[shim {}] channel error: {e}", args.id);
                child.kill().ok();
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

                        let formatted = format!("{}\n", body);
                        let mut writer = pty_writer.lock().unwrap();
                        if let Err(e) = writer.write_all(formatted.as_bytes()) {
                            drop(inner);
                            cmd_channel.send(&Event::Error {
                                command: "SendMessage".into(),
                                reason: format!("PTY write failed: {e}"),
                            })?;
                            continue;
                        }
                        writer.flush().ok();

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
                {
                    let mut writer = pty_writer.lock().unwrap();
                    writer.write_all(b"\x03").ok(); // Ctrl-C
                    writer.flush().ok();
                }
                let deadline = Instant::now() + std::time::Duration::from_secs(timeout_secs as u64);
                loop {
                    if Instant::now() > deadline {
                        child.kill().ok();
                        break;
                    }
                    if let Ok(Some(_)) = child.try_wait() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                break;
            }

            Command::Kill => {
                child.kill().ok();
                break;
            }
        }
    }

    pty_handle.join().ok();
    Ok(())
}

// ---------------------------------------------------------------------------
// Drain the message queue, emitting Error events for each dropped message
// ---------------------------------------------------------------------------

fn drain_queue_errors(
    queue: &mut VecDeque<QueuedMessage>,
    terminal_state: ShimState,
) -> Vec<Event> {
    let mut events = Vec::new();
    while let Some(msg) = queue.pop_front() {
        events.push(Event::Error {
            command: "SendMessage".into(),
            reason: format!(
                "agent entered {} state, queued message dropped{}",
                terminal_state,
                msg.message_id
                    .map(|id| format!(" (id: {id})"))
                    .unwrap_or_default(),
            ),
        });
    }
    events
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
    message_id: Option<String>,
) -> Vec<Event> {
    let mut events = vec![Event::StateChanged {
        from,
        to,
        summary: summary.to_string(),
    }];

    // Working → Idle = completion
    if from == ShimState::Working && to == ShimState::Idle {
        let response = extract_response(pre_injection_content, current_content);
        events.push(Event::Completion {
            message_id,
            response,
            last_lines: summary.to_string(),
        });
    }

    // Any → ContextExhausted
    if to == ShimState::ContextExhausted {
        events.push(Event::ContextExhausted {
            message: "Agent reported context exhaustion".to_string(),
            last_lines: summary.to_string(),
        });
    }

    events
}

/// Extract the agent's response by diffing pre-injection and post-completion
/// screen content.
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

    // Strip trailing empty lines and prompt lines
    let mut end = response_lines.len();
    while end > 0 && response_lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    while end > 0 && is_prompt_line(response_lines[end - 1].trim()) {
        end -= 1;
    }
    while end > 0 && response_lines[end - 1].trim().is_empty() {
        end -= 1;
    }

    response_lines[..end].join("\n")
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
}
