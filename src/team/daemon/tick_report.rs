//! Structured observability for one daemon poll iteration.
//!
//! `TickReport` is the return value of [`TeamDaemon::tick`]. It captures the
//! side effects produced during a single iteration of the daemon's poll loop
//! so callers (tests, the future `batty debug tick` subcommand, operators)
//! can assert against them without scraping logs.
//!
//! Phase 1 ships with `cycle` and `subsystem_errors` populated. The remaining
//! fields are intentionally `Default`-shaped placeholders so the contract is
//! stable for callers; later phases of the scenario framework will fill them
//! in by snapshotting state around the tick.

use crate::team::events::TeamEvent;
use crate::team::standup::MemberState;

/// Observable side effects produced by one [`TeamDaemon::tick`] call.
#[derive(Debug, Default, Clone)]
pub struct TickReport {
    /// Daemon poll cycle counter at the end of the tick.
    pub cycle: u64,

    /// Subsystem failures recorded during the tick. Each entry is
    /// `(step_name, error_text)` and represents one call to
    /// `record_loop_step_error`. Both transient (recoverable) and
    /// fatal-this-tick (loop_step) errors land here.
    pub subsystem_errors: Vec<(String, String)>,

    /// Events appended to `events.jsonl` during the tick. Empty in phase 1
    /// (placeholder for the scenario framework's diff-based snapshotting).
    pub events_emitted: Vec<TeamEvent>,

    /// Member state transitions observed during the tick.
    /// Empty in phase 1 (placeholder).
    pub state_transitions: Vec<(String, MemberState, MemberState)>,

    /// New `main` HEAD SHA if the tick advanced the main branch.
    /// `None` in phase 1 (placeholder).
    pub main_advanced_to: Option<String>,

    /// Inbox messages delivered during the tick, as `(recipient, message_id)`.
    /// Empty in phase 1 (placeholder).
    pub inbox_delivered: Vec<(String, String)>,

    /// Task status transitions observed during the tick, as
    /// `(task_id, from_status, to_status)`. Empty in phase 1 (placeholder).
    pub tasks_transitioned: Vec<(u32, String, String)>,
}

impl TickReport {
    /// Convenience constructor used by `tick()` to start a fresh report.
    pub(crate) fn new(cycle: u64) -> Self {
        Self {
            cycle,
            ..Self::default()
        }
    }

    /// True if the tick recorded no subsystem errors.
    pub fn ok(&self) -> bool {
        self.subsystem_errors.is_empty()
    }
}
