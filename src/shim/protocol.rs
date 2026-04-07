//! Wire protocol: Commands (orchestrator→shim) and Events (shim→orchestrator).
//!
//! Transport: length-prefixed JSON over a Unix SOCK_STREAM socketpair.
//! 4-byte big-endian length prefix + JSON payload.

use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;

// ---------------------------------------------------------------------------
// Commands (sent TO the shim)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd")]
pub enum Command {
    SendMessage {
        from: String,
        body: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
    },
    CaptureScreen {
        last_n_lines: Option<usize>,
    },
    GetState,
    Resize {
        rows: u16,
        cols: u16,
    },
    Shutdown {
        timeout_secs: u32,
        #[serde(default)]
        reason: ShutdownReason,
    },
    Kill,
    Ping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ShutdownReason {
    #[default]
    Requested,
    RestartHandoff,
    ContextExhausted,
    TopologyChange,
    DaemonStop,
}

impl ShutdownReason {
    pub fn label(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::RestartHandoff => "restart_handoff",
            Self::ContextExhausted => "context_exhausted",
            Self::TopologyChange => "topology_change",
            Self::DaemonStop => "daemon_stop",
        }
    }
}

// ---------------------------------------------------------------------------
// Events (sent FROM the shim)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum Event {
    Ready,
    StateChanged {
        from: ShimState,
        to: ShimState,
        summary: String,
    },
    MessageDelivered {
        id: String,
    },
    Completion {
        #[serde(skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
        response: String,
        last_lines: String,
    },
    Died {
        exit_code: Option<i32>,
        last_lines: String,
    },
    ContextExhausted {
        message: String,
        last_lines: String,
    },
    ContextApproaching {
        message: String,
        input_tokens: u64,
        output_tokens: u64,
    },
    ScreenCapture {
        content: String,
        cursor_row: u16,
        cursor_col: u16,
    },
    State {
        state: ShimState,
        since_secs: u64,
    },
    SessionStats {
        output_bytes: u64,
        uptime_secs: u64,
        #[serde(default)]
        input_tokens: u64,
        #[serde(default)]
        output_tokens: u64,
    },
    Pong,
    Warning {
        message: String,
        idle_secs: Option<u64>,
    },
    DeliveryFailed {
        id: String,
        reason: String,
    },
    Error {
        command: String,
        reason: String,
    },
}

// ---------------------------------------------------------------------------
// Shim state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShimState {
    Starting,
    Idle,
    Working,
    Dead,
    ContextExhausted,
}

impl std::fmt::Display for ShimState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Starting => write!(f, "starting"),
            Self::Idle => write!(f, "idle"),
            Self::Working => write!(f, "working"),
            Self::Dead => write!(f, "dead"),
            Self::ContextExhausted => write!(f, "context_exhausted"),
        }
    }
}

// ---------------------------------------------------------------------------
// Framed channel over a Unix socket
// ---------------------------------------------------------------------------

/// Blocking, length-prefixed JSON channel over a Unix stream socket.
///
/// Uses 4-byte big-endian length + JSON payload for robustness.
pub struct Channel {
    stream: UnixStream,
    read_buf: Vec<u8>,
}

const MAX_MSG: usize = 1_048_576; // 1 MB

impl Channel {
    pub fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            read_buf: vec![0u8; 4096],
        }
    }

    /// Send a serializable message.
    pub fn send<T: Serialize>(&mut self, msg: &T) -> anyhow::Result<()> {
        let json = serde_json::to_vec(msg)?;
        if json.len() > MAX_MSG {
            anyhow::bail!("message too large: {} bytes", json.len());
        }
        let len = (json.len() as u32).to_be_bytes();
        self.stream.write_all(&len)?;
        self.stream.write_all(&json)?;
        self.stream.flush()?;
        Ok(())
    }

    /// Receive a deserializable message. Blocks until a message arrives.
    /// Returns Ok(None) on clean EOF (peer closed).
    pub fn recv<T: for<'de> Deserialize<'de>>(&mut self) -> anyhow::Result<Option<T>> {
        let mut len_buf = [0u8; 4];
        match self.stream.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > MAX_MSG {
            anyhow::bail!("incoming message too large: {} bytes", len);
        }
        if self.read_buf.len() < len {
            self.read_buf.resize(len, 0);
        }
        self.stream.read_exact(&mut self.read_buf[..len])?;
        let msg = serde_json::from_slice(&self.read_buf[..len])?;
        Ok(Some(msg))
    }

    /// Set a read timeout on the underlying socket.
    /// After this, `recv()` will return an error if no data arrives
    /// within the given duration (instead of blocking forever).
    pub fn set_read_timeout(&mut self, timeout: Option<std::time::Duration>) -> anyhow::Result<()> {
        self.stream.set_read_timeout(timeout)?;
        Ok(())
    }

    /// Clone the underlying fd for use in a second thread.
    pub fn try_clone(&self) -> anyhow::Result<Self> {
        Ok(Self {
            stream: self.stream.try_clone()?,
            read_buf: vec![0u8; 4096],
        })
    }
}

// ---------------------------------------------------------------------------
// Create a connected socketpair
// ---------------------------------------------------------------------------

/// Create a connected pair of Unix stream sockets.
/// Returns (parent_socket, child_socket).
pub fn socketpair() -> anyhow::Result<(UnixStream, UnixStream)> {
    let (a, b) = UnixStream::pair()?;
    Ok((a, b))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_command_send_message() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        let cmd = Command::SendMessage {
            from: "user".into(),
            body: "say hello".into(),
            message_id: Some("msg-1".into()),
        };
        sender.send(&cmd).unwrap();
        let received: Command = receiver.recv::<Command>().unwrap().unwrap();

        match received {
            Command::SendMessage {
                from,
                body,
                message_id,
            } => {
                assert_eq!(from, "user");
                assert_eq!(body, "say hello");
                assert_eq!(message_id.as_deref(), Some("msg-1"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_command_capture_screen() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        let cmd = Command::CaptureScreen {
            last_n_lines: Some(10),
        };
        sender.send(&cmd).unwrap();
        let received: Command = receiver.recv::<Command>().unwrap().unwrap();
        match received {
            Command::CaptureScreen { last_n_lines } => assert_eq!(last_n_lines, Some(10)),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_command_get_state() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        sender.send(&Command::GetState).unwrap();
        let received: Command = receiver.recv::<Command>().unwrap().unwrap();
        assert!(matches!(received, Command::GetState));
    }

    #[test]
    fn roundtrip_command_resize() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        let cmd = Command::Resize {
            rows: 50,
            cols: 220,
        };
        sender.send(&cmd).unwrap();
        let received: Command = receiver.recv::<Command>().unwrap().unwrap();
        match received {
            Command::Resize { rows, cols } => {
                assert_eq!(rows, 50);
                assert_eq!(cols, 220);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_command_shutdown() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        let cmd = Command::Shutdown {
            timeout_secs: 30,
            reason: ShutdownReason::Requested,
        };
        sender.send(&cmd).unwrap();
        let received: Command = receiver.recv::<Command>().unwrap().unwrap();
        match received {
            Command::Shutdown {
                timeout_secs,
                reason,
            } => {
                assert_eq!(timeout_secs, 30);
                assert_eq!(reason, ShutdownReason::Requested);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn shutdown_reason_labels_restart_handoff_explicitly() {
        assert_eq!(ShutdownReason::RestartHandoff.label(), "restart_handoff");
        assert_ne!(
            ShutdownReason::RestartHandoff.label(),
            "orchestrator disconnected"
        );
    }

    #[test]
    fn roundtrip_command_kill() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        sender.send(&Command::Kill).unwrap();
        let received: Command = receiver.recv::<Command>().unwrap().unwrap();
        assert!(matches!(received, Command::Kill));
    }

    #[test]
    fn roundtrip_command_ping() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        sender.send(&Command::Ping).unwrap();
        let received: Command = receiver.recv::<Command>().unwrap().unwrap();
        assert!(matches!(received, Command::Ping));
    }

    #[test]
    fn roundtrip_event_completion() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        let evt = Event::Completion {
            message_id: None,
            response: "Hello!".into(),
            last_lines: "Hello!\n❯".into(),
        };
        sender.send(&evt).unwrap();
        let received: Event = receiver.recv::<Event>().unwrap().unwrap();

        match received {
            Event::Completion { response, .. } => assert_eq!(response, "Hello!"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_event_message_delivered() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        let evt = Event::MessageDelivered { id: "msg-1".into() };
        sender.send(&evt).unwrap();
        let received: Event = receiver.recv::<Event>().unwrap().unwrap();

        match received {
            Event::MessageDelivered { id } => assert_eq!(id, "msg-1"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_event_state_changed() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        let evt = Event::StateChanged {
            from: ShimState::Idle,
            to: ShimState::Working,
            summary: "working now".into(),
        };
        sender.send(&evt).unwrap();
        let received: Event = receiver.recv::<Event>().unwrap().unwrap();
        match received {
            Event::StateChanged { from, to, summary } => {
                assert_eq!(from, ShimState::Idle);
                assert_eq!(to, ShimState::Working);
                assert_eq!(summary, "working now");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_event_ready() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        sender.send(&Event::Ready).unwrap();
        let received: Event = receiver.recv::<Event>().unwrap().unwrap();
        assert!(matches!(received, Event::Ready));
    }

    #[test]
    fn roundtrip_event_pong() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        sender.send(&Event::Pong).unwrap();
        let received: Event = receiver.recv::<Event>().unwrap().unwrap();
        assert!(matches!(received, Event::Pong));
    }

    #[test]
    fn roundtrip_event_delivery_failed() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        let evt = Event::DeliveryFailed {
            id: "msg-1".into(),
            reason: "stdin write failed".into(),
        };
        sender.send(&evt).unwrap();
        let received: Event = receiver.recv::<Event>().unwrap().unwrap();

        match received {
            Event::DeliveryFailed { id, reason } => {
                assert_eq!(id, "msg-1");
                assert_eq!(reason, "stdin write failed");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_event_died() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        let evt = Event::Died {
            exit_code: Some(1),
            last_lines: "error occurred".into(),
        };
        sender.send(&evt).unwrap();
        let received: Event = receiver.recv::<Event>().unwrap().unwrap();
        match received {
            Event::Died {
                exit_code,
                last_lines,
            } => {
                assert_eq!(exit_code, Some(1));
                assert_eq!(last_lines, "error occurred");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_event_context_exhausted() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        let evt = Event::ContextExhausted {
            message: "context full".into(),
            last_lines: "last output".into(),
        };
        sender.send(&evt).unwrap();
        let received: Event = receiver.recv::<Event>().unwrap().unwrap();
        match received {
            Event::ContextExhausted {
                message,
                last_lines,
            } => {
                assert_eq!(message, "context full");
                assert_eq!(last_lines, "last output");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_event_screen_capture() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        let evt = Event::ScreenCapture {
            content: "screen data".into(),
            cursor_row: 5,
            cursor_col: 10,
        };
        sender.send(&evt).unwrap();
        let received: Event = receiver.recv::<Event>().unwrap().unwrap();
        match received {
            Event::ScreenCapture {
                content,
                cursor_row,
                cursor_col,
            } => {
                assert_eq!(content, "screen data");
                assert_eq!(cursor_row, 5);
                assert_eq!(cursor_col, 10);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_event_session_stats() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        let evt = Event::SessionStats {
            output_bytes: 123_456,
            uptime_secs: 61,
            input_tokens: 5000,
            output_tokens: 1200,
        };
        sender.send(&evt).unwrap();
        let received: Event = receiver.recv::<Event>().unwrap().unwrap();
        match received {
            Event::SessionStats {
                output_bytes,
                uptime_secs,
                input_tokens,
                output_tokens,
            } => {
                assert_eq!(output_bytes, 123_456);
                assert_eq!(uptime_secs, 61);
                assert_eq!(input_tokens, 5000);
                assert_eq!(output_tokens, 1200);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_event_context_approaching() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        let evt = Event::ContextApproaching {
            message: "context pressure detected".into(),
            input_tokens: 80000,
            output_tokens: 20000,
        };
        sender.send(&evt).unwrap();
        let received: Event = receiver.recv::<Event>().unwrap().unwrap();
        match received {
            Event::ContextApproaching {
                message,
                input_tokens,
                output_tokens,
            } => {
                assert_eq!(message, "context pressure detected");
                assert_eq!(input_tokens, 80000);
                assert_eq!(output_tokens, 20000);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_event_error() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        let evt = Event::Error {
            command: "SendMessage".into(),
            reason: "agent busy".into(),
        };
        sender.send(&evt).unwrap();
        let received: Event = receiver.recv::<Event>().unwrap().unwrap();
        match received {
            Event::Error { command, reason } => {
                assert_eq!(command, "SendMessage");
                assert_eq!(reason, "agent busy");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_event_warning() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        let evt = Event::Warning {
            message: "no screen change".into(),
            idle_secs: Some(300),
        };
        sender.send(&evt).unwrap();
        let received: Event = receiver.recv::<Event>().unwrap().unwrap();
        match received {
            Event::Warning { message, idle_secs } => {
                assert_eq!(message, "no screen change");
                assert_eq!(idle_secs, Some(300));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn eof_returns_none() {
        let (a, b) = socketpair().unwrap();
        drop(a); // close sender
        let mut receiver = Channel::new(b);
        let result: Option<Command> = receiver.recv().unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn all_states_serialize() {
        for state in [
            ShimState::Starting,
            ShimState::Idle,
            ShimState::Working,
            ShimState::Dead,
            ShimState::ContextExhausted,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let back: ShimState = serde_json::from_str(&json).unwrap();
            assert_eq!(state, back);
        }
    }

    #[test]
    fn shim_state_display() {
        assert_eq!(ShimState::Starting.to_string(), "starting");
        assert_eq!(ShimState::Idle.to_string(), "idle");
        assert_eq!(ShimState::Working.to_string(), "working");
        assert_eq!(ShimState::Dead.to_string(), "dead");
        assert_eq!(ShimState::ContextExhausted.to_string(), "context_exhausted");
    }

    #[test]
    fn socketpair_creates_connected_pair() {
        let (a, b) = socketpair().unwrap();
        // Basic connectivity: write on a, read on b
        let mut ch_a = Channel::new(a);
        let mut ch_b = Channel::new(b);
        ch_a.send(&Command::Ping).unwrap();
        let msg: Command = ch_b.recv().unwrap().unwrap();
        assert!(matches!(msg, Command::Ping));
    }
}
