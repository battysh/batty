//! Regression for 0.10.4 / 0.10.6: the disk hygiene cascade must run
//! cleanly when shared-target is non-empty and the project is under
//! pressure. Before the fix, the cleanup tiers panicked or left
//! engineer directories in an inconsistent state.
//!
//! Phase 1 scope: this scenario creates a synthetic shared-target
//! tree, drives `maybe_run_disk_hygiene`, and asserts the path did
//! not log subsystem errors and did not delete the legitimate
//! engineer directory. Full disk-pressure emergency cascade (sparse
//! files, 3x budget) requires infrastructure that lands in ticket
//! #642's `disk_pressure` cross-feature scenario.

use super::super::super::scenarios_common::ScenarioFixture;

#[test]
fn disk_emergency_hygiene_tick_runs_cleanly_with_populated_shared_target() {
    let mut fixture = ScenarioFixture::builder()
        .with_manager("manager")
        .with_engineers(1)
        .build();

    // Create a synthetic shared-target tree so the hygiene path has
    // something to inspect. The engineer directory under it should
    // survive; deps/build-style dirs are candidates for cleanup.
    let shared_target = fixture.project_root().join(".batty").join("shared-target");
    std::fs::create_dir_all(shared_target.join("eng-1").join("debug")).unwrap();
    std::fs::write(shared_target.join("eng-1").join("keep.txt"), "keep\n").unwrap();

    let report = fixture.tick();

    // The hygiene subsystem should run without errors on a synthetic
    // shared-target tree.
    let relevant_errors: Vec<&(String, String)> = report
        .subsystem_errors
        .iter()
        .filter(|(step, _)| {
            step.contains("disk_hygiene")
                || step.contains("shared_cargo_target")
                || step.contains("cleanup")
        })
        .collect();
    assert!(
        relevant_errors.is_empty(),
        "disk hygiene path should run cleanly, got errors: {:?}",
        relevant_errors
    );

    // The engineer's shared-target directory should still exist —
    // the cleanup should never blow away the whole tree on a single
    // tick without pressure.
    assert!(
        shared_target.join("eng-1").exists(),
        "engineer shared-target directory should survive a clean tick"
    );
}
