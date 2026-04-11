//! State desync: a task is `in-progress` but the engineer's branch
//! state does not reflect an active commit. Reconcile must handle
//! this gracefully.

use super::super::scenarios_common::{ScenarioFixture, board_ops};

#[test]
fn state_desync_in_progress_task_without_active_branch_reconciles_cleanly() {
    let mut fixture = ScenarioFixture::builder()
        .with_manager("manager")
        .with_engineers(1)
        .with_task(1, "desync task", "in-progress", Some("eng-1"))
        .build();
    board_ops::init_git_repo(fixture.project_root());

    // Engineer's daemon-side state has NO active task (desync).
    // Reconcile must walk this without panicking and without creating
    // spurious preserve-failure alerts.
    let report = fixture.tick();

    let reconcile_errors: Vec<&(String, String)> = report
        .subsystem_errors
        .iter()
        .filter(|(step, _)| step.contains("reconcile"))
        .collect();
    assert!(
        reconcile_errors.is_empty(),
        "reconcile should handle state desync cleanly, got {:?}",
        reconcile_errors
    );
    fixture.assert_state_consistent();
}
