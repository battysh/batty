//! Post-merge cooldown: prevents premature re-dispatch after an engineer
//! transitions to idle.

use std::time::Duration;

use super::super::*;

impl TeamDaemon {
    pub(in super::super) fn should_hold_dispatch_for_stabilization(&self, engineer: &str) -> bool {
        let idle_since = self.idle_started_at.get(engineer);
        let delay = Duration::from_secs(
            self.config
                .team_config
                .board
                .dispatch_stabilization_delay_secs,
        );
        idle_since.is_none_or(|started| started.elapsed() < delay)
    }
}
