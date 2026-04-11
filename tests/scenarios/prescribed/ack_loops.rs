//! Ack loops: the manager and engineer repeatedly ack the same
//! message. Dedup in `report_preserve_failure` and related surfaces
//! must prevent a cascade of identical alerts.
//!
//! This scenario extends `preserve_dedup` (ticket #641) to multiple
//! contexts: calling the reporter with DIFFERENT contexts should
//! produce multiple entries, but calling with the SAME context
//! should dedup.

use super::super::scenarios_common::ScenarioFixture;

#[test]
fn ack_loops_dedup_distinguishes_contexts_but_suppresses_duplicates() {
    let mut fixture = ScenarioFixture::builder()
        .with_manager("manager")
        .with_engineers(1)
        .build();

    // Same context, same detail, same task: should dedup.
    for _ in 0..3 {
        fixture
            .daemon_mut()
            .scenario_hooks()
            .report_preserve_failure_for_test(
                "eng-1",
                Some(1),
                "task reset",
                "dirty worktree: a.rs",
            );
    }
    assert_eq!(
        fixture
            .daemon_mut()
            .scenario_hooks()
            .recent_escalations_count(),
        1,
        "3 identical calls should dedup to 1 escalation"
    );

    // Different context: separate dedup key.
    fixture
        .daemon_mut()
        .scenario_hooks()
        .report_preserve_failure_for_test(
            "eng-1",
            Some(1),
            "daemon shutdown",
            "dirty worktree: a.rs",
        );
    assert_eq!(
        fixture
            .daemon_mut()
            .scenario_hooks()
            .recent_escalations_count(),
        2,
        "new context should create a new escalation entry"
    );

    // Different task: separate dedup key.
    fixture
        .daemon_mut()
        .scenario_hooks()
        .report_preserve_failure_for_test("eng-1", Some(2), "task reset", "dirty worktree: b.rs");
    assert_eq!(
        fixture
            .daemon_mut()
            .scenario_hooks()
            .recent_escalations_count(),
        3
    );

    fixture.assert_state_consistent();
}
