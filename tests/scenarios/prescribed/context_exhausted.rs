//! Context exhausted: the fake shim emits a `ContextExhausted` event
//! during a command. The daemon must mark the shim state accordingly
//! and the respawn path must not panic.

use batty_cli::shim::fake::ShimBehavior;
use batty_cli::shim::protocol::{Event, ShimState};

use super::super::scenarios_common::{ScenarioFixture, board_ops};

#[test]
fn context_exhausted_fake_event_is_observed_by_daemon() {
    let mut fixture = ScenarioFixture::builder()
        .with_manager("manager")
        .with_engineers(1)
        .build();
    board_ops::init_git_repo(fixture.project_root());
    fixture.insert_fake_shim("eng-1");

    fixture.shim("eng-1").queue(ShimBehavior::ContextExhausted {
        message: "out of context".to_string(),
    });

    fixture.send_to_shim("eng-1", "manager", "do something big");
    let events = fixture.process_shim("eng-1");

    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::ContextExhausted { .. })),
        "fake should emit ContextExhausted event, got {:?}",
        events
    );

    // The fake's own state tracks ContextExhausted after handling.
    assert_eq!(fixture.shim("eng-1").state(), ShimState::ContextExhausted);

    // Daemon tick drains the event without panicking.
    let report = fixture.tick();
    assert!(
        report.subsystem_errors.is_empty(),
        "context-exhausted handling should not error, got {:?}",
        report.subsystem_errors
    );
    fixture.assert_state_consistent();
}
