//! Per-agent shim handle: owns the Channel, tracks state, manages lifecycle.
//!
//! When `use_shim` is enabled in team config, each agent member gets an
//! `AgentHandle` instead of a tmux-backed watcher. The handle holds the
//! orchestrator side of the socketpair channel, the child process, and the
//! last known shim state.

use std::path::PathBuf;
use std::time::Instant;

use crate::shim::protocol::{Channel, Command, Event, ShimState, ShutdownReason};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::team) struct InFlightMessage {
    pub from: String,
    pub body: String,
    pub message_id: Option<String>,
}

/// Per-agent handle for a shim subprocess.
pub(in crate::team) struct AgentHandle {
    /// Unique agent instance name (e.g. "eng-1-1").
    pub id: String,
    /// Orchestrator-side channel to the shim process.
    pub channel: Channel,
    /// Child process ID for lifecycle management.
    pub child_pid: u32,
    /// Last known shim state.
    pub state: ShimState,
    /// When the state last changed.
    pub state_changed_at: Instant,
    /// When the last Pong was received (None until first Pong).
    pub last_pong_at: Option<Instant>,
    /// Agent type for respawning (e.g. "claude", "codex").
    pub agent_type: String,
    /// Agent command for respawning.
    pub agent_cmd: String,
    /// Working directory for respawning.
    pub work_dir: PathBuf,
    /// Last known pane width (for resize sync).
    pub last_cols: u16,
    /// Last known pane height (for resize sync).
    pub last_rows: u16,
    /// Latest session output byte count reported by the shim.
    pub output_bytes: u64,
    /// Approximate total input bytes sent to the agent session.
    pub input_bytes: u64,
    /// When the shim last reported activity.
    pub last_activity_at: Option<Instant>,
    /// Most recent message delivered into the shim and still awaiting completion.
    pub in_flight_message: Option<InFlightMessage>,
}

impl AgentHandle {
    /// Create a new handle for a freshly spawned shim.
    pub fn new(
        id: String,
        channel: Channel,
        child_pid: u32,
        agent_type: String,
        agent_cmd: String,
        work_dir: PathBuf,
    ) -> Self {
        Self {
            id,
            channel,
            child_pid,
            state: ShimState::Starting,
            state_changed_at: Instant::now(),
            last_pong_at: None,
            agent_type,
            agent_cmd,
            work_dir,
            last_cols: 0,
            last_rows: 0,
            output_bytes: 0,
            input_bytes: 0,
            last_activity_at: None,
            in_flight_message: None,
        }
    }

    /// Update shim state from a received event.
    pub fn apply_state_change(&mut self, new_state: ShimState) {
        self.state = new_state;
        self.state_changed_at = Instant::now();
        self.record_activity();
    }

    /// Whether the agent is ready to receive messages.
    pub fn is_ready(&self) -> bool {
        self.state == ShimState::Idle
    }

    /// Whether the agent is currently working.
    #[allow(dead_code)]
    pub fn is_working(&self) -> bool {
        self.state == ShimState::Working
    }

    /// Whether the agent is dead or context-exhausted.
    pub fn is_terminal(&self) -> bool {
        matches!(self.state, ShimState::Dead | ShimState::ContextExhausted)
    }

    /// Send a message to the shim.
    pub fn send_message(&mut self, from: &str, body: &str) -> anyhow::Result<()> {
        self.send_message_with_id(from, body, None)
    }

    /// Send a message to the shim with a caller-supplied delivery id.
    pub fn send_message_with_id(
        &mut self,
        from: &str,
        body: &str,
        message_id: Option<String>,
    ) -> anyhow::Result<()> {
        self.input_bytes = self
            .input_bytes
            .saturating_add(from.len() as u64 + body.len() as u64);
        self.record_activity();
        self.in_flight_message = Some(InFlightMessage {
            from: from.to_string(),
            body: body.to_string(),
            message_id: message_id.clone(),
        });
        self.channel.send(&Command::SendMessage {
            from: from.to_string(),
            body: body.to_string(),
            message_id,
        })
    }

    /// Send a shutdown command to the shim.
    pub fn send_shutdown(&mut self, timeout_secs: u32) -> anyhow::Result<()> {
        self.send_shutdown_with_reason(timeout_secs, ShutdownReason::Requested)
    }

    /// Send a shutdown command to the shim with an explicit disconnect reason.
    pub fn send_shutdown_with_reason(
        &mut self,
        timeout_secs: u32,
        reason: ShutdownReason,
    ) -> anyhow::Result<()> {
        self.channel.send(&Command::Shutdown {
            timeout_secs,
            reason,
        })
    }

    /// Send a kill command to the shim.
    pub fn send_kill(&mut self) -> anyhow::Result<()> {
        self.channel.send(&Command::Kill)
    }

    /// Send a ping to the shim for health monitoring.
    pub fn send_ping(&mut self) -> anyhow::Result<()> {
        self.channel.send(&Command::Ping)
    }

    /// Record that a Pong was received.
    pub fn record_pong(&mut self) {
        let now = Instant::now();
        self.last_pong_at = Some(now);
        self.last_activity_at = Some(now);
    }

    pub fn record_output_bytes(&mut self, output_bytes: u64) {
        self.output_bytes = output_bytes;
        self.record_activity();
    }

    pub fn record_activity(&mut self) {
        self.last_activity_at = Some(Instant::now());
    }

    pub fn clear_in_flight_message(&mut self) {
        self.in_flight_message = None;
    }

    pub fn take_in_flight_message(&mut self) -> Option<InFlightMessage> {
        self.in_flight_message.take()
    }

    /// Seconds since last Pong, or None if no Pong received yet.
    pub fn secs_since_last_pong(&self) -> Option<u64> {
        self.last_pong_at.map(|t| t.elapsed().as_secs())
    }

    /// Seconds since the shim last reported any activity, or None if never.
    pub fn secs_since_last_activity(&self) -> Option<u64> {
        self.last_activity_at.map(|t| t.elapsed().as_secs())
    }

    /// Try to receive an event from the shim (non-blocking via timeout).
    /// Returns Ok(None) on EOF or timeout.
    pub fn try_recv_event(&mut self) -> anyhow::Result<Option<Event>> {
        match self.channel.recv::<Event>() {
            Ok(event) => Ok(event),
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
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    /// Seconds since last state change.
    #[allow(dead_code)]
    pub fn secs_since_state_change(&self) -> u64 {
        self.state_changed_at.elapsed().as_secs()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shim::protocol::socketpair;

    fn make_test_handle() -> (AgentHandle, Channel) {
        let (parent_sock, child_sock) = socketpair().unwrap();
        let parent_channel = Channel::new(parent_sock);
        let child_channel = Channel::new(child_sock);
        let handle = AgentHandle::new(
            "eng-1-1".into(),
            parent_channel,
            12345,
            "claude".into(),
            "claude".into(),
            PathBuf::from("/tmp/test"),
        );
        (handle, child_channel)
    }

    #[test]
    fn new_handle_starts_in_starting_state() {
        let (handle, _child) = make_test_handle();
        assert_eq!(handle.id, "eng-1-1");
        assert_eq!(handle.child_pid, 12345);
        assert_eq!(handle.state, ShimState::Starting);
        assert!(!handle.is_ready());
        assert!(!handle.is_working());
        assert!(!handle.is_terminal());
        assert!(handle.last_pong_at.is_none());
        assert!(handle.last_activity_at.is_none());
        assert!(handle.in_flight_message.is_none());
        assert_eq!(handle.agent_type, "claude");
        assert_eq!(handle.agent_cmd, "claude");
        assert_eq!(handle.work_dir, PathBuf::from("/tmp/test"));
    }

    #[test]
    fn apply_state_change_updates_state() {
        let (mut handle, _child) = make_test_handle();
        handle.apply_state_change(ShimState::Idle);
        assert_eq!(handle.state, ShimState::Idle);
        assert!(handle.is_ready());
        assert!(!handle.is_working());
    }

    #[test]
    fn apply_state_change_to_working() {
        let (mut handle, _child) = make_test_handle();
        handle.apply_state_change(ShimState::Working);
        assert!(handle.is_working());
        assert!(!handle.is_ready());
        assert!(!handle.is_terminal());
    }

    #[test]
    fn is_terminal_for_dead() {
        let (mut handle, _child) = make_test_handle();
        handle.apply_state_change(ShimState::Dead);
        assert!(handle.is_terminal());
    }

    #[test]
    fn is_terminal_for_context_exhausted() {
        let (mut handle, _child) = make_test_handle();
        handle.apply_state_change(ShimState::ContextExhausted);
        assert!(handle.is_terminal());
    }

    #[test]
    fn send_message_delivers_command_to_child() {
        let (mut handle, mut child) = make_test_handle();
        handle.send_message("manager", "do the thing").unwrap();

        let cmd: Command = child.recv().unwrap().unwrap();
        match cmd {
            Command::SendMessage { from, body, .. } => {
                assert_eq!(from, "manager");
                assert_eq!(body, "do the thing");
            }
            _ => panic!("expected SendMessage, got {:?}", cmd),
        }
    }

    #[test]
    fn send_shutdown_delivers_command() {
        let (mut handle, mut child) = make_test_handle();
        handle.send_shutdown(30).unwrap();

        let cmd: Command = child.recv().unwrap().unwrap();
        match cmd {
            Command::Shutdown {
                timeout_secs,
                reason,
            } => {
                assert_eq!(timeout_secs, 30);
                assert_eq!(reason, ShutdownReason::Requested);
            }
            _ => panic!("expected Shutdown"),
        }
    }

    #[test]
    fn send_kill_delivers_command() {
        let (mut handle, mut child) = make_test_handle();
        handle.send_kill().unwrap();

        let cmd: Command = child.recv().unwrap().unwrap();
        assert!(matches!(cmd, Command::Kill));
    }

    #[test]
    fn secs_since_state_change_is_zero_initially() {
        let (handle, _child) = make_test_handle();
        assert!(handle.secs_since_state_change() < 2);
    }

    #[test]
    fn state_lifecycle_starting_to_idle_to_working_to_idle() {
        let (mut handle, _child) = make_test_handle();
        assert_eq!(handle.state, ShimState::Starting);

        handle.apply_state_change(ShimState::Idle);
        assert!(handle.is_ready());

        handle.apply_state_change(ShimState::Working);
        assert!(handle.is_working());

        handle.apply_state_change(ShimState::Idle);
        assert!(handle.is_ready());
        assert!(!handle.is_working());
    }

    #[test]
    fn try_recv_event_returns_sent_event() {
        let (mut handle, mut child) = make_test_handle();
        child.send(&Event::Ready).unwrap();

        let event = handle.try_recv_event().unwrap().unwrap();
        assert!(matches!(event, Event::Ready));
    }

    #[test]
    fn state_changed_at_updates_on_apply() {
        let (mut handle, _child) = make_test_handle();
        let first = handle.state_changed_at;
        std::thread::sleep(std::time::Duration::from_millis(10));
        handle.apply_state_change(ShimState::Idle);
        assert!(handle.state_changed_at > first);
    }

    #[test]
    fn send_ping_delivers_command() {
        let (mut handle, mut child) = make_test_handle();
        handle.send_ping().unwrap();

        let cmd: Command = child.recv().unwrap().unwrap();
        assert!(matches!(cmd, Command::Ping));
    }

    #[test]
    fn record_pong_sets_last_pong_at() {
        let (mut handle, _child) = make_test_handle();
        assert!(handle.last_pong_at.is_none());
        assert!(handle.last_activity_at.is_none());
        assert!(handle.secs_since_last_pong().is_none());
        assert!(handle.secs_since_last_activity().is_none());

        handle.record_pong();
        assert!(handle.last_pong_at.is_some());
        assert!(handle.last_activity_at.is_some());
        assert_eq!(handle.secs_since_last_pong(), Some(0));
        assert_eq!(handle.secs_since_last_activity(), Some(0));
    }

    #[test]
    fn record_output_bytes_marks_recent_activity() {
        let (mut handle, _child) = make_test_handle();
        assert!(handle.last_activity_at.is_none());

        handle.record_output_bytes(128);

        assert_eq!(handle.output_bytes, 128);
        assert!(handle.last_activity_at.is_some());
        assert_eq!(handle.secs_since_last_activity(), Some(0));
    }

    #[test]
    fn send_message_tracks_input_bytes() {
        let (mut handle, mut child) = make_test_handle();
        handle.send_message("manager", "do the thing").unwrap();

        assert_eq!(
            handle.input_bytes,
            "manager".len() as u64 + "do the thing".len() as u64
        );

        let cmd: Command = child.recv().unwrap().unwrap();
        assert!(matches!(cmd, Command::SendMessage { .. }));
        let tracked = handle.take_in_flight_message().unwrap();
        assert_eq!(tracked.from, "manager");
        assert_eq!(tracked.body, "do the thing");
        assert!(handle.in_flight_message.is_none());
    }

    #[test]
    fn try_recv_event_returns_none_on_timeout() {
        let (mut handle, _child) = make_test_handle();
        handle
            .channel
            .set_read_timeout(Some(std::time::Duration::from_millis(5)))
            .unwrap();

        let event = handle.try_recv_event().unwrap();
        assert!(event.is_none());
    }
}
