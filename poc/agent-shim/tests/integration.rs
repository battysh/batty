//! Integration tests: spawn a real shim subprocess with a bash agent,
//! exchange messages over the socketpair protocol, verify behavior.

use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};

// Re-implement the minimal protocol types here to avoid depending on the
// library (integration tests are separate compilation units).

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd")]
enum Cmd {
    SendMessage {
        from: String,
        body: String,
        message_id: Option<String>,
    },
    CaptureScreen {
        last_n_lines: Option<usize>,
    },
    GetState,
    Ping,
    Shutdown {
        timeout_secs: u32,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "event")]
enum Evt {
    Ready,
    StateChanged {
        from: String,
        to: String,
        summary: String,
    },
    Completion {
        message_id: Option<String>,
        response: String,
        last_lines: String,
    },
    Died {
        exit_code: Option<i32>,
        last_lines: String,
    },
    ScreenCapture {
        content: String,
        cursor_row: u16,
        cursor_col: u16,
    },
    State {
        state: String,
        since_secs: u64,
    },
    Pong,
    Error {
        command: String,
        reason: String,
    },
    #[serde(other)]
    Unknown,
}

// -- Minimal framed channel (matches protocol.rs) --

fn send_msg(stream: &mut UnixStream, msg: &impl Serialize) {
    let json = serde_json::to_vec(msg).unwrap();
    let len = (json.len() as u32).to_be_bytes();
    stream.write_all(&len).unwrap();
    stream.write_all(&json).unwrap();
    stream.flush().unwrap();
}

fn recv_msg(stream: &mut UnixStream) -> Option<Evt> {
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(_) => return None,
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

/// Receive events until we get one matching the predicate, with timeout.
fn recv_until(stream: &mut UnixStream, timeout: Duration, mut pred: impl FnMut(&Evt) -> bool) -> Option<Evt> {
    stream.set_read_timeout(Some(timeout)).unwrap();
    loop {
        match recv_msg(stream) {
            Some(evt) => {
                if pred(&evt) {
                    stream.set_read_timeout(None).unwrap();
                    return Some(evt);
                }
                // else keep reading
            }
            None => return None,
        }
    }
}

fn spawn_shim(cmd: &str) -> (std::process::Child, UnixStream) {
    let (parent, child_sock) = UnixStream::pair().unwrap();
    let child_fd = child_sock.as_raw_fd();

    let exe = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("agent-shim");

    // Fall back to debug build path
    let exe = if exe.exists() {
        exe
    } else {
        // Try target/debug
        std::path::PathBuf::from(env!("CARGO_BIN_EXE_agent-shim"))
    };

    let child = unsafe {
        Command::new(&exe)
            .args([
                "shim",
                "--id", "test",
                "--agent-type", "generic",
                "--cmd", cmd,
                "--cwd", "/tmp",
                "--rows", "24",
                "--cols", "80",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .pre_exec(move || {
                if child_fd != 3 {
                    let ret = libc::dup2(child_fd, 3);
                    if ret < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            })
            .spawn()
            .expect("failed to spawn shim")
    };

    drop(child_sock);
    (child, parent)
}

#[test]
fn shim_ready_and_ping() {
    let (mut child, mut sock) = spawn_shim("bash --norc --noprofile");

    // Wait for Ready
    let evt = recv_until(&mut sock, Duration::from_secs(10), |e| matches!(e, Evt::Ready));
    assert!(evt.is_some(), "expected Ready event");

    // Ping
    send_msg(&mut sock, &Cmd::Ping);
    let evt = recv_until(&mut sock, Duration::from_secs(2), |e| matches!(e, Evt::Pong));
    assert!(evt.is_some(), "expected Pong");

    // Shutdown
    send_msg(&mut sock, &Cmd::Shutdown { timeout_secs: 2 });
    child.wait().ok();
}

#[test]
fn shim_echo_command() {
    let (mut child, mut sock) = spawn_shim("bash --norc --noprofile");

    // Wait for Ready
    recv_until(&mut sock, Duration::from_secs(10), |e| matches!(e, Evt::Ready))
        .expect("Ready timeout");

    // Send echo command
    send_msg(&mut sock, &Cmd::SendMessage {
        from: "test".into(),
        body: "echo hello_shim_test".into(),
        message_id: Some("msg-1".into()),
    });

    // Wait for Completion
    let evt = recv_until(&mut sock, Duration::from_secs(10), |e| matches!(e, Evt::Completion { .. }));
    match evt {
        Some(Evt::Completion { response, .. }) => {
            assert!(
                response.contains("hello_shim_test"),
                "response should contain our echo output, got: {response}"
            );
        }
        other => panic!("expected Completion, got: {other:?}"),
    }

    send_msg(&mut sock, &Cmd::Shutdown { timeout_secs: 2 });
    child.wait().ok();
}

#[test]
fn shim_capture_screen() {
    let (mut child, mut sock) = spawn_shim("bash --norc --noprofile");

    recv_until(&mut sock, Duration::from_secs(10), |e| matches!(e, Evt::Ready))
        .expect("Ready timeout");

    // Capture screen
    send_msg(&mut sock, &Cmd::CaptureScreen { last_n_lines: Some(5) });
    let evt = recv_until(&mut sock, Duration::from_secs(2), |e| matches!(e, Evt::ScreenCapture { .. }));
    match evt {
        Some(Evt::ScreenCapture { content, .. }) => {
            assert!(!content.is_empty(), "screen capture should not be empty");
        }
        other => panic!("expected ScreenCapture, got: {other:?}"),
    }

    send_msg(&mut sock, &Cmd::Shutdown { timeout_secs: 2 });
    child.wait().ok();
}

#[test]
fn shim_get_state() {
    let (mut child, mut sock) = spawn_shim("bash --norc --noprofile");

    recv_until(&mut sock, Duration::from_secs(10), |e| matches!(e, Evt::Ready))
        .expect("Ready timeout");

    send_msg(&mut sock, &Cmd::GetState);
    let evt = recv_until(&mut sock, Duration::from_secs(2), |e| matches!(e, Evt::State { .. }));
    match evt {
        Some(Evt::State { state, .. }) => {
            assert_eq!(state, "idle", "agent should be idle after Ready");
        }
        other => panic!("expected State, got: {other:?}"),
    }

    send_msg(&mut sock, &Cmd::Shutdown { timeout_secs: 2 });
    child.wait().ok();
}

#[test]
fn shim_agent_exit_produces_died() {
    let (mut child, mut sock) = spawn_shim("bash --norc --noprofile");

    recv_until(&mut sock, Duration::from_secs(10), |e| matches!(e, Evt::Ready))
        .expect("Ready timeout");

    // Tell bash to exit
    send_msg(&mut sock, &Cmd::SendMessage {
        from: "test".into(),
        body: "exit 0".into(),
        message_id: None,
    });

    // Should get a Died event
    let evt = recv_until(&mut sock, Duration::from_secs(10), |e| matches!(e, Evt::Died { .. }));
    assert!(evt.is_some(), "expected Died event after 'exit'");

    child.wait().ok();
}
