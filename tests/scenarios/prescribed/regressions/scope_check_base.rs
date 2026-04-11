//! Regression for 0.10.1: scope-check diff base must use
//! `git merge-base(main, HEAD)` instead of `main..HEAD`. On a stale
//! branch the old base-resolution logic interpreted commits on main
//! that diverged AFTER the branch point as "protected file edits by
//! the engineer," rejecting every completion.
//!
//! Test: create a git repo, branch, advance main with a new commit to
//! `planning/foo.md` (a protected path), then run the merge-base
//! resolution used by the scope check. Assert the resolved base is the
//! actual branch point (not the current main tip) so any subsequent
//! diff sees only the engineer's real edits.

use std::process::Command;

use super::super::super::scenarios_common::{ScenarioFixture, board_ops};

#[test]
fn scope_check_base_uses_merge_base_not_main_head() {
    let fixture = ScenarioFixture::builder().with_engineers(1).build();
    let repo = fixture.project_root();
    board_ops::init_git_repo(repo);

    // Branch from current main.
    git(repo, &["checkout", "-b", "eng-1/42"]);
    // Engineer commits a non-protected file on their branch.
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "// eng work\n").unwrap();
    git(repo, &["add", "src/lib.rs"]);
    git(repo, &["commit", "-m", "eng: real work"]);
    let branch_tip = git_rev_parse(repo, "HEAD");

    // Meanwhile, main advances with a commit to a protected path.
    git(repo, &["checkout", "main"]);
    std::fs::create_dir_all(repo.join("planning")).unwrap();
    std::fs::write(repo.join("planning/foo.md"), "# planning\n").unwrap();
    git(repo, &["add", "planning/foo.md"]);
    git(repo, &["commit", "-m", "planning: unrelated doc"]);
    let main_tip = git_rev_parse(repo, "HEAD");

    // Switch back to the engineer branch for the scope check.
    git(repo, &["checkout", "eng-1/42"]);

    // --- Act ---
    // The 0.10.1 fix was to use `git merge-base main HEAD` instead of
    // the main tip. Resolve both and verify they are DIFFERENT on this
    // diverged setup (so the bug is actually reachable).
    let merge_base_direct = git_output(repo, &["merge-base", "main", "HEAD"]);

    // --- Assert ---
    // Merge-base resolves to the branch point (i.e. the commit main
    // was at before the planning/foo.md commit), not the current main.
    assert_ne!(
        merge_base_direct.trim(),
        main_tip,
        "merge-base should be the branch point, not the current main tip"
    );
    // And merge-base should be a proper ancestor of both branch_tip
    // and main_tip — `git merge-base --is-ancestor` returns 0.
    let is_ancestor_of_main = Command::new("git")
        .current_dir(repo)
        .args([
            "merge-base",
            "--is-ancestor",
            merge_base_direct.trim(),
            "main",
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .unwrap()
        .success();
    assert!(
        is_ancestor_of_main,
        "merge-base should be an ancestor of main"
    );

    // Diff from merge-base sees ONLY the engineer's file, not
    // planning/foo.md. This is what the scope check validates.
    let changed = git_output(
        repo,
        &[
            "diff",
            "--name-only",
            &format!("{}..{}", merge_base_direct.trim(), branch_tip),
        ],
    );
    let changed_files: Vec<&str> = changed.trim().lines().collect();
    assert!(
        changed_files.contains(&"src/lib.rs"),
        "engineer's change should be in the diff: {changed_files:?}"
    );
    assert!(
        !changed_files.contains(&"planning/foo.md"),
        "protected main-branch change must NOT appear in branch diff: {changed_files:?}"
    );
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .unwrap();
    assert!(status.success(), "git {:?} failed", args);
}

fn git_rev_parse(dir: &std::path::Path, rev: &str) -> String {
    let out = git_output(dir, &["rev-parse", rev]);
    out.trim().to_string()
}

fn git_output(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {:?} failed", args);
    String::from_utf8_lossy(&out.stdout).to_string()
}
