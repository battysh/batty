//! Integration tests for the shim runtime.
//!
//! Gated behind `shim-integration` feature flag.
//! Run with: cargo test --features shim-integration
//!
//! These tests spawn a real bash process via the shim runtime and
//! exercise the protocol end-to-end.

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use crate::shim::protocol::{self, Channel, Command, Event, ShimState};
    use crate::shim::runtime::ShimArgs;

    /// Helper: spawn a shim with bash in a background thread, return the parent channel.
    fn spawn_bash_shim() -> Channel {
        let (parent_sock, child_sock) = protocol::socketpair().unwrap();
        let channel = Channel::new(child_sock);

        let args = ShimArgs {
            id: "test-agent".into(),
            agent_type: crate::shim::classifier::AgentType::Generic,
            cmd: "bash".into(),
            cwd: PathBuf::from("/tmp"),
            rows: 24,
            cols: 80,
        };

        std::thread::spawn(move || {
            crate::shim::runtime::run(args, channel).ok();
        });

        Channel::new(parent_sock)
    }

    /// Wait for a Ready event (with timeout).
    fn wait_for_ready(ch: &mut Channel) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            if std::time::Instant::now() > deadline {
                return false;
            }
            // Set a read timeout on the underlying stream
            match ch.recv::<Event>() {
                Ok(Some(Event::Ready)) => return true,
                Ok(Some(Event::StateChanged { .. })) => continue,
                Ok(Some(_)) => continue,
                Ok(None) => return false,
                Err(_) => return false,
            }
        }
    }

    /// Drain events until we get a Completion or timeout.
    fn wait_for_completion(ch: &mut Channel, timeout: Duration) -> Option<Event> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if std::time::Instant::now() > deadline {
                return None;
            }
            match ch.recv::<Event>() {
                Ok(Some(evt @ Event::Completion { .. })) => return Some(evt),
                Ok(Some(Event::Died { .. })) => return None,
                Ok(Some(_)) => continue, // StateChanged, etc.
                Ok(None) => return None,
                Err(_) => return None,
            }
        }
    }

    /// Drain events until we get a specific event type or timeout.
    fn wait_for_event<F>(ch: &mut Channel, timeout: Duration, matcher: F) -> Option<Event>
    where
        F: Fn(&Event) -> bool,
    {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if std::time::Instant::now() > deadline {
                return None;
            }
            match ch.recv::<Event>() {
                Ok(Some(ref evt)) if matcher(evt) => return Some(evt.clone()),
                Ok(Some(_)) => continue,
                Ok(None) => return None,
                Err(_) => return None,
            }
        }
    }

    // -----------------------------------------------------------------------
    // Tests — all gated behind shim-integration
    // -----------------------------------------------------------------------

    #[test]
    #[cfg_attr(not(feature = "shim-integration"), ignore)]
    fn shim_echo_hello() {
        let mut ch = spawn_bash_shim();
        assert!(wait_for_ready(&mut ch), "shim did not become ready");

        ch.send(&Command::SendMessage {
            from: "user".into(),
            body: "echo hello".into(),
            message_id: None,
        })
        .unwrap();

        let evt = wait_for_completion(&mut ch, Duration::from_secs(10));
        assert!(evt.is_some(), "did not receive Completion event");
        if let Some(Event::Completion { response, .. }) = evt {
            assert!(
                response.contains("hello"),
                "response should contain 'hello', got: {response}"
            );
        }

        ch.send(&Command::Shutdown { timeout_secs: 2 }).unwrap();
    }

    #[test]
    #[cfg_attr(not(feature = "shim-integration"), ignore)]
    fn shim_exit_produces_died() {
        let mut ch = spawn_bash_shim();
        assert!(wait_for_ready(&mut ch), "shim did not become ready");

        ch.send(&Command::SendMessage {
            from: "user".into(),
            body: "exit".into(),
            message_id: None,
        })
        .unwrap();

        let evt = wait_for_event(&mut ch, Duration::from_secs(10), |e| {
            matches!(e, Event::Died { .. })
        });
        assert!(evt.is_some(), "did not receive Died event");
        if let Some(Event::Died { exit_code, .. }) = evt {
            assert_eq!(exit_code, Some(0).or(None)); // bash exit 0 or None
        }
    }

    #[test]
    #[cfg_attr(not(feature = "shim-integration"), ignore)]
    fn shim_capture_screen() {
        let mut ch = spawn_bash_shim();
        assert!(wait_for_ready(&mut ch), "shim did not become ready");

        ch.send(&Command::CaptureScreen {
            last_n_lines: Some(10),
        })
        .unwrap();

        let evt = wait_for_event(&mut ch, Duration::from_secs(5), |e| {
            matches!(e, Event::ScreenCapture { .. })
        });
        assert!(evt.is_some(), "did not receive ScreenCapture event");
        if let Some(Event::ScreenCapture { content, .. }) = evt {
            assert!(!content.is_empty(), "screen capture should not be empty");
        }

        ch.send(&Command::Shutdown { timeout_secs: 2 }).unwrap();
    }

    #[test]
    #[cfg_attr(not(feature = "shim-integration"), ignore)]
    fn shim_resize_no_error() {
        let mut ch = spawn_bash_shim();
        assert!(wait_for_ready(&mut ch), "shim did not become ready");

        // Resize should not produce an error event
        ch.send(&Command::Resize {
            rows: 40,
            cols: 120,
        })
        .unwrap();

        // Give it a moment, then verify with a Ping that the shim is still alive
        ch.send(&Command::Ping).unwrap();
        let evt = wait_for_event(&mut ch, Duration::from_secs(5), |e| {
            matches!(e, Event::Pong)
        });
        assert!(
            evt.is_some(),
            "shim should still be responsive after resize"
        );

        ch.send(&Command::Shutdown { timeout_secs: 2 }).unwrap();
    }

    #[test]
    #[cfg_attr(not(feature = "shim-integration"), ignore)]
    fn shim_ping_pong() {
        let mut ch = spawn_bash_shim();
        assert!(wait_for_ready(&mut ch), "shim did not become ready");

        ch.send(&Command::Ping).unwrap();
        let evt = wait_for_event(&mut ch, Duration::from_secs(5), |e| {
            matches!(e, Event::Pong)
        });
        assert!(evt.is_some(), "did not receive Pong event");

        ch.send(&Command::Shutdown { timeout_secs: 2 }).unwrap();
    }

    #[test]
    #[cfg_attr(not(feature = "shim-integration"), ignore)]
    fn shim_get_state() {
        let mut ch = spawn_bash_shim();
        assert!(wait_for_ready(&mut ch), "shim did not become ready");

        ch.send(&Command::GetState).unwrap();
        let evt = wait_for_event(&mut ch, Duration::from_secs(5), |e| {
            matches!(e, Event::State { .. })
        });
        assert!(evt.is_some(), "did not receive State event");
        if let Some(Event::State { state, .. }) = evt {
            assert_eq!(state, ShimState::Idle, "state should be Idle after ready");
        }

        ch.send(&Command::Shutdown { timeout_secs: 2 }).unwrap();
    }

    #[test]
    #[cfg_attr(not(feature = "shim-integration"), ignore)]
    fn shim_send_while_working_returns_error() {
        let mut ch = spawn_bash_shim();
        assert!(wait_for_ready(&mut ch), "shim did not become ready");

        // Send a command that takes a moment
        ch.send(&Command::SendMessage {
            from: "user".into(),
            body: "sleep 2".into(),
            message_id: None,
        })
        .unwrap();

        // Wait for the Working state transition
        let working = wait_for_event(&mut ch, Duration::from_secs(5), |e| {
            matches!(
                e,
                Event::StateChanged {
                    to: ShimState::Working,
                    ..
                }
            )
        });
        assert!(working.is_some(), "should transition to Working");

        // Now try to send another message — should get Error
        ch.send(&Command::SendMessage {
            from: "user".into(),
            body: "echo should fail".into(),
            message_id: None,
        })
        .unwrap();

        let err = wait_for_event(&mut ch, Duration::from_secs(3), |e| {
            matches!(e, Event::Error { .. })
        });
        assert!(
            err.is_some(),
            "should receive Error when sending while working"
        );

        // Wait for the original command to complete
        let _ = wait_for_completion(&mut ch, Duration::from_secs(10));
        ch.send(&Command::Shutdown { timeout_secs: 2 }).unwrap();
    }

    #[test]
    #[cfg_attr(not(feature = "shim-integration"), ignore)]
    fn shim_sleep_shows_working_then_idle() {
        let mut ch = spawn_bash_shim();
        assert!(wait_for_ready(&mut ch), "shim did not become ready");

        ch.send(&Command::SendMessage {
            from: "user".into(),
            body: "sleep 1 && echo done".into(),
            message_id: None,
        })
        .unwrap();

        // Should see Working state
        let working = wait_for_event(&mut ch, Duration::from_secs(5), |e| {
            matches!(
                e,
                Event::StateChanged {
                    to: ShimState::Working,
                    ..
                }
            )
        });
        assert!(working.is_some(), "should transition to Working");

        // Then completion (which implies transition back to Idle)
        let completion = wait_for_completion(&mut ch, Duration::from_secs(15));
        assert!(completion.is_some(), "should get completion after sleep");

        ch.send(&Command::Shutdown { timeout_secs: 2 }).unwrap();
    }
}
