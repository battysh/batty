//! Wire protocol: Commands (orchestrator→shim) and Events (shim→orchestrator).
//!
//! Transport: Unix SOCK_SEQPACKET socketpair. Each message is one JSON object.
//! SEQPACKET preserves message boundaries, so no length-prefix framing needed.

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
    },
    Kill,
    Ping,
}

// ---------------------------------------------------------------------------
// Events (sent FROM the shim)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum Event {
    Ready,
    StateChanged {
        from: ShimState,
        to: ShimState,
        summary: String,
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
    ScreenCapture {
        content: String,
        cursor_row: u16,
        cursor_col: u16,
    },
    State {
        state: ShimState,
        since_secs: u64,
    },
    Pong,
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
/// If the OS supports SOCK_SEQPACKET we could skip the length prefix, but
/// portable-pty's ChildKiller / std UnixStream only expose SOCK_STREAM.
/// So we use 4-byte big-endian length + JSON payload for robustness.
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
    fn roundtrip_command() {
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
            Command::SendMessage { from, body, message_id } => {
                assert_eq!(from, "user");
                assert_eq!(body, "say hello");
                assert_eq!(message_id.as_deref(), Some("msg-1"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_event() {
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
}
