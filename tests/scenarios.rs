//! End-to-end scenario framework test target.
//!
//! This binary hosts the [`ScenarioFixture`]-driven tests that drive the
//! real `TeamDaemon` against in-process [`FakeShim`]s. Each scenario runs
//! in its own `tempdir`, has its own git repo, its own board, its own
//! daemon, and its own fake shims — fully parallelizable, zero subprocess
//! spawns, zero tmux.
//!
//! Phase 1 (ticket #639) ships only the harness + a smoke test. Individual
//! scenarios land in later tickets (#640–#642).
//!
//! Run with: `cargo test --test scenarios --features scenario-test`.

#![cfg(feature = "scenario-test")]

// Use an explicit path so cargo's default module lookup does not resolve
// `mod common` to the existing `tests/common/mod.rs` (which is owned by
// the legacy integration_harness target).
#[path = "scenarios/common/mod.rs"]
mod scenarios_common;

#[path = "scenarios/prescribed/mod.rs"]
mod prescribed;

use scenarios_common::ScenarioFixture;

#[test]
fn fixture_builds_3_engineer_team_and_ticks_cleanly() {
    let mut fixture = ScenarioFixture::builder().with_engineers(3).build();

    let reports = fixture.tick_n(5);

    assert_eq!(reports.len(), 5);
    for (i, report) in reports.iter().enumerate() {
        assert_eq!(
            report.cycle,
            (i + 1) as u64,
            "cycle counter should advance monotonically across ticks"
        );
        assert!(
            report.subsystem_errors.is_empty(),
            "tick {i} should have no subsystem errors, got {:?}",
            report.subsystem_errors
        );
    }
}

#[test]
fn fixture_tick_until_stops_at_matching_predicate() {
    let mut fixture = ScenarioFixture::builder().with_engineers(1).build();

    let report = fixture
        .tick_until(|report| report.cycle == 3)
        .expect("predicate should fire within budget");

    assert_eq!(report.cycle, 3);
}

#[test]
fn fixture_with_manager_and_tasks_builds_consistent_board() {
    let fixture = ScenarioFixture::builder()
        .with_manager("manager")
        .with_engineers(2)
        .with_task(1, "first task", "todo", None)
        .with_task(2, "second task", "todo", None)
        .build();

    let tasks = fixture.task_ids();
    assert_eq!(tasks, vec![1, 2]);
}
