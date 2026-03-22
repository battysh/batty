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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    use crate::team::config::BoardConfig;
    use crate::team::standup::MemberState;
    use crate::team::test_support::{TestDaemonBuilder, engineer_member, manager_member};

    #[test]
    fn holds_when_no_idle_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("mgr", None),
                engineer_member("eng-1", Some("mgr"), false),
            ])
            .board(BoardConfig {
                dispatch_stabilization_delay_secs: 10,
                ..BoardConfig::default()
            })
            .build();

        assert!(
            daemon.should_hold_dispatch_for_stabilization("eng-1"),
            "should hold when no idle_started_at entry"
        );
    }

    #[test]
    fn holds_when_within_delay() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("mgr", None),
                engineer_member("eng-1", Some("mgr"), false),
            ])
            .board(BoardConfig {
                dispatch_stabilization_delay_secs: 30,
                ..BoardConfig::default()
            })
            .build();
        daemon
            .idle_started_at
            .insert("eng-1".to_string(), Instant::now() - Duration::from_secs(5));

        assert!(
            daemon.should_hold_dispatch_for_stabilization("eng-1"),
            "should hold when idle for 5s with 30s delay"
        );
    }

    #[test]
    fn releases_after_delay_expires() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("mgr", None),
                engineer_member("eng-1", Some("mgr"), false),
            ])
            .board(BoardConfig {
                dispatch_stabilization_delay_secs: 10,
                ..BoardConfig::default()
            })
            .build();
        daemon.idle_started_at.insert(
            "eng-1".to_string(),
            Instant::now() - Duration::from_secs(60),
        );

        assert!(
            !daemon.should_hold_dispatch_for_stabilization("eng-1"),
            "should release after delay expires"
        );
    }

    #[test]
    fn zero_delay_releases_immediately() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("mgr", None),
                engineer_member("eng-1", Some("mgr"), false),
            ])
            .board(BoardConfig {
                dispatch_stabilization_delay_secs: 0,
                ..BoardConfig::default()
            })
            .build();
        // Even with idle_started_at set to now, a 0-second delay means
        // it should always be expired.
        daemon.idle_started_at.insert(
            "eng-1".to_string(),
            Instant::now() - Duration::from_millis(1),
        );

        assert!(
            !daemon.should_hold_dispatch_for_stabilization("eng-1"),
            "zero delay should release immediately"
        );
    }

    #[test]
    fn unknown_engineer_holds() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = TestDaemonBuilder::new(tmp.path())
            .board(BoardConfig {
                dispatch_stabilization_delay_secs: 10,
                ..BoardConfig::default()
            })
            .build();

        assert!(
            daemon.should_hold_dispatch_for_stabilization("nonexistent"),
            "unknown engineer has no idle entry, should hold"
        );
    }
}
