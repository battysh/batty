//! Worktree corruption: truncate the git index file. The daemon's
//! worktree/git subsystems must not panic when encountering a broken
//! index.
//!
//! Phase 1 scope: bootstrap a git repo, truncate `.git/index`, drive
//! a tick, assert the daemon logs the failure as a subsystem error
//! (or handles it cleanly) without crashing.

use super::super::super::scenarios_common::{ScenarioFixture, board_ops};

#[test]
fn worktree_corruption_broken_index_tick_does_not_panic() {
    let mut fixture = ScenarioFixture::builder()
        .with_manager("manager")
        .with_engineers(1)
        .build();
    let repo = fixture.project_root();
    board_ops::init_git_repo(repo);

    // Truncate the git index so any subsequent `git status`-style
    // operation has to deal with a broken index.
    let index = repo.join(".git").join("index");
    if index.exists() {
        std::fs::write(&index, b"").unwrap();
    }

    // The tick must run to completion. Subsystem errors are allowed
    // (broken index is a real failure) but the daemon must not panic.
    let report = fixture.tick();

    // Sanity: the tick did advance the cycle counter.
    assert!(report.cycle >= 1);
    fixture.assert_state_consistent();
}
