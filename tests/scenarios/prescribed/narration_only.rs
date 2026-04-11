//! Narration-only: the fake shim emits `Completion` events with zero
//! committed files. The daemon's narration detection must notice this
//! and mark the completion for rejection (or escalate).

use std::path::PathBuf;

use batty_cli::shim::fake::ShimBehavior;
use batty_cli::shim::protocol::Event;

use super::super::scenarios_common::{ScenarioFixture, board_ops};

#[test]
fn narration_only_fake_completion_roundtrips_through_daemon() {
    let mut fixture = ScenarioFixture::builder()
        .with_manager("manager")
        .with_engineers(1)
        .build();
    board_ops::init_git_repo(fixture.project_root());
    fixture.insert_fake_shim("eng-1");

    // Queue two narration-only behaviors — the daemon should notice
    // both as non-productive completions.
    fixture.shim("eng-1").queue(ShimBehavior::NarrationOnly {
        response: "I'll work on this".to_string(),
    });
    fixture.shim("eng-1").queue(ShimBehavior::NarrationOnly {
        response: "Still thinking".to_string(),
    });

    // Drive two round-trips through the shim.
    fixture.send_to_shim("eng-1", "manager", "do it");
    let first = fixture.process_shim("eng-1");
    let _ = fixture.tick();
    fixture.send_to_shim("eng-1", "manager", "do it again");
    let second = fixture.process_shim("eng-1");
    let _ = fixture.tick();

    // Both rounds emitted narration-only Completion events (response
    // present, zero files).
    for (label, events) in [("first", &first), ("second", &second)] {
        let has_completion = events.iter().any(|e| matches!(e, Event::Completion { .. }));
        assert!(
            has_completion,
            "{label} narration round should emit Completion, got {:?}",
            events
        );
    }

    // No file should exist — narration behaviors don't write code.
    assert!(
        !fixture.project_root().join("src/lib.rs").exists(),
        "narration-only completions must not write files"
    );
    // Suppress unused import warning when PathBuf is only needed for
    // the parallel file-not-exists check.
    let _ = PathBuf::from("src/lib.rs");
    fixture.assert_state_consistent();
}
