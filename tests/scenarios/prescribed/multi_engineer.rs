//! Multi-engineer: 3 engineers + 10 tasks on the board. The daemon
//! must walk the board and all subsystems without crashing across
//! several ticks.

use super::super::scenarios_common::{ScenarioFixture, board_ops};

#[test]
fn multi_engineer_three_engineers_ten_tasks_walks_cleanly() {
    let mut builder = ScenarioFixture::builder()
        .with_manager("manager")
        .with_engineers(3);

    for id in 1..=10u32 {
        builder = builder.with_task(id, format!("task #{id}"), "todo", None);
    }

    let mut fixture = builder.build();
    board_ops::init_git_repo(fixture.project_root());

    let reports = fixture.tick_n(5);
    for (i, report) in reports.iter().enumerate() {
        assert!(
            report.subsystem_errors.is_empty(),
            "multi-engineer tick #{i} should run cleanly, got {:?}",
            report.subsystem_errors
        );
    }
    fixture.assert_state_consistent();
}
