//! Regression for 0.10.2: `report_preserve_failure` must dedup
//! repeated alerts for the same (member, task, context, detail). Before
//! the fix every reconciliation cycle fired the same alert to engineer
//! + manager, creating tight ack loops that flooded the inbox.
//!
//! Test: call `report_preserve_failure` 5 times with identical args;
//! assert only the first call recorded a new escalation entry.

use super::super::super::scenarios_common::ScenarioFixture;

#[test]
fn preserve_dedup_suppresses_repeated_failures_in_window() {
    let mut fixture = ScenarioFixture::builder()
        .with_manager("manager")
        .with_engineers(1)
        .build();

    let mut new_escalations = 0usize;
    for _ in 0..5 {
        let recorded = fixture
            .daemon_mut()
            .scenario_hooks()
            .report_preserve_failure_for_test(
                "eng-1",
                Some(42),
                "task reset",
                "dirty worktree: src/lib.rs modified",
            );
        if recorded {
            new_escalations += 1;
        }
    }

    assert_eq!(
        new_escalations, 1,
        "exactly one escalation should be recorded across 5 identical calls, got {new_escalations}"
    );
    assert_eq!(
        fixture
            .daemon_mut()
            .scenario_hooks()
            .recent_escalations_count(),
        1,
        "recent_escalations should contain exactly one key"
    );
}
