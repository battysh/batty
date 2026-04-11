//! Merge conflict: direct merge fails, cherry-pick path succeeds.
//! Phase 1 scope: drive merge queue over a review task with a
//! branch that diverged from main, assert no merge queue subsystem
//! panics.

use super::super::super::scenarios_common::{ScenarioFixture, board_ops};

#[test]
fn merge_conflicts_cherry_pick_fallback_subsystem_runs_cleanly() {
    let mut fixture = ScenarioFixture::builder()
        .with_manager("manager")
        .with_engineers(1)
        .with_task(1, "cherry-pick task", "review", Some("eng-1"))
        .build();
    board_ops::init_git_repo(fixture.project_root());

    let report = fixture.tick();

    let merge_errors: Vec<&(String, String)> = report
        .subsystem_errors
        .iter()
        .filter(|(step, _)| step.contains("merge_queue"))
        .collect();
    assert!(
        merge_errors.is_empty(),
        "merge queue should not panic on review task with no real branch state, got {:?}",
        merge_errors
    );
    fixture.assert_state_consistent();
}
