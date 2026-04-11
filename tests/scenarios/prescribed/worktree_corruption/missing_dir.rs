//! Worktree corruption: the engineer's `.batty/worktrees/eng-1/`
//! directory disappears between ticks. The daemon must detect this
//! during reconcile and not panic.
//!
//! Phase 1 scope: create a synthetic worktree dir, delete it, drive
//! a tick, assert no reconcile/worktree-staleness subsystem errors.
//! Recreating the worktree end-to-end requires engineer worktree
//! bootstrap that future fixture work will wire in.

use super::super::super::scenarios_common::{ScenarioFixture, board_ops};

#[test]
fn worktree_corruption_missing_dir_reconciles_cleanly() {
    let mut fixture = ScenarioFixture::builder()
        .with_manager("manager")
        .with_engineers(1)
        .with_task(1, "drive reconcile", "in-progress", Some("eng-1"))
        .build();
    board_ops::init_git_repo(fixture.project_root());

    // Create the worktree directory, then delete it mid-scenario.
    let worktree = fixture
        .project_root()
        .join(".batty")
        .join("worktrees")
        .join("eng-1");
    std::fs::create_dir_all(&worktree).unwrap();
    std::fs::remove_dir_all(&worktree).unwrap();

    fixture.set_active_task("eng-1", 1);

    // Tick: reconcile + worktree-staleness should walk the board
    // cleanly, noting the missing dir without crashing.
    let report = fixture.tick();
    let relevant: Vec<&(String, String)> = report
        .subsystem_errors
        .iter()
        .filter(|(step, _)| {
            step.contains("reconcile")
                || step.contains("worktree_staleness")
                || step.contains("reconcile_stale_worktrees")
        })
        .collect();
    assert!(
        relevant.is_empty(),
        "worktree-corruption tick should not error, got {:?}",
        relevant
    );

    fixture.assert_state_consistent();
}
