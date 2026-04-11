//! Scenario framework public test-API surface.
//!
//! Integration tests in `tests/` live in a separate crate and cannot see
//! `pub(super)` or `pub(crate)` items on [`TeamDaemon`]. This module exposes
//! a narrow, deliberately public test-API surface ([`ScenarioHooks`]) so the
//! scenario framework can drive the daemon (insert fake shim handles,
//! backdate timers, inspect state) without broadly widening internal
//! visibility.
//!
//! The module is gated by `#[cfg(any(test, feature = "scenario-test"))]` so
//! it does not exist in release builds. The gate is also why we can expose
//! `ScenarioHooks` as truly `pub` without leaking into the shipped binary.
//!
//! Phase 1 of the scenario framework (ticket #637). See
//! `planning/scenario-framework-execution.md` for the full plan.

#![cfg(any(test, feature = "scenario-test"))]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::shim::protocol::{Channel, ShimState};
use crate::team::daemon::TeamDaemon;
use crate::team::daemon::agent_handle::AgentHandle;
use crate::team::standup::MemberState;

/// Thin mutable wrapper around [`TeamDaemon`] that exposes a curated set of
/// test-only hooks. Obtained via [`TeamDaemon::scenario_hooks`].
///
/// Every method here is either an injection primitive (insert a fake shim,
/// backdate a timer) or a pure read-only introspection helper. No business
/// logic belongs here — scenarios call the real daemon methods for that.
pub struct ScenarioHooks<'a> {
    daemon: &'a mut TeamDaemon,
}

impl<'a> ScenarioHooks<'a> {
    /// Construct hooks borrowing a mutable reference to the daemon. Used by
    /// [`TeamDaemon::scenario_hooks`]; tests should not call this directly.
    pub(crate) fn new(daemon: &'a mut TeamDaemon) -> Self {
        Self { daemon }
    }

    // -----------------------------------------------------------------
    // Shim handle injection
    // -----------------------------------------------------------------

    /// Inject a fake-shim [`AgentHandle`] into the daemon's shim map. The
    /// caller supplies the parent side of a [`Channel`] paired with the
    /// [`FakeShim`](crate::shim::fake::FakeShim)'s child channel.
    ///
    /// This is the canonical replacement for the ad-hoc
    /// `insert_handle_with_channel` helper used inside `ping_pong.rs`
    /// tests: scenarios must go through this method so all fake-shim
    /// injection has a single, documented seam.
    pub fn insert_fake_shim(
        &mut self,
        name: &str,
        parent_channel: Channel,
        child_pid: u32,
        agent_type: &str,
        agent_cmd: &str,
        work_dir: PathBuf,
    ) {
        let handle = AgentHandle::new(
            name.to_string(),
            parent_channel,
            child_pid,
            agent_type.to_string(),
            agent_cmd.to_string(),
            work_dir,
        );
        self.daemon.shim_handles.insert(name.to_string(), handle);
    }

    /// Number of shim handles currently registered. Read-only.
    pub fn shim_handle_count(&self) -> usize {
        self.daemon.shim_handles.len()
    }

    /// Inspect the current shim state for `name`. Returns `None` if no
    /// handle is registered for that member.
    pub fn inspect_shim_state(&self, name: &str) -> Option<ShimState> {
        self.daemon
            .shim_handles
            .get(name)
            .map(|handle| handle.state)
    }

    /// Remove a shim handle. Mirrors the shutdown path; tests use this to
    /// simulate an agent dying.
    pub fn remove_shim_handle(&mut self, name: &str) -> bool {
        self.daemon.shim_handles.remove(name).is_some()
    }

    // -----------------------------------------------------------------
    // Time warp
    // -----------------------------------------------------------------

    /// Backdate the `state_changed_at` timestamp for a shim handle. After
    /// the call, the next tick sees the member's state as having been
    /// stable for `by` longer than it actually has. Used to force
    /// stall-detection timeouts without waiting in real time.
    pub fn backdate_shim_state_change(&mut self, name: &str, by: Duration) {
        if let Some(handle) = self.daemon.shim_handles.get_mut(name) {
            handle.state_changed_at = handle
                .state_changed_at
                .checked_sub(by)
                .unwrap_or_else(|| Instant::now().checked_sub(by).unwrap_or(Instant::now()));
        }
    }

    /// Backdate `last_activity_at` for a shim handle. Simulates a silent
    /// (hung) agent.
    pub fn backdate_shim_last_activity(&mut self, name: &str, by: Duration) {
        if let Some(handle) = self.daemon.shim_handles.get_mut(name) {
            if let Some(ts) = handle.last_activity_at.as_mut() {
                *ts = ts
                    .checked_sub(by)
                    .unwrap_or_else(|| Instant::now().checked_sub(by).unwrap_or(Instant::now()));
            } else {
                handle.last_activity_at = Instant::now().checked_sub(by);
            }
        }
    }

    /// Backdate the daemon's `last_shim_health_check` timestamp. Forces
    /// the next tick to run a shim health check immediately.
    pub fn backdate_last_shim_health_check(&mut self, by: Duration) {
        self.daemon.last_shim_health_check = self
            .daemon
            .last_shim_health_check
            .checked_sub(by)
            .unwrap_or_else(|| Instant::now().checked_sub(by).unwrap_or(Instant::now()));
    }

    /// Backdate the daemon's `last_disk_hygiene_check` timestamp. Forces
    /// the next tick to run disk hygiene.
    pub fn backdate_last_disk_hygiene(&mut self, by: Duration) {
        self.daemon.last_disk_hygiene_check = self
            .daemon
            .last_disk_hygiene_check
            .checked_sub(by)
            .unwrap_or_else(|| Instant::now().checked_sub(by).unwrap_or(Instant::now()));
    }

    /// Force a stall-state timeout for `member` by backdating its
    /// `state_changed_at` past `shim_working_state_timeout_secs`. Tests
    /// use this instead of wall-clock sleeping.
    pub fn force_stall_timeout(&mut self, member: &str) {
        let timeout = self
            .daemon
            .config
            .team_config
            .shim_working_state_timeout_secs;
        self.backdate_shim_state_change(member, Duration::from_secs(timeout + 60));
    }

    // -----------------------------------------------------------------
    // Board and state introspection (read-only)
    // -----------------------------------------------------------------

    /// The task id a member is currently assigned to (from the daemon's
    /// in-memory `active_tasks` map). Returns `None` if the member has no
    /// active task.
    pub fn active_task_for(&self, member: &str) -> Option<u32> {
        self.daemon.active_tasks.get(member).copied()
    }

    /// Currently-tracked [`MemberState`] for `member`, or `None` if the
    /// daemon has no state entry (e.g. the member hasn't started yet).
    pub fn member_state(&self, member: &str) -> Option<MemberState> {
        self.daemon.states.get(member).copied()
    }

    /// Current poll cycle counter. Useful for assertions about how many
    /// ticks a scenario has driven.
    pub fn poll_cycle_count(&self) -> u64 {
        self.daemon.poll_cycle_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shim::protocol::{self, Channel};
    use crate::team::test_support::TestDaemonBuilder;

    fn bootstrap_board(dir: &std::path::Path) {
        std::fs::create_dir_all(dir.join(".batty/team_config/board/tasks")).unwrap();
    }

    #[test]
    fn scenario_hooks_insert_and_inspect_shim() {
        let tmp = tempfile::tempdir().unwrap();
        bootstrap_board(tmp.path());
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();

        let (parent, _child) = protocol::socketpair().unwrap();
        let parent_channel = Channel::new(parent);

        let mut hooks = daemon.scenario_hooks();
        assert_eq!(hooks.shim_handle_count(), 0);

        hooks.insert_fake_shim(
            "eng-1",
            parent_channel,
            12345,
            "claude",
            "claude",
            PathBuf::from("/tmp/fake"),
        );

        assert_eq!(hooks.shim_handle_count(), 1);
        assert_eq!(
            hooks.inspect_shim_state("eng-1"),
            Some(ShimState::Starting),
            "freshly inserted handles start in Starting state"
        );
        assert_eq!(hooks.inspect_shim_state("eng-2"), None);
    }

    #[test]
    fn scenario_hooks_backdate_shim_state_change() {
        let tmp = tempfile::tempdir().unwrap();
        bootstrap_board(tmp.path());
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();

        let (parent, _child) = protocol::socketpair().unwrap();
        let mut hooks = daemon.scenario_hooks();
        hooks.insert_fake_shim(
            "eng-1",
            Channel::new(parent),
            1,
            "claude",
            "claude",
            PathBuf::from("/tmp"),
        );

        hooks.backdate_shim_state_change("eng-1", Duration::from_secs(120));
        // Can't assert the exact elapsed duration (wall-clock), but we can
        // confirm the handle still exists after backdating.
        assert_eq!(hooks.shim_handle_count(), 1);
    }

    #[test]
    fn scenario_hooks_force_stall_timeout_sets_state_far_past_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        bootstrap_board(tmp.path());
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();

        let (parent, _child) = protocol::socketpair().unwrap();
        let mut hooks = daemon.scenario_hooks();
        hooks.insert_fake_shim(
            "eng-1",
            Channel::new(parent),
            1,
            "claude",
            "claude",
            PathBuf::from("/tmp"),
        );

        hooks.force_stall_timeout("eng-1");
        // force_stall_timeout is a shortcut — just verify it didn't panic
        // and the handle still exists.
        assert_eq!(hooks.shim_handle_count(), 1);
    }

    #[test]
    fn scenario_hooks_remove_shim_handle() {
        let tmp = tempfile::tempdir().unwrap();
        bootstrap_board(tmp.path());
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();

        let (parent, _child) = protocol::socketpair().unwrap();
        let mut hooks = daemon.scenario_hooks();
        hooks.insert_fake_shim(
            "eng-1",
            Channel::new(parent),
            1,
            "claude",
            "claude",
            PathBuf::from("/tmp"),
        );

        assert!(hooks.remove_shim_handle("eng-1"));
        assert_eq!(hooks.shim_handle_count(), 0);
        assert!(!hooks.remove_shim_handle("eng-1"));
    }
}
