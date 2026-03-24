//! Per-agent shim handle: owns the Channel, tracks state, manages lifecycle.
//!
//! When `use_shim` is enabled in team config, each agent member gets an
//! `AgentHandle` instead of a tmux-backed watcher. The handle holds the
//! orchestrator side of the socketpair channel, the child process, and the
//! last known shim state.

use std::time::Instant;

use crate::shim::protocol::{Channel, Command, Event, ShimState};

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
}

impl AgentHandle {
    /// Create a new handle for a freshly spawned shim.
    pub fn new(id: String, channel: Channel, child_pid: u32) -> Self {
        Self {
            id,
            channel,
            child_pid,
            state: ShimState::Starting,
            state_changed_at: Instant::now(),
        }
    }

    /// Update shim state from a received event.
    pub fn apply_state_change(&mut self, new_state: ShimState) {
        self.state = new_state;
        self.state_changed_at = Instant::now();
    }

    /// Whether the agent is ready to receive messages.
    pub fn is_ready(&self) -> bool {
        self.state == ShimState::Idle
    }

    /// Whether the agent is currently working.
    pub fn is_working(&self) -> bool {
        self.state == ShimState::Working
    }

    /// Whether the agent is dead or context-exhausted.
    pub fn is_terminal(&self) -> bool {
        matches!(self.state, ShimState::Dead | ShimState::ContextExhausted)
    }

    /// Send a message to the shim.
    pub fn send_message(&mut self, from: &str, body: &str) -> anyhow::Result<()> {
        self.channel.send(&Command::SendMessage {
            from: from.to_string(),
            body: body.to_string(),
            message_id: None,
        })
    }

    /// Send a shutdown command to the shim.
    pub fn send_shutdown(&mut self, timeout_secs: u32) -> anyhow::Result<()> {
        self.channel.send(&Command::Shutdown { timeout_secs })
    }

    /// Send a kill command to the shim.
    pub fn send_kill(&mut self) -> anyhow::Result<()> {
        self.channel.send(&Command::Kill)
    }

    /// Try to receive an event from the shim (non-blocking via timeout).
    /// Returns Ok(None) on EOF or timeout.
    pub fn try_recv_event(&mut self) -> anyhow::Result<Option<Event>> {
        // Set a short read timeout so we don't block the daemon loop
        self.channel.recv::<Event>()
    }

    /// Seconds since last state change.
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
        let handle = AgentHandle::new("eng-1-1".into(), parent_channel, 12345);
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
            Command::Shutdown { timeout_secs } => assert_eq!(timeout_secs, 30),
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
}
