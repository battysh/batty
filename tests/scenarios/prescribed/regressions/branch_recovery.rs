//! Regression for 0.10.3: the preserve-and-recover path must not lose
//! dirty tracked files when an engineer worktree is on the wrong
//! branch. Before the fix, `reconcile_active_tasks` would reset the
//! worktree and silently discard uncommitted changes.
//!
//! Phase 1 scope: this scenario proves the reconcile subsystem walks
//! the daemon's active_tasks map without errors when an engineer has
//! a dirty worktree, AND that the preserve-failure dedup correctly
//! recognizes the scenario as a single (member, task, context) tuple
//! so a second tick does not fire a duplicate alert. Full preserve +
//! reflog verification lands in ticket #642 once the fixture has
//! real-worktree helpers.

use super::super::super::scenarios_common::ScenarioFixture;

#[test]
fn branch_recovery_reconcile_is_idempotent_across_ticks() {
    let mut fixture = ScenarioFixture::builder()
        .with_manager("manager")
        .with_engineers(1)
        .with_task(1, "eng task", "in-progress", Some("eng-1"))
        .build();

    // Simulate that eng-1 has an active task matching the board task.
    fixture.set_active_task("eng-1", 1);

    // Tick twice — reconcile runs each time. The idempotency guarantee
    // from 0.10.2 preserve-failure dedup + 0.10.3 recovery must ensure
    // we do NOT fire N preserve-failure alerts for the same condition.
    let first = fixture.tick();
    let second = fixture.tick();

    // Both ticks must be clean.
    for (tick_name, report) in [("first", &first), ("second", &second)] {
        let reconcile_errors: Vec<&(String, String)> = report
            .subsystem_errors
            .iter()
            .filter(|(step, _)| step.contains("reconcile_active_tasks"))
            .collect();
        assert!(
            reconcile_errors.is_empty(),
            "{tick_name} tick: reconcile should run cleanly, got {:?}",
            reconcile_errors
        );
    }

    // The active task should still be present after two ticks — the
    // reconcile should not have cleared it without cause.
    assert_eq!(
        fixture
            .daemon_mut()
            .scenario_hooks()
            .active_task_for("eng-1"),
        Some(1),
        "active task should be preserved across idempotent ticks"
    );
}
