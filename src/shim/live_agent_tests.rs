//! Live-agent integration tests for the shim runtime.
//!
//! These tests spawn **real** agent CLIs (Claude, Codex, Kiro) through the
//! shim and validate end-to-end message passing, response extraction, and
//! state transition logging.
//!
//! Gated behind `live-agent` feature flag.
//! Run with: cargo test --features live-agent live_agent -- --nocapture
//!
//! **Prerequisites:**
//! - `claude` CLI installed and authenticated
//! - `codex` CLI installed and authenticated
//! - `kiro` CLI installed and authenticated (optional — test skips if missing)
//!
//! These tests cost real API tokens. Each test sends a single short message.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::shim::classifier::AgentType;
use crate::shim::protocol::{self, Channel, Command, Event, ShimState};
use crate::shim::runtime::ShimArgs;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn is_timeout_error(e: &anyhow::Error) -> bool {
    if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
        matches!(
            io_err.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        )
    } else {
        false
    }
}

fn binary_available(name: &str) -> bool {
    std::process::Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn kiro_command() -> Option<&'static str> {
    if binary_available("kiro-cli") {
        Some("kiro-cli")
    } else if binary_available("kiro") {
        Some("kiro")
    } else {
        None
    }
}

/// Capture the current screen content from the shim for diagnostics.
fn capture_screen(ch: &mut Channel) -> Option<String> {
    if ch
        .send(&Command::CaptureScreen {
            last_n_lines: Some(40),
        })
        .is_err()
    {
        return None;
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if Instant::now() > deadline {
            return None;
        }
        match ch.recv::<Event>() {
            Ok(Some(Event::ScreenCapture { content, .. })) => return Some(content),
            Ok(Some(_)) => continue,
            Err(e) if is_timeout_error(&e) => continue,
            _ => return None,
        }
    }
}

/// Spawn a shim with the given agent type and command.
fn spawn_agent_shim(agent_type: AgentType, cmd: &str) -> Channel {
    let (parent_sock, child_sock) = protocol::socketpair().unwrap();
    let channel = Channel::new(child_sock);

    let args = ShimArgs {
        id: format!("live-{agent_type}"),
        agent_type,
        cmd: cmd.to_string(),
        cwd: PathBuf::from("/tmp"),
        rows: 50,
        cols: 220,
        pty_log_path: None,
        graceful_shutdown_timeout_secs: 5,
        auto_commit_on_restart: true,
    };

    std::thread::spawn(move || {
        crate::shim::runtime::run(args, channel).ok();
    });

    let mut ch = Channel::new(parent_sock);
    ch.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    ch
}

/// Spawn a shim with PTY log for timestamp verification.
fn spawn_agent_shim_with_log(agent_type: AgentType, cmd: &str, log_path: PathBuf) -> Channel {
    let (parent_sock, child_sock) = protocol::socketpair().unwrap();
    let channel = Channel::new(child_sock);

    let args = ShimArgs {
        id: format!("live-{agent_type}-log"),
        agent_type,
        cmd: cmd.to_string(),
        cwd: PathBuf::from("/tmp"),
        rows: 50,
        cols: 220,
        pty_log_path: Some(log_path),
        graceful_shutdown_timeout_secs: 5,
        auto_commit_on_restart: true,
    };

    std::thread::spawn(move || {
        crate::shim::runtime::run(args, channel).ok();
    });

    let mut ch = Channel::new(parent_sock);
    ch.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    ch
}

/// Wait for Ready event with generous timeout.
fn wait_for_ready(ch: &mut Channel) -> bool {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if Instant::now() > deadline {
            return false;
        }
        match ch.recv::<Event>() {
            Ok(Some(Event::Ready)) => return true,
            Ok(Some(Event::StateChanged { .. })) => continue,
            Ok(Some(Event::Error { reason, .. })) => {
                eprintln!("[live-agent] startup error: {reason}");
                return false;
            }
            Ok(Some(Event::Died { last_lines, .. })) => {
                eprintln!("[live-agent] agent died during startup: {last_lines}");
                return false;
            }
            Ok(Some(_)) => continue,
            Ok(None) => return false,
            Err(e) if is_timeout_error(&e) => continue,
            Err(_) => return false,
        }
    }
}

/// Wait for Ready, then capture screen to verify the agent is truly idle.
/// Returns the screen content at idle for diagnostics.
fn wait_for_ready_and_verify(ch: &mut Channel) -> (bool, Option<String>) {
    if !wait_for_ready(ch) {
        let screen = capture_screen(ch);
        eprintln!(
            "[live-agent] FAILED to reach Ready. Screen dump:\n{}",
            screen.as_deref().unwrap_or("<no screen>")
        );
        return (false, screen);
    }
    // Small delay for any post-Ready UI rendering (e.g., confirmation dialogs)
    std::thread::sleep(Duration::from_millis(500));
    let screen = capture_screen(ch);
    if let Some(ref content) = screen {
        eprintln!(
            "[live-agent] Screen at idle ({} chars):\n{}",
            content.len(),
            &content[..content
                .char_indices()
                .nth(600)
                .map_or(content.len(), |(i, _)| i)]
        );
        // Warn about known blocking dialogs
        let lower = content.to_lowercase();
        if lower.contains("enter to confirm") || lower.contains("esc to cancel") {
            eprintln!(
                "[live-agent] WARNING: Agent appears to be showing a confirmation dialog. \
                 Messages sent now will go to the dialog, not the agent prompt."
            );
        }
    }
    (true, screen)
}

/// Collect all events until a Completion arrives or timeout.
/// On timeout, captures screen for diagnostics.
fn collect_until_completion(ch: &mut Channel, timeout: Duration) -> (Vec<Event>, Option<Event>) {
    let deadline = Instant::now() + timeout;
    let mut events = Vec::new();
    loop {
        if Instant::now() > deadline {
            // Timeout — capture screen for diagnostics
            let screen = capture_screen(ch);
            eprintln!(
                "[live-agent] TIMEOUT waiting for Completion after {:.0}s.\n\
                 Events collected ({}):\n{events:#?}\n\
                 Screen dump:\n{}",
                timeout.as_secs_f64(),
                events.len(),
                screen.as_deref().unwrap_or("<no screen>")
            );
            return (events, None);
        }
        match ch.recv::<Event>() {
            Ok(Some(evt)) => {
                let is_completion = matches!(&evt, Event::Completion { .. });
                let is_died = matches!(&evt, Event::Died { .. });
                events.push(evt.clone());
                if is_completion {
                    return (events, Some(evt));
                }
                if is_died {
                    eprintln!("[live-agent] Agent died before Completion. Events: {events:#?}");
                    return (events, None);
                }
            }
            Ok(None) => {
                eprintln!("[live-agent] Channel closed. Events: {events:#?}");
                return (events, None);
            }
            Err(e) if is_timeout_error(&e) => continue,
            Err(e) => {
                eprintln!("[live-agent] Channel error: {e}. Events: {events:#?}");
                return (events, None);
            }
        }
    }
}

fn shutdown(ch: &mut Channel) {
    let _ = ch.send(&Command::Shutdown { timeout_secs: 5 });
}

// =========================================================================
// Test 0a: Diagnostic — dump kiro-cli after message injection
// =========================================================================

#[test]
#[cfg_attr(not(feature = "live-agent"), ignore)]
fn live_kiro_injection_diagnostic() {
    if !binary_available("kiro-cli") {
        eprintln!("SKIP: kiro-cli not found on PATH");
        return;
    }

    let mut ch = spawn_agent_shim(AgentType::Kiro, "kiro-cli");
    let (ready, _) = wait_for_ready_and_verify(&mut ch);
    assert!(ready, "kiro-cli did not become ready");

    // Send message
    ch.send(&Command::SendMessage {
        from: "test".into(),
        body: "what is 2+2? reply with just the number".into(),
        message_id: None,
    })
    .unwrap();

    // Wait 15s for kiro to process
    std::thread::sleep(Duration::from_secs(15));

    // Capture screen
    if let Some(screen) = capture_screen(&mut ch) {
        eprintln!("[kiro-diag] Screen 5s after injection:\n{screen}");
    } else {
        eprintln!("[kiro-diag] Screen capture failed");
    }

    // Collect events for a few more seconds
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match ch.recv::<Event>() {
            Ok(Some(evt)) => eprintln!("[kiro-diag] event: {evt:?}"),
            Err(e) if is_timeout_error(&e) => continue,
            _ => break,
        }
    }

    shutdown(&mut ch);
}

// =========================================================================
// Test 0b: Diagnostic — dump Claude startup sequence
// =========================================================================

#[test]
#[cfg_attr(not(feature = "live-agent"), ignore)]
fn live_claude_startup_diagnostic() {
    if !binary_available("claude") {
        eprintln!("SKIP: claude CLI not found on PATH");
        return;
    }

    let mut ch = spawn_agent_shim(AgentType::Claude, "claude --dangerously-skip-permissions");

    // Collect all events for up to 60s, dumping screen every 10s
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut last_screen_dump = Instant::now();
    let mut events = Vec::new();
    let mut got_ready = false;

    while Instant::now() < deadline {
        match ch.recv::<Event>() {
            Ok(Some(evt)) => {
                eprintln!("[diag] event: {evt:?}");
                if matches!(&evt, Event::Ready) {
                    got_ready = true;
                }
                events.push(evt);
                if got_ready {
                    break;
                }
            }
            Ok(None) => {
                eprintln!("[diag] channel closed");
                break;
            }
            Err(e) if is_timeout_error(&e) => {
                // Periodically dump screen
                if last_screen_dump.elapsed() > Duration::from_secs(10) {
                    if let Some(screen) = capture_screen(&mut ch) {
                        eprintln!(
                            "[diag] screen at +{:.0}s ({} chars):\n{}",
                            Instant::now()
                                .duration_since(deadline - Duration::from_secs(60))
                                .as_secs_f64(),
                            screen.len(),
                            &screen[..screen
                                .char_indices()
                                .nth(800)
                                .map_or(screen.len(), |(i, _)| i)]
                        );
                    } else {
                        eprintln!(
                            "[diag] screen capture failed at +{:.0}s",
                            Instant::now()
                                .duration_since(deadline - Duration::from_secs(60))
                                .as_secs_f64()
                        );
                    }
                    last_screen_dump = Instant::now();
                }
            }
            Err(e) => {
                eprintln!("[diag] channel error: {e}");
                break;
            }
        }
    }

    eprintln!("[diag] total events: {}", events.len());
    eprintln!("[diag] got_ready: {got_ready}");

    // Final screen dump
    if let Some(screen) = capture_screen(&mut ch) {
        eprintln!("[diag] final screen:\n{screen}");
    }

    shutdown(&mut ch);

    if !got_ready {
        panic!("Claude did not reach Ready within 60s. Events: {events:#?}");
    }
}

// =========================================================================
// Test 1: Message delivery — message reaches the agent and triggers Working
// =========================================================================

#[test]
#[cfg_attr(not(feature = "live-agent"), ignore)]
fn live_claude_message_delivered() {
    if !binary_available("claude") {
        eprintln!("SKIP: claude CLI not found on PATH");
        return;
    }

    let mut ch = spawn_agent_shim(AgentType::Claude, "claude --dangerously-skip-permissions");
    let (ready, idle_screen) = wait_for_ready_and_verify(&mut ch);
    assert!(ready, "Claude did not become ready within 120s");

    // Diagnostic: check if stuck on confirmation dialog
    if let Some(ref s) = idle_screen {
        if s.to_lowercase().contains("enter to confirm") {
            eprintln!(
                "[live-claude] BUG: Classifier reported Idle but agent is showing \
                 a confirmation dialog. The ❯ prompt character is visible behind \
                 the dialog, causing a false-positive idle detection."
            );
        }
    }

    ch.send(&Command::SendMessage {
        from: "test".into(),
        body: "Reply with exactly: BATTY_TEST_OK".into(),
        message_id: Some("live-claude-1".into()),
    })
    .unwrap();

    let (events, completion) = collect_until_completion(&mut ch, Duration::from_secs(90));

    let saw_working = events.iter().any(|e| {
        matches!(
            e,
            Event::StateChanged {
                to: ShimState::Working,
                ..
            }
        )
    });
    assert!(
        saw_working,
        "Should see Idle→Working transition (message was delivered)"
    );

    assert!(completion.is_some(), "Should receive Completion event");

    shutdown(&mut ch);
}

#[test]
#[cfg_attr(not(feature = "live-agent"), ignore)]
fn live_codex_message_delivered() {
    if !binary_available("codex") {
        eprintln!("SKIP: codex CLI not found on PATH");
        return;
    }

    let mut ch = spawn_agent_shim(AgentType::Codex, "codex");
    let (ready, _) = wait_for_ready_and_verify(&mut ch);
    assert!(ready, "Codex did not become ready within 120s");

    ch.send(&Command::SendMessage {
        from: "test".into(),
        body: "Reply with exactly: BATTY_TEST_OK".into(),
        message_id: Some("live-codex-1".into()),
    })
    .unwrap();

    let (events, completion) = collect_until_completion(&mut ch, Duration::from_secs(90));

    let saw_working = events.iter().any(|e| {
        matches!(
            e,
            Event::StateChanged {
                to: ShimState::Working,
                ..
            }
        )
    });
    assert!(saw_working, "Should see Working transition for Codex");
    assert!(completion.is_some(), "Should receive Completion for Codex");

    shutdown(&mut ch);
}

#[test]
#[cfg_attr(not(feature = "live-agent"), ignore)]
fn live_kiro_message_delivered() {
    let Some(kiro_cmd) = kiro_command() else {
        eprintln!("SKIP: kiro-cli not found on PATH");
        return;
    };

    let mut ch = spawn_agent_shim(AgentType::Kiro, kiro_cmd);
    let (ready, _) = wait_for_ready_and_verify(&mut ch);
    assert!(ready, "Kiro did not become ready within 120s");

    ch.send(&Command::SendMessage {
        from: "test".into(),
        body: "Reply with exactly: BATTY_TEST_OK".into(),
        message_id: Some("live-kiro-1".into()),
    })
    .unwrap();

    let (events, completion) = collect_until_completion(&mut ch, Duration::from_secs(90));

    let saw_working = events.iter().any(|e| {
        matches!(
            e,
            Event::StateChanged {
                to: ShimState::Working,
                ..
            }
        )
    });
    assert!(saw_working, "Should see Working transition for Kiro");
    assert!(completion.is_some(), "Should receive Completion for Kiro");

    shutdown(&mut ch);
}

// =========================================================================
// Test 2: Response extraction — agent output reaches the chat app
// =========================================================================

#[test]
#[cfg_attr(not(feature = "live-agent"), ignore)]
fn live_claude_response_extracted() {
    if !binary_available("claude") {
        eprintln!("SKIP: claude CLI not found on PATH");
        return;
    }

    let mut ch = spawn_agent_shim(AgentType::Claude, "claude --dangerously-skip-permissions");
    let (ready, _) = wait_for_ready_and_verify(&mut ch);
    assert!(ready, "Claude did not become ready");

    ch.send(&Command::SendMessage {
        from: "test".into(),
        body: "What is 2+2? Reply with ONLY the number, nothing else.".into(),
        message_id: Some("live-response-1".into()),
    })
    .unwrap();

    let (_events, completion) = collect_until_completion(&mut ch, Duration::from_secs(90));

    assert!(completion.is_some(), "Should get Completion");

    if let Some(Event::Completion {
        response,
        last_lines,
        message_id,
    }) = completion
    {
        let combined = format!("{response}\n{last_lines}");

        eprintln!(
            "[live-claude] response ({} chars): {response:?}",
            response.len()
        );
        eprintln!(
            "[live-claude] last_lines ({} chars): {last_lines:?}",
            last_lines.len()
        );

        assert!(
            combined.contains('4'),
            "Response should contain '4' (the answer to 2+2).\n\
             response={response:?}\n\
             last_lines={last_lines:?}"
        );

        assert_eq!(
            message_id.as_deref(),
            Some("live-response-1"),
            "message_id should roundtrip"
        );

        if response.is_empty() {
            eprintln!(
                "[live-claude] BUG: response field is empty — extract_response \
                 is failing to diff pre/post screen content for Claude's TUI. \
                 Fell back to last_lines."
            );
        }
    }

    shutdown(&mut ch);
}

#[test]
#[cfg_attr(not(feature = "live-agent"), ignore)]
fn live_codex_response_extracted() {
    if !binary_available("codex") {
        eprintln!("SKIP: codex CLI not found on PATH");
        return;
    }

    let mut ch = spawn_agent_shim(AgentType::Codex, "codex");
    let (ready, _) = wait_for_ready_and_verify(&mut ch);
    assert!(ready, "Codex did not become ready");

    ch.send(&Command::SendMessage {
        from: "test".into(),
        body: "What is 2+2? Reply with ONLY the number, nothing else.".into(),
        message_id: Some("live-codex-resp-1".into()),
    })
    .unwrap();

    let (_events, completion) = collect_until_completion(&mut ch, Duration::from_secs(90));

    assert!(completion.is_some(), "Should get Completion");

    if let Some(Event::Completion {
        response,
        last_lines,
        ..
    }) = completion
    {
        let combined = format!("{response}\n{last_lines}");
        eprintln!("[live-codex] response: {response:?}");
        eprintln!("[live-codex] last_lines: {last_lines:?}");

        assert!(
            combined.contains('4'),
            "Response should contain '4'. response={response:?}, last_lines={last_lines:?}"
        );

        if response.is_empty() {
            eprintln!("[live-codex] BUG: response field is empty");
        }
    }

    shutdown(&mut ch);
}

#[test]
#[cfg_attr(not(feature = "live-agent"), ignore)]
fn live_kiro_response_extracted() {
    let Some(kiro_cmd) = kiro_command() else {
        eprintln!("SKIP: kiro-cli not found on PATH");
        return;
    };

    let mut ch = spawn_agent_shim(AgentType::Kiro, kiro_cmd);
    let (ready, _) = wait_for_ready_and_verify(&mut ch);
    assert!(ready, "Kiro did not become ready");

    ch.send(&Command::SendMessage {
        from: "test".into(),
        body: "What is 2+2? Reply with ONLY the number, nothing else.".into(),
        message_id: Some("live-kiro-resp-1".into()),
    })
    .unwrap();

    let (_events, completion) = collect_until_completion(&mut ch, Duration::from_secs(90));

    assert!(completion.is_some(), "Should get Completion");

    if let Some(Event::Completion {
        response,
        last_lines,
        ..
    }) = completion
    {
        let combined = format!("{response}\n{last_lines}");
        eprintln!("[live-kiro] response: {response:?}");
        eprintln!("[live-kiro] last_lines: {last_lines:?}");

        assert!(
            combined.contains('4'),
            "Response should contain '4'. response={response:?}, last_lines={last_lines:?}"
        );

        if response.is_empty() {
            eprintln!("[live-kiro] BUG: response field is empty");
        }
    }

    shutdown(&mut ch);
}

// =========================================================================
// Test 3: State transitions timed — Working/Idle cycle with wall-clock
// =========================================================================

#[test]
#[cfg_attr(not(feature = "live-agent"), ignore)]
fn live_claude_state_transitions_timed() {
    if !binary_available("claude") {
        eprintln!("SKIP: claude CLI not found on PATH");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("claude-live.pty.log");

    let mut ch = spawn_agent_shim_with_log(
        AgentType::Claude,
        "claude --dangerously-skip-permissions",
        log_path.clone(),
    );
    let (ready, _) = wait_for_ready_and_verify(&mut ch);
    assert!(ready, "Claude did not become ready");

    let t0 = Instant::now();

    ch.send(&Command::SendMessage {
        from: "test".into(),
        body: "Say hello".into(),
        message_id: None,
    })
    .unwrap();

    let (events, _) = collect_until_completion(&mut ch, Duration::from_secs(90));
    let elapsed = t0.elapsed();

    // Collect state transitions with wall-clock timestamps
    let mut transition_log = Vec::new();
    let mut t_offset = Duration::ZERO;
    for evt in &events {
        if let Event::StateChanged {
            from, to, summary, ..
        } = evt
        {
            transition_log.push(format!(
                "  +{:.1}s: {from} → {to} (summary: {})",
                t_offset.as_secs_f64(),
                &summary[..summary.len().min(80)]
            ));
        }
        // We don't have exact per-event timestamps from the protocol,
        // but we know they arrived in order within `elapsed`.
        t_offset = t0.elapsed();
    }

    let state_changes: Vec<&Event> = events
        .iter()
        .filter(|e| matches!(e, Event::StateChanged { .. }))
        .collect();

    eprintln!(
        "[live-claude] Completed in {:.1}s, {} state transitions:\n{}",
        elapsed.as_secs_f64(),
        state_changes.len(),
        transition_log.join("\n")
    );

    assert!(
        state_changes.len() >= 2,
        "Should have >= 2 state transitions (Idle→Working, Working→Idle). \
         Got {}: {state_changes:?}",
        state_changes.len()
    );

    // Verify ordering
    let mut saw_working = false;
    let mut saw_idle_after_working = false;
    for evt in &state_changes {
        if let Event::StateChanged { to, .. } = evt {
            match to {
                ShimState::Working => saw_working = true,
                ShimState::Idle if saw_working => saw_idle_after_working = true,
                _ => {}
            }
        }
    }
    assert!(saw_working, "Must see Working transition");
    assert!(saw_idle_after_working, "Must see Idle after Working");

    // Wall-clock sanity
    assert!(
        elapsed.as_millis() > 100,
        "Processing should take >100ms (took {}ms)",
        elapsed.as_millis()
    );

    // PTY log should have content
    if log_path.exists() {
        let log_size = std::fs::metadata(&log_path).unwrap().len();
        assert!(log_size > 0, "PTY log should have content");
        eprintln!("[live-claude] PTY log: {} bytes", log_size);
    }

    // GetState should report Idle with recent timestamp
    ch.send(&Command::GetState).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if Instant::now() > deadline {
            break;
        }
        match ch.recv::<Event>() {
            Ok(Some(Event::State { state, since_secs })) => {
                assert_eq!(state, ShimState::Idle);
                assert!(since_secs < 60, "since_secs={since_secs} should be <60");
                eprintln!("[live-claude] GetState: {state}, since: {since_secs}s");
                break;
            }
            Ok(Some(_)) => continue,
            Err(e) if is_timeout_error(&e) => continue,
            _ => break,
        }
    }

    shutdown(&mut ch);
}

#[test]
#[cfg_attr(not(feature = "live-agent"), ignore)]
fn live_codex_state_transitions_timed() {
    if !binary_available("codex") {
        eprintln!("SKIP: codex CLI not found on PATH");
        return;
    }

    let mut ch = spawn_agent_shim(AgentType::Codex, "codex");
    let (ready, _) = wait_for_ready_and_verify(&mut ch);
    assert!(ready, "Codex did not become ready");

    let t0 = Instant::now();

    ch.send(&Command::SendMessage {
        from: "test".into(),
        body: "Say hello".into(),
        message_id: None,
    })
    .unwrap();

    let (events, _) = collect_until_completion(&mut ch, Duration::from_secs(90));
    let elapsed = t0.elapsed();

    let state_changes: Vec<&Event> = events
        .iter()
        .filter(|e| matches!(e, Event::StateChanged { .. }))
        .collect();

    assert!(
        state_changes.len() >= 2,
        "Codex should have >= 2 state transitions. Got {}: {state_changes:?}",
        state_changes.len()
    );

    eprintln!(
        "[live-codex] Completed in {:.1}s, {} transitions",
        elapsed.as_secs_f64(),
        state_changes.len()
    );

    shutdown(&mut ch);
}

#[test]
#[cfg_attr(not(feature = "live-agent"), ignore)]
fn live_kiro_state_transitions_timed() {
    let Some(kiro_cmd) = kiro_command() else {
        eprintln!("SKIP: kiro-cli not found on PATH");
        return;
    };

    let mut ch = spawn_agent_shim(AgentType::Kiro, kiro_cmd);
    let (ready, _) = wait_for_ready_and_verify(&mut ch);
    assert!(ready, "Kiro did not become ready");

    let t0 = Instant::now();

    ch.send(&Command::SendMessage {
        from: "test".into(),
        body: "Say hello".into(),
        message_id: None,
    })
    .unwrap();

    let (events, _) = collect_until_completion(&mut ch, Duration::from_secs(90));
    let elapsed = t0.elapsed();

    let state_changes: Vec<&Event> = events
        .iter()
        .filter(|e| matches!(e, Event::StateChanged { .. }))
        .collect();

    assert!(
        state_changes.len() >= 2,
        "Kiro should have >= 2 state transitions. Got {}: {state_changes:?}",
        state_changes.len()
    );

    eprintln!(
        "[live-kiro] Completed in {:.1}s, {} transitions",
        elapsed.as_secs_f64(),
        state_changes.len()
    );

    shutdown(&mut ch);
}

// =========================================================================
// Test 4: Screen capture shows agent content at idle
// =========================================================================

#[test]
#[cfg_attr(not(feature = "live-agent"), ignore)]
fn live_claude_screen_capture_has_content() {
    if !binary_available("claude") {
        eprintln!("SKIP: claude CLI not found on PATH");
        return;
    }

    let mut ch = spawn_agent_shim(AgentType::Claude, "claude --dangerously-skip-permissions");
    let (ready, idle_screen) = wait_for_ready_and_verify(&mut ch);
    assert!(ready, "Claude did not become ready");

    // Idle screen should have content
    let screen = idle_screen.expect("Should be able to capture screen");
    assert!(
        !screen.trim().is_empty(),
        "Screen should not be empty at idle"
    );

    // Send a message, wait for completion, capture post-response screen
    ch.send(&Command::SendMessage {
        from: "test".into(),
        body: "What is 3+3? Reply with ONLY the number.".into(),
        message_id: None,
    })
    .unwrap();

    let (_, _) = collect_until_completion(&mut ch, Duration::from_secs(90));

    if let Some(post_screen) = capture_screen(&mut ch) {
        eprintln!(
            "[live-claude] Post-response screen ({} chars):\n{}",
            post_screen.len(),
            &post_screen[..post_screen.len().min(600)]
        );
        // After responding, screen should have changed from the idle screen
        assert!(
            post_screen != screen,
            "Screen should change after agent responds"
        );
    }

    shutdown(&mut ch);
}

// =========================================================================
// Test 5: Multiple sequential messages to same agent session
// =========================================================================

#[test]
#[cfg_attr(not(feature = "live-agent"), ignore)]
fn live_claude_sequential_messages() {
    if !binary_available("claude") {
        eprintln!("SKIP: claude CLI not found on PATH");
        return;
    }

    let mut ch = spawn_agent_shim(AgentType::Claude, "claude --dangerously-skip-permissions");
    let (ready, _) = wait_for_ready_and_verify(&mut ch);
    assert!(ready, "Claude did not become ready");

    let questions = [
        ("What is 1+1? Reply ONLY the number.", "2"),
        ("What is 5+5? Reply ONLY the number.", "10"),
    ];

    for (i, (question, expected)) in questions.iter().enumerate() {
        eprintln!("[live-claude] Sending message {}: {question}", i + 1);

        ch.send(&Command::SendMessage {
            from: "test".into(),
            body: question.to_string(),
            message_id: Some(format!("seq-{i}")),
        })
        .unwrap();

        let (_events, completion) = collect_until_completion(&mut ch, Duration::from_secs(90));

        assert!(
            completion.is_some(),
            "Message {} should produce Completion",
            i + 1
        );

        if let Some(Event::Completion {
            response,
            last_lines,
            message_id,
        }) = completion
        {
            let combined = format!("{response}\n{last_lines}");
            eprintln!("[live-claude] Message {} response: {response:?}", i + 1);

            assert!(
                combined.contains(expected),
                "Message {} should contain '{expected}'. \
                 response={response:?}, last_lines={last_lines:?}",
                i + 1
            );
            assert_eq!(
                message_id.as_deref(),
                Some(format!("seq-{i}").as_str()),
                "message_id should roundtrip for message {}",
                i + 1
            );
        }
    }

    shutdown(&mut ch);
}
