//! Integration tests for the SDK shim runtime.
//!
//! These tests spawn mock subprocesses that emit scripted NDJSON responses,
//! exercising the SDK runtime's message parsing, state machine, and protocol
//! emission end-to-end.
//!
//! Gated behind `shim-integration` feature flag.
//! Run with: cargo test --features shim-integration tests_sdk

use std::path::PathBuf;
use std::time::Duration;

use crate::shim::protocol::{self, Channel, Command, Event, ShimState};
use crate::shim::runtime::ShimArgs;

/// Check if an error is a read timeout (WouldBlock/TimedOut).
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

/// Wait for a specific event type, skipping others, with a timeout.
fn wait_for_event<F>(ch: &mut Channel, timeout: Duration, predicate: F) -> Option<Event>
where
    F: Fn(&Event) -> bool,
{
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if std::time::Instant::now() > deadline {
            return None;
        }
        match ch.recv::<Event>() {
            Ok(Some(evt)) if predicate(&evt) => return Some(evt),
            Ok(Some(_)) => continue,
            Ok(None) => return None,
            Err(e) if is_timeout_error(&e) => continue,
            Err(_) => return None,
        }
    }
}

/// Spawn an SDK shim with a bash command that acts as a mock Claude Code subprocess.
/// The mock command should read from stdin and write NDJSON to stdout.
///
/// Returns the parent Channel for sending Commands and receiving Events.
fn spawn_sdk_mock(mock_cmd: &str) -> Channel {
    let (parent_sock, child_sock) = protocol::socketpair().unwrap();
    let channel = Channel::new(child_sock);

    let args = ShimArgs {
        id: "sdk-test".into(),
        agent_type: crate::shim::classifier::AgentType::Claude,
        cmd: mock_cmd.to_string(),
        cwd: PathBuf::from("/tmp"),
        rows: 24,
        cols: 80,
        pty_log_path: None,
        graceful_shutdown_timeout_secs: 5,
        auto_commit_on_restart: true,
    };

    std::thread::spawn(move || {
        crate::shim::runtime_sdk::run_sdk(args, channel).ok();
    });

    let mut ch = Channel::new(parent_sock);
    ch.set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    ch
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// SDK mode emits Ready immediately (no startup prompt to wait for).
#[test]
#[cfg_attr(not(feature = "shim-integration"), ignore)]
fn sdk_emits_ready_immediately() {
    // Mock: just sleep forever doing nothing, so we can test Ready arrives
    let mut ch = spawn_sdk_mock("sleep 30");

    let evt = wait_for_event(&mut ch, Duration::from_secs(5), |e| {
        matches!(e, Event::Ready)
    });
    assert!(evt.is_some(), "expected Ready event");

    ch.send(&Command::Shutdown { timeout_secs: 2 }).ok();
}

/// SDK mode responds to Ping with Pong.
#[test]
#[cfg_attr(not(feature = "shim-integration"), ignore)]
fn sdk_ping_pong() {
    let mut ch = spawn_sdk_mock("sleep 30");

    // Wait for Ready
    wait_for_event(&mut ch, Duration::from_secs(5), |e| {
        matches!(e, Event::Ready)
    })
    .expect("Ready");

    ch.send(&Command::Ping).unwrap();
    let evt = wait_for_event(&mut ch, Duration::from_secs(2), |e| {
        matches!(e, Event::Pong)
    });
    assert!(evt.is_some(), "expected Pong");

    ch.send(&Command::Shutdown { timeout_secs: 2 }).ok();
}

/// SDK mode reports state as Idle after Ready.
#[test]
#[cfg_attr(not(feature = "shim-integration"), ignore)]
fn sdk_get_state_idle() {
    let mut ch = spawn_sdk_mock("sleep 30");

    wait_for_event(&mut ch, Duration::from_secs(5), |e| {
        matches!(e, Event::Ready)
    })
    .expect("Ready");

    ch.send(&Command::GetState).unwrap();
    let evt = wait_for_event(&mut ch, Duration::from_secs(2), |e| {
        matches!(e, Event::State { .. })
    });
    match evt {
        Some(Event::State { state, .. }) => assert_eq!(state, ShimState::Idle),
        _ => panic!("expected State event"),
    }

    ch.send(&Command::Shutdown { timeout_secs: 2 }).ok();
}

/// When a message is sent, the SDK runtime transitions Idle→Working.
/// When the mock writes a result, it transitions Working→Idle and emits Completion.
#[test]
#[cfg_attr(not(feature = "shim-integration"), ignore)]
fn sdk_message_produces_completion() {
    // Mock: reads one line from stdin (the user message), then writes an assistant
    // message followed by a result message.
    let mock_script = r#"bash -c '
read line
echo "{\"type\":\"assistant\",\"session_id\":\"sess-1\",\"uuid\":\"u1\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"Hello from mock\"}]}}"
sleep 0.1
echo "{\"type\":\"result\",\"subtype\":\"success\",\"session_id\":\"sess-1\",\"uuid\":\"u2\",\"result\":\"done\",\"num_turns\":1,\"is_error\":false,\"duration_ms\":100,\"duration_api_ms\":50,\"total_cost_usd\":0.01,\"stop_reason\":\"end_turn\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5},\"modelUsage\":{},\"permission_denials\":[]}"
sleep 30
'"#;

    let mut ch = spawn_sdk_mock(mock_script);

    wait_for_event(&mut ch, Duration::from_secs(5), |e| {
        matches!(e, Event::Ready)
    })
    .expect("Ready");

    // Send a message
    ch.send(&Command::SendMessage {
        from: "user".into(),
        body: "Hello".into(),
        message_id: Some("msg-1".into()),
    })
    .unwrap();

    // Should get Idle→Working
    let evt = wait_for_event(&mut ch, Duration::from_secs(5), |e| {
        matches!(
            e,
            Event::StateChanged {
                to: ShimState::Working,
                ..
            }
        )
    });
    assert!(evt.is_some(), "expected Working state change");

    // Should get StateChanged(Working→Idle) and Completion (order may vary).
    // Collect both within the timeout.
    let mut got_completion = false;
    let mut got_idle = false;
    let mut response_text = String::new();
    let mut completion_msg_id = None;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !(got_completion && got_idle) && std::time::Instant::now() < deadline {
        match ch.recv::<Event>() {
            Ok(Some(Event::Completion {
                response,
                message_id,
                ..
            })) => {
                response_text = response;
                completion_msg_id = message_id;
                got_completion = true;
            }
            Ok(Some(Event::StateChanged {
                to: ShimState::Idle,
                ..
            })) => {
                got_idle = true;
            }
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(e) if is_timeout_error(&e) => continue,
            Err(_) => break,
        }
    }
    assert!(got_completion, "expected Completion event");
    assert!(got_idle, "expected Idle state after completion");
    assert!(
        response_text.contains("Hello from mock"),
        "response should contain mock text, got: {response_text}"
    );
    assert_eq!(completion_msg_id.as_deref(), Some("msg-1"));

    ch.send(&Command::Shutdown { timeout_secs: 2 }).ok();
}

/// When the mock subprocess exits, the SDK runtime emits Died.
#[test]
#[cfg_attr(not(feature = "shim-integration"), ignore)]
fn sdk_subprocess_exit_emits_died() {
    // Mock: exits immediately after a short delay
    let mut ch = spawn_sdk_mock("bash -c 'sleep 0.5; exit 0'");

    wait_for_event(&mut ch, Duration::from_secs(5), |e| {
        matches!(e, Event::Ready)
    })
    .expect("Ready");

    // Wait for Died event
    let evt = wait_for_event(&mut ch, Duration::from_secs(5), |e| {
        matches!(e, Event::Died { .. })
    });
    assert!(evt.is_some(), "expected Died event after subprocess exit");
}

/// Sending a message while the agent is already Working queues it.
#[test]
#[cfg_attr(not(feature = "shim-integration"), ignore)]
fn sdk_message_queued_while_working() {
    // Mock: reads stdin line by line, responds to each with assistant+result
    let mock_script = r#"bash -c '
while read line; do
  echo "{\"type\":\"assistant\",\"session_id\":\"s\",\"uuid\":\"u\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"response\"}]}}"
  sleep 0.1
  echo "{\"type\":\"result\",\"subtype\":\"success\",\"session_id\":\"s\",\"uuid\":\"u2\",\"result\":\"ok\",\"num_turns\":1,\"is_error\":false,\"duration_ms\":1,\"duration_api_ms\":1,\"total_cost_usd\":0,\"stop_reason\":\"end_turn\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"modelUsage\":{},\"permission_denials\":[]}"
done
'"#;

    let mut ch = spawn_sdk_mock(mock_script);

    wait_for_event(&mut ch, Duration::from_secs(5), |e| {
        matches!(e, Event::Ready)
    })
    .expect("Ready");

    // Send first message
    ch.send(&Command::SendMessage {
        from: "user".into(),
        body: "first".into(),
        message_id: Some("m1".into()),
    })
    .unwrap();

    // Wait for Working state
    wait_for_event(&mut ch, Duration::from_secs(5), |e| {
        matches!(
            e,
            Event::StateChanged {
                to: ShimState::Working,
                ..
            }
        )
    })
    .expect("Working after first message");

    // Send second message while still working — should queue
    ch.send(&Command::SendMessage {
        from: "user".into(),
        body: "second".into(),
        message_id: Some("m2".into()),
    })
    .unwrap();

    // Should get a Warning about queued message
    let evt = wait_for_event(&mut ch, Duration::from_secs(3), |e| {
        matches!(e, Event::Warning { .. })
    });
    assert!(evt.is_some(), "expected Warning about queued message");

    // Wait for first completion
    wait_for_event(&mut ch, Duration::from_secs(10), |e| {
        matches!(e, Event::Completion { .. })
    })
    .expect("first Completion");

    // Queued message should auto-deliver and produce a second Completion
    let evt = wait_for_event(&mut ch, Duration::from_secs(10), |e| {
        matches!(e, Event::Completion { .. })
    });
    assert!(
        evt.is_some(),
        "expected second Completion from queued message"
    );

    ch.send(&Command::Shutdown { timeout_secs: 2 }).ok();
}

/// Context exhaustion is detected from result error messages.
#[test]
#[cfg_attr(not(feature = "shim-integration"), ignore)]
fn sdk_context_exhaustion_detected() {
    // Mock: responds with an error result containing context exhaustion text
    let mock_script = r#"bash -c '
read line
echo "{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"session_id\":\"s\",\"uuid\":\"u\",\"is_error\":true,\"errors\":[\"context window exceeded\"],\"num_turns\":1,\"duration_ms\":1,\"duration_api_ms\":1,\"total_cost_usd\":0,\"stop_reason\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"modelUsage\":{},\"permission_denials\":[]}"
sleep 30
'"#;

    let mut ch = spawn_sdk_mock(mock_script);

    wait_for_event(&mut ch, Duration::from_secs(5), |e| {
        matches!(e, Event::Ready)
    })
    .expect("Ready");

    ch.send(&Command::SendMessage {
        from: "user".into(),
        body: "test".into(),
        message_id: None,
    })
    .unwrap();

    // Should detect context exhaustion
    let evt = wait_for_event(&mut ch, Duration::from_secs(10), |e| {
        matches!(e, Event::ContextExhausted { .. })
    });
    assert!(evt.is_some(), "expected ContextExhausted event");

    ch.send(&Command::Kill).ok();
}

/// Auto-approval of tool use control requests.
#[test]
#[cfg_attr(not(feature = "shim-integration"), ignore)]
fn sdk_auto_approves_tool_use() {
    // Mock: emits a control_request, then reads the response from stdin,
    // then emits a result. If it reads a control_response with approved=true,
    // the test passes.
    let mock_script = r#"bash -c '
read user_msg
echo "{\"type\":\"control_request\",\"request_id\":\"req-1\",\"request\":{\"subtype\":\"can_use_tool\",\"tool_name\":\"Bash\",\"tool_use_id\":\"tu-1\",\"input\":{\"command\":\"ls\"}}}"
read approval
if echo "$approval" | grep -q "approved.*true"; then
  echo "{\"type\":\"assistant\",\"session_id\":\"s\",\"uuid\":\"u\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"tool approved\"}]}}"
  sleep 0.1
  echo "{\"type\":\"result\",\"subtype\":\"success\",\"session_id\":\"s\",\"uuid\":\"u2\",\"result\":\"done\",\"num_turns\":1,\"is_error\":false,\"duration_ms\":1,\"duration_api_ms\":1,\"total_cost_usd\":0,\"stop_reason\":\"end_turn\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"modelUsage\":{},\"permission_denials\":[]}"
else
  echo "{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"session_id\":\"s\",\"uuid\":\"u2\",\"is_error\":true,\"errors\":[\"approval denied\"],\"num_turns\":1,\"duration_ms\":1,\"duration_api_ms\":1,\"total_cost_usd\":0,\"stop_reason\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":1},\"modelUsage\":{},\"permission_denials\":[]}"
fi
sleep 30
'"#;

    let mut ch = spawn_sdk_mock(mock_script);

    wait_for_event(&mut ch, Duration::from_secs(5), |e| {
        matches!(e, Event::Ready)
    })
    .expect("Ready");

    ch.send(&Command::SendMessage {
        from: "user".into(),
        body: "run a command".into(),
        message_id: None,
    })
    .unwrap();

    // Should get Completion with "tool approved" (not "approval denied")
    let evt = wait_for_event(&mut ch, Duration::from_secs(10), |e| {
        matches!(e, Event::Completion { .. })
    });
    match evt {
        Some(Event::Completion { response, .. }) => {
            assert!(
                response.contains("tool approved"),
                "expected auto-approval, got: {response}"
            );
        }
        _ => panic!("expected Completion"),
    }

    ch.send(&Command::Shutdown { timeout_secs: 2 }).ok();
}

/// CaptureScreen returns accumulated response text in SDK mode.
#[test]
#[cfg_attr(not(feature = "shim-integration"), ignore)]
fn sdk_capture_screen_returns_accumulated_text() {
    let mut ch = spawn_sdk_mock("sleep 30");

    wait_for_event(&mut ch, Duration::from_secs(5), |e| {
        matches!(e, Event::Ready)
    })
    .expect("Ready");

    // In idle state with no accumulated response, screen should be empty
    ch.send(&Command::CaptureScreen {
        last_n_lines: Some(10),
    })
    .unwrap();

    let evt = wait_for_event(&mut ch, Duration::from_secs(2), |e| {
        matches!(e, Event::ScreenCapture { .. })
    });
    match evt {
        Some(Event::ScreenCapture { content, .. }) => {
            assert!(
                content.is_empty(),
                "expected empty screen before any messages"
            );
        }
        _ => panic!("expected ScreenCapture"),
    }

    ch.send(&Command::Shutdown { timeout_secs: 2 }).ok();
}

/// Shutdown gracefully closes the subprocess.
#[test]
#[cfg_attr(not(feature = "shim-integration"), ignore)]
fn sdk_shutdown_terminates_cleanly() {
    let mut ch = spawn_sdk_mock("sleep 30");

    wait_for_event(&mut ch, Duration::from_secs(5), |e| {
        matches!(e, Event::Ready)
    })
    .expect("Ready");

    ch.send(&Command::Shutdown { timeout_secs: 3 }).unwrap();

    // Should eventually get Died or the channel should close
    let evt = wait_for_event(&mut ch, Duration::from_secs(10), |e| {
        matches!(
            e,
            Event::Died { .. }
                | Event::StateChanged {
                    to: ShimState::Dead,
                    ..
                }
        )
    });
    assert!(evt.is_some(), "expected Died or Dead state after shutdown");
}
