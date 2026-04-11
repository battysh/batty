//! Regression for the review-queue aging fix: a review task that sits
//! unclaimed past the aging threshold must be loadable and visible to
//! the manager's review-backlog intervention surface.
//!
//! Phase 1 scope: seed a review task via the fixture, drive a tick,
//! assert the review-queue subsystem ran without logging errors.
//! Manager-inbox delivery requires pane_map wiring phase 1 doesn't
//! provide; a full end-to-end assertion lands in ticket #642.

use super::super::super::scenarios_common::ScenarioFixture;

#[test]
fn review_queue_aging_walks_board_without_errors() {
    let mut fixture = ScenarioFixture::builder()
        .with_manager("manager")
        .with_engineers(1)
        // Seed via the builder's with_task path so the frontmatter
        // shape matches TestHarness::with_board_task exactly.
        .with_task(42, "aged review task", "review", None)
        .build();

    // Sanity BEFORE tick: the task is on the board in review status.
    let pre_tasks = batty_cli::task::load_tasks_from_dir(&fixture.board_tasks_dir()).unwrap();
    let pre_aged = pre_tasks
        .iter()
        .find(|t| t.id == 42)
        .expect("review task should be loadable");
    assert_eq!(
        pre_aged.status, "review",
        "seeded review task should load with status=review before tick"
    );

    let report = fixture.tick();

    // The review-queue subsystems must complete without errors. Before
    // the review-queue aging fix this path panicked on tasks with no
    // claimed_by + old timestamps.
    let relevant_errors: Vec<&(String, String)> = report
        .subsystem_errors
        .iter()
        .filter(|(step, _)| {
            step.contains("review_backlog")
                || step.contains("stale_reviews")
                || step.contains("task_aging")
        })
        .collect();
    assert!(
        relevant_errors.is_empty(),
        "review-queue subsystems should run cleanly on aged review task, got {:?}",
        relevant_errors
    );
}
