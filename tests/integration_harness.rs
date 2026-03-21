mod common;

use std::collections::HashMap;

use batty_cli::task;

use common::{InboxMessage, MemberState, TestHarness, engineer_member, manager_member};

#[test]
fn harness_builds_daemon_with_mock_member_availability_without_tmux() {
    let harness = TestHarness::new()
        .with_members(vec![
            manager_member("lead", None),
            engineer_member("eng-1", Some("lead"), false),
        ])
        .with_availability(HashMap::from([
            ("lead".to_string(), MemberState::Idle),
            ("eng-1".to_string(), MemberState::Working),
        ]))
        .with_board_task(1, "todo-task", "todo", None)
        .with_board_task(2, "claimed-task", "in-progress", Some("eng-1"));

    let daemon = harness.build_daemon().unwrap();
    let tasks = task::load_tasks_from_dir(&harness.board_tasks_dir()).unwrap();

    assert_eq!(harness.daemon_member_count(&daemon), 2);
    assert_eq!(
        harness.daemon_state(&daemon, "lead"),
        Some(MemberState::Idle)
    );
    assert_eq!(
        harness.daemon_state(&daemon, "eng-1"),
        Some(MemberState::Working)
    );
    assert_eq!(tasks.len(), 2);
    assert!(tasks.iter().any(|task| task.status == "todo"));
    assert!(
        tasks
            .iter()
            .any(|task| task.claimed_by.as_deref() == Some("eng-1"))
    );
}

#[test]
fn harness_sets_up_pending_and_delivered_inbox_states() {
    let harness = TestHarness::new()
        .with_member(manager_member("lead", None))
        .with_inbox_message(
            "lead",
            InboxMessage::new_send("eng-1", "lead", "pending"),
            false,
        )
        .with_inbox_message(
            "lead",
            InboxMessage::new_send("eng-2", "lead", "delivered"),
            true,
        );

    let pending = harness.pending_inbox_messages("lead").unwrap();

    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].body, "pending");
    assert_eq!(pending[0].from, "eng-1");
}
