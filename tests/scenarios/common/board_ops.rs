//! Board/git helpers shared across scenarios.
//!
//! Keep functions here small and orthogonal — scenarios reach for
//! `board_ops::init_git_repo(dir)` instead of re-implementing git
//! bootstrap in every test file.

use std::path::Path;
use std::process::Command;

/// Bootstrap a minimal git repo in `dir` so FakeShim's
/// `CompleteWith { files_touched }` behavior has somewhere to commit.
/// Uses `GIT_CONFIG_GLOBAL=/dev/null` to avoid picking up the
/// developer's local git identity.
pub fn init_git_repo(dir: &Path) {
    let env = [
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ];
    let run = |args: &[&str]| {
        let mut cmd = Command::new("git");
        cmd.current_dir(dir).args(args);
        for (k, v) in &env {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("git spawn");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(&["init", "-b", "main"]);
    run(&["config", "user.email", "scenario@batty.test"]);
    run(&["config", "user.name", "Scenario"]);
    std::fs::write(dir.join("README.md"), "seed\n").unwrap();
    run(&["add", "README.md"]);
    run(&["commit", "-m", "seed"]);
}
