//! Happy-path scenario — the canary that proves the harness plumbing
//! works end-to-end (ticket #640).
//!
//! Phase 1 scope: prove the shim round-trip. A fake shim is inserted,
//! the daemon sends a `SendMessage` command through the socketpair,
//! the fake consumes it and responds with a scripted
//! `StateChanged → MessageDelivered → StateChanged → Completion`
//! sequence (committing a file along the way), and the next
//! `daemon.tick()` drains the events back through `poll_shim_handles`.
//!
//! Full `dispatch → verify → merge → main advances` requires engineer
//! worktrees + base branches that phase 1 does not set up; that cycle
//! lands in tickets #641/#642. What #640 must prove is that every
//! piece below the merge pipeline is wired correctly — if this test
//! fails, the harness has a defect and every later scenario cascades.

use batty_cli::shim::fake::ShimBehavior;
use batty_cli::shim::protocol::{Command, Event, ShimState};
use batty_cli::team::standup::MemberState;
use std::path::PathBuf;

use super::super::scenarios_common::{ScenarioFixture, board_ops};

#[test]
fn happy_path_dispatch_round_trips_through_fake_shim() {
    let mut fixture = ScenarioFixture::builder()
        .with_manager("manager")
        .with_engineers(1)
        .with_task(1, "wire the fake shim", "todo", None)
        .build();

    board_ops::init_git_repo(fixture.project_root());
    fixture.insert_fake_shim("eng-1");
    fixture.shim("eng-1").queue(ShimBehavior::CompleteWith {
        response: "implemented src/lib.rs".to_string(),
        files_touched: vec![(
            PathBuf::from("src/lib.rs"),
            "// happy path canary\npub fn ok() -> bool { true }\n".to_string(),
        )],
    });

    fixture.send_to_shim("eng-1", "manager", "implement src/lib.rs");
    let events = fixture.process_shim("eng-1");
    let report = fixture.tick();

    // Plumbing: exactly one SendMessage reached the fake.
    let handled = fixture.shim("eng-1").handled_commands();
    assert_eq!(handled.len(), 1, "expected 1 command, got {:?}", handled);
    assert!(
        matches!(
            handled[0],
            Command::SendMessage { ref from, ref body, .. }
                if from == "manager" && body == "implement src/lib.rs"
        ),
        "unexpected command: {:?}",
        handled[0]
    );

    // Fake emitted the scripted Completion.
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::Completion { response, .. } if response == "implemented src/lib.rs"
        )),
        "expected Completion event, got {:?}",
        events
    );

    // File was actually committed to the worktree.
    assert!(
        fixture.project_root().join("src/lib.rs").exists(),
        "fake CompleteWith should commit the promised file"
    );

    // Daemon drained the events and transitioned to Idle.
    assert_eq!(
        fixture
            .daemon_mut()
            .scenario_hooks()
            .inspect_shim_state("eng-1"),
        Some(ShimState::Idle),
    );
    assert_eq!(
        fixture.daemon_mut().scenario_hooks().member_state("eng-1"),
        Some(MemberState::Idle),
    );

    // Tick recorded no subsystem errors.
    assert!(
        report.subsystem_errors.is_empty(),
        "tick draining completion should have no errors, got {:?}",
        report.subsystem_errors
    );
}
