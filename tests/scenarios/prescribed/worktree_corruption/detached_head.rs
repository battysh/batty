//! Worktree corruption: force a detached HEAD inside the engineer's
//! repo. The daemon's worktree-staleness check must observe this and
//! not panic.
//!
//! Phase 1 scope: initialize a real git repo, detach HEAD, drive a
//! tick, assert no subsystem errors. Full recovery (reattaching the
//! engineer to the correct branch) requires engineer worktree
//! bootstrap.

use std::process::Command;

use super::super::super::scenarios_common::{ScenarioFixture, board_ops};

#[test]
fn worktree_corruption_detached_head_tick_runs_cleanly() {
    let mut fixture = ScenarioFixture::builder()
        .with_manager("manager")
        .with_engineers(1)
        .build();
    let repo = fixture.project_root().to_path_buf();
    board_ops::init_git_repo(&repo);

    // Write a second commit so we can detach to a non-HEAD sha.
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    run_git(&repo, &["add", "a.txt"]);
    run_git(&repo, &["commit", "-m", "second"]);
    let first_commit = git_output(&repo, &["rev-parse", "HEAD~1"]);
    run_git(&repo, &["checkout", first_commit.trim()]);

    // Confirm HEAD is detached.
    let head_ref = git_output(&repo, &["symbolic-ref", "--quiet", "HEAD"]);
    assert!(
        head_ref.is_empty(),
        "HEAD should be detached, symbolic-ref returned: {head_ref:?}"
    );

    let report = fixture.tick();
    assert!(
        report.subsystem_errors.is_empty()
            || report
                .subsystem_errors
                .iter()
                .all(|(step, _)| !step.contains("worktree")),
        "detached-HEAD tick should not emit worktree errors, got {:?}",
        report.subsystem_errors
    );

    fixture.assert_state_consistent();
}

fn run_git(dir: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .unwrap();
    assert!(status.success(), "git {:?} failed", args);
}

fn git_output(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).to_string()
}
