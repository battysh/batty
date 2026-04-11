//! Disk pressure: grow a fake shared-target tree large enough that
//! the size-based cleanup tier would fire before the disk-based
//! tier. The hygiene path must handle it without errors.
//!
//! Phase 1 scope: we don't actually fill 13GB — we create a
//! synthetic `deps/` directory inside shared-target that would
//! match the size-based cleanup's eligibility rules and assert the
//! hygiene tick runs cleanly.

use super::super::scenarios_common::ScenarioFixture;

#[test]
fn disk_pressure_size_tier_hygiene_runs_cleanly_with_deps_dir() {
    let mut fixture = ScenarioFixture::builder()
        .with_manager("manager")
        .with_engineers(1)
        .build();

    let shared_target = fixture.project_root().join(".batty").join("shared-target");
    std::fs::create_dir_all(shared_target.join("eng-1").join("debug").join("deps")).unwrap();
    std::fs::write(
        shared_target
            .join("eng-1")
            .join("debug")
            .join("deps")
            .join("a.rlib"),
        vec![0u8; 4096],
    )
    .unwrap();
    std::fs::write(shared_target.join("eng-1").join("marker.txt"), "keep\n").unwrap();

    let report = fixture.tick();
    let hygiene_errors: Vec<&(String, String)> = report
        .subsystem_errors
        .iter()
        .filter(|(step, _)| step.contains("disk_hygiene") || step.contains("shared_cargo_target"))
        .collect();
    assert!(
        hygiene_errors.is_empty(),
        "size-tier disk hygiene should run cleanly on deps tree, got {:?}",
        hygiene_errors
    );
    assert!(
        shared_target.join("eng-1").exists(),
        "engineer shared-target root should survive clean tick"
    );
    fixture.assert_state_consistent();
}
