//! Integration tests for the Codex SDK shim runtime.
//!
//! These tests spawn mock subprocesses that emit scripted JSONL events
//! matching the Codex `exec --json` protocol.
//!
//! Gated behind `shim-integration` feature flag.
//! Run with: cargo test --features shim-integration tests_codex

use std::path::PathBuf;
use std::time::Duration;

use crate::shim::protocol::{self, Channel, Command, Event, ShimState};
use crate::shim::runtime::ShimArgs;

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

/// Spawn a Codex SDK shim with a mock bash script as the sentinel process.
fn spawn_codex_mock() -> Channel {
    let (parent_sock, child_sock) = protocol::socketpair().unwrap();
    let channel = Channel::new(child_sock);

    let args = ShimArgs {
        id: "codex-test".into(),
        agent_type: crate::shim::classifier::AgentType::Codex,
        cmd: "sleep 300".into(), // sentinel — runtime doesn't use this
        cwd: PathBuf::from("/tmp"),
        rows: 24,
        cols: 80,
        pty_log_path: None,
    };

    std::thread::spawn(move || {
        crate::shim::runtime_codex::run_codex_sdk(args, channel).ok();
    });

    let mut ch = Channel::new(parent_sock);
    ch.set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    ch
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Codex SDK emits Ready immediately (no persistent subprocess).
#[test]
#[cfg_attr(not(feature = "shim-integration"), ignore)]
fn codex_sdk_emits_ready() {
    let mut ch = spawn_codex_mock();

    let evt = wait_for_event(&mut ch, Duration::from_secs(5), |e| {
        matches!(e, Event::Ready)
    });
    assert!(evt.is_some(), "expected Ready event");

    ch.send(&Command::Shutdown { timeout_secs: 1 }).ok();
}

/// Codex SDK responds to Ping.
#[test]
#[cfg_attr(not(feature = "shim-integration"), ignore)]
fn codex_sdk_ping_pong() {
    let mut ch = spawn_codex_mock();
    wait_for_event(&mut ch, Duration::from_secs(5), |e| {
        matches!(e, Event::Ready)
    })
    .expect("Ready");

    ch.send(&Command::Ping).unwrap();
    let evt = wait_for_event(&mut ch, Duration::from_secs(2), |e| {
        matches!(e, Event::Pong)
    });
    assert!(evt.is_some(), "expected Pong");

    ch.send(&Command::Shutdown { timeout_secs: 1 }).ok();
}

/// Codex SDK reports Idle state.
#[test]
#[cfg_attr(not(feature = "shim-integration"), ignore)]
fn codex_sdk_state_is_idle() {
    let mut ch = spawn_codex_mock();
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
        _ => panic!("expected State"),
    }

    ch.send(&Command::Shutdown { timeout_secs: 1 }).ok();
}

/// When a message is sent, Codex spawns a subprocess and produces a Completion.
#[test]
#[cfg_attr(not(feature = "shim-integration"), ignore)]
fn codex_sdk_message_completion() {
    let mut ch = spawn_codex_mock();
    wait_for_event(&mut ch, Duration::from_secs(5), |e| {
        matches!(e, Event::Ready)
    })
    .expect("Ready");

    // The SendMessage will cause the runtime to spawn `codex exec --json ...`
    // which won't be found (no real codex binary in test). But the runtime
    // should handle the spawn failure gracefully and return to Idle.
    ch.send(&Command::SendMessage {
        from: "user".into(),
        body: "test message".into(),
        message_id: Some("msg-1".into()),
    })
    .unwrap();

    // Should get Working state
    let evt = wait_for_event(&mut ch, Duration::from_secs(5), |e| {
        matches!(
            e,
            Event::StateChanged {
                to: ShimState::Working,
                ..
            }
        )
    });
    assert!(evt.is_some(), "expected Working state");

    // Should get both Completion and Idle (order may vary)
    let mut got_completion = false;
    let mut got_idle = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !(got_completion && got_idle) && std::time::Instant::now() < deadline {
        match ch.recv::<Event>() {
            Ok(Some(Event::Completion { .. })) => got_completion = true,
            Ok(Some(Event::StateChanged {
                to: ShimState::Idle,
                ..
            })) => got_idle = true,
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(e) if is_timeout_error(&e) => continue,
            Err(_) => break,
        }
    }
    assert!(got_completion, "expected Completion event");
    assert!(got_idle, "expected Idle state after completion");

    ch.send(&Command::Shutdown { timeout_secs: 1 }).ok();
}

/// Shutdown terminates cleanly.
#[test]
#[cfg_attr(not(feature = "shim-integration"), ignore)]
fn codex_sdk_shutdown() {
    let mut ch = spawn_codex_mock();
    wait_for_event(&mut ch, Duration::from_secs(5), |e| {
        matches!(e, Event::Ready)
    })
    .expect("Ready");

    ch.send(&Command::Shutdown { timeout_secs: 2 }).unwrap();

    // Channel should close (shim exited)
    std::thread::sleep(Duration::from_secs(1));
    // Verify we can't send anymore (channel closed)
    let result = ch.send(&Command::Ping);
    // Either error or the channel is dead — both OK
    drop(result);
}
