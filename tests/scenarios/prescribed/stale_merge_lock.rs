//! Stale merge lock: pre-place a `.batty/merge.lock` file before
//! starting a tick. The daemon must not hang or panic on startup.
//!
//! Note: if this scenario reveals that the daemon does NOT clean up
//! stale locks at startup, file a follow-up task to add that
//! cleanup. Phase 1 scope: the tick must at minimum not panic, and
//! the merge queue must observe the lock as a signal rather than
//! crashing.

use super::super::scenarios_common::{ScenarioFixture, board_ops};

#[test]
fn stale_merge_lock_daemon_tick_is_resilient_to_pre_existing_lock() {
    let mut fixture = ScenarioFixture::builder()
        .with_manager("manager")
        .with_engineers(1)
        .build();
    board_ops::init_git_repo(fixture.project_root());

    // Pre-place a stale merge lock file with a synthetic PID that
    // definitely doesn't exist.
    let batty_dir = fixture.project_root().join(".batty");
    std::fs::create_dir_all(&batty_dir).unwrap();
    std::fs::write(batty_dir.join("merge.lock"), "pid=999999\n").unwrap();

    let report = fixture.tick();

    // The tick must run to completion — no infinite loop, no panic.
    assert!(report.cycle >= 1);
    // Merge-queue subsystem must not panic (it may log an error if it
    // cannot acquire the lock, which is expected behavior).
    fixture.assert_state_consistent();
}
