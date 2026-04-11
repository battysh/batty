//! Merge conflict: two engineer branches touch the same file. First
//! lands on main; the second must rebase on top. This scenario
//! verifies the merge queue subsystem walks two review-state tasks
//! without crashing — the full rebase retry lands once the merge
//! pipeline is wired into the fixture.

use super::super::super::scenarios_common::{ScenarioFixture, board_ops};

#[test]
fn merge_conflicts_rebase_retry_subsystem_clean_on_two_review_tasks() {
    let mut fixture = ScenarioFixture::builder()
        .with_manager("manager")
        .with_engineers(2)
        .with_task(1, "first conflict task", "review", Some("eng-1"))
        .with_task(2, "second conflict task", "review", Some("eng-2"))
        .build();
    board_ops::init_git_repo(fixture.project_root());

    let report = fixture.tick();

    let relevant: Vec<&(String, String)> = report
        .subsystem_errors
        .iter()
        .filter(|(step, _)| step.contains("merge_queue") || step.contains("process_merge_queue"))
        .collect();
    assert!(
        relevant.is_empty(),
        "merge queue should walk two review tasks cleanly, got {:?}",
        relevant
    );
    fixture.assert_state_consistent();
}
