//! Scope-fence violations: the fake shim commits to `planning/` or
//! `docs/` (protected paths). The scope-check subsystem must observe
//! this and refuse the completion.
//!
//! Phase 1 scope: verify the fake can commit to a protected path
//! without the daemon's scope check crashing. Full auto-revert lands
//! once the completion → scope-check → merge pipeline is wired.

use std::path::PathBuf;

use batty_cli::shim::fake::ShimBehavior;

use super::super::scenarios_common::{ScenarioFixture, board_ops};

#[test]
fn scope_fence_violations_fake_commits_to_protected_path() {
    let mut fixture = ScenarioFixture::builder()
        .with_manager("manager")
        .with_engineers(1)
        .build();
    board_ops::init_git_repo(fixture.project_root());
    fixture.insert_fake_shim("eng-1");

    // Fake commits to planning/ — a protected path. The scope check
    // should not crash on this.
    fixture.shim("eng-1").queue(ShimBehavior::CompleteWith {
        response: "wrote planning doc".to_string(),
        files_touched: vec![(
            PathBuf::from("planning/scope_violation.md"),
            "# not allowed\n".to_string(),
        )],
    });

    fixture.send_to_shim("eng-1", "manager", "document the plan");
    let _ = fixture.process_shim("eng-1");
    let report = fixture.tick();

    // The file was actually written by the fake.
    assert!(
        fixture
            .project_root()
            .join("planning/scope_violation.md")
            .exists(),
        "fake should have written the protected-path file"
    );

    // Tick ran cleanly through all subsystems including scope check.
    assert!(
        report.subsystem_errors.is_empty(),
        "scope-check path should not panic on protected-file commit, got {:?}",
        report.subsystem_errors
    );
    fixture.assert_state_consistent();
}
