//! Silent death: the fake shim emits no events for a long simulated
//! duration. The daemon's health-check + working-state timeout must
//! observe this and not panic.

use std::time::Duration;

use batty_cli::shim::fake::ShimBehavior;

use super::super::scenarios_common::{ScenarioFixture, board_ops};

#[test]
fn silent_death_handle_backdated_past_timeout_does_not_panic() {
    let mut fixture = ScenarioFixture::builder()
        .with_manager("manager")
        .with_engineers(1)
        .build();
    board_ops::init_git_repo(fixture.project_root());
    fixture.insert_fake_shim("eng-1");

    // Queue Silent so any command is swallowed without a response.
    fixture.shim("eng-1").queue(ShimBehavior::Silent);
    fixture.send_to_shim("eng-1", "manager", "work");
    let events = fixture.process_shim("eng-1");
    assert!(
        events.is_empty(),
        "Silent behavior should emit no events, got {:?}",
        events
    );

    // Backdate the shim's state-changed timestamp so the working-
    // state timeout fires on the next tick. This simulates the
    // "2h of silence" case without waiting in real time.
    fixture
        .daemon_mut()
        .scenario_hooks()
        .backdate_shim_state_change("eng-1", Duration::from_secs(7200));
    fixture
        .daemon_mut()
        .scenario_hooks()
        .backdate_shim_last_activity("eng-1", Duration::from_secs(7200));
    fixture
        .daemon_mut()
        .scenario_hooks()
        .backdate_last_shim_health_check(Duration::from_secs(120));

    let report = fixture.tick();
    assert!(
        report.subsystem_errors.is_empty(),
        "silent death tick should handle timeouts cleanly, got {:?}",
        report.subsystem_errors
    );
    fixture.assert_state_consistent();
}
