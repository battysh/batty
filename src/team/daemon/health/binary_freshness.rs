//! Binary-vs-HEAD freshness detection (#675).
//!
//! During a quota outage on 2026-04-15, four critical fixes landed on
//! `main` while the running daemon was executing a 5+ hour old binary.
//! The fixes didn't take effect until the binary was rebuilt and the
//! daemon restarted — there was no automated signal surfacing the gap.
//!
//! This module computes whether the running daemon binary is stale
//! relative to the git HEAD of the batty source tree, filtered by
//! commits that actually touched `src/**` (docs-only commits don't make
//! the binary stale).
//!
//! The goal is *detection and loud surfacing*, not auto-restart. The
//! operator decides when to cycle the daemon.
//!
//! Public surface:
//! - [`BinaryFreshness`] — the result type rendered by status.
//! - [`evaluate_binary_freshness`] — pure entry point: takes binary path
//!   and repo root, returns freshness report.
//! - [`DEFAULT_STALE_THRESHOLD_SECS`] — 10 minute tolerance.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};

use crate::team::merge::inspect_root_dirty_state;

/// Commits newer than the binary by less than this window are treated
/// as "fresh enough" — avoids false alarms for a just-rebuilt binary
/// racing a freshly-pushed commit.
pub const DEFAULT_STALE_THRESHOLD_SECS: i64 = 600; // 10 minutes
pub const STALE_RECOVERY_COMMAND: &str = "batty daemon-restart-if-stale";
pub const STALE_RECOVERY_DRY_RUN_COMMAND: &str = "batty daemon-restart-if-stale --dry-run";
pub const STALE_MANUAL_RECOVERY_COMMAND: &str = "cargo build --release && cp target/release/batty ~/.cargo/bin/batty && codesign --force --sign - ~/.cargo/bin/batty && batty stop && batty start";

/// Result of a binary-vs-HEAD freshness check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryFreshness {
    /// True when the binary is considered up-to-date.
    pub fresh: bool,
    /// Number of commits on main that touched `src/**` and landed after
    /// the binary was built. Zero when fresh.
    pub commits_behind: u32,
    /// Subject line of the most recent commit that would flip the binary
    /// to stale. Empty when fresh.
    pub last_subject: String,
    /// Hash (short) of the last stale-triggering commit. Empty when fresh.
    pub last_hash: String,
    /// Unix timestamp of the binary's mtime.
    pub binary_mtime: i64,
    /// Unix timestamp of the HEAD commit (author date).
    pub head_ts: i64,
    /// Whether the source worktree has uncommitted non-runtime changes.
    /// Batty-managed runtime noise is safe for daemon-owned recovery.
    pub worktree_dirty: bool,
}

impl BinaryFreshness {
    fn fresh_with_stamps(binary_mtime: i64, head_ts: i64) -> Self {
        Self {
            fresh: true,
            commits_behind: 0,
            last_subject: String::new(),
            last_hash: String::new(),
            binary_mtime,
            head_ts,
            worktree_dirty: false,
        }
    }

    pub fn recovery_action(&self) -> String {
        if self.worktree_dirty {
            format!(
                "auto-restart refused: source worktree has uncommitted changes; next: inspect `git status --short`, commit/stash/clear the source edits, then run `{}`; manual fallback: `{}`",
                STALE_RECOVERY_COMMAND, STALE_MANUAL_RECOVERY_COMMAND
            )
        } else {
            format!(
                "next: run `{}` to inspect, then `{}`",
                STALE_RECOVERY_DRY_RUN_COMMAND, STALE_RECOVERY_COMMAND
            )
        }
    }

    /// Formatted one-line message suitable for the status output.
    pub fn status_line(&self) -> String {
        if self.fresh {
            "Daemon Binary: fresh".to_string()
        } else if self.commits_behind == 1 {
            format!(
                "Daemon Binary: STALE — 1 commit behind main (last: {}); {}",
                self.last_subject,
                self.recovery_action()
            )
        } else {
            format!(
                "Daemon Binary: STALE — {} commits behind main (last: {}); {}",
                self.commits_behind,
                self.last_subject,
                self.recovery_action()
            )
        }
    }
}

/// Compute binary freshness against the given repo root.
///
/// Returns Ok(freshness) on success; returns Ok(None) when binary path
/// does not exist or repo is not a git worktree — those cases should
/// not fail-close the daemon. Hard errors bubble up from
/// [`anyhow::Context`].
pub fn evaluate_binary_freshness(
    binary_path: &Path,
    repo_root: &Path,
) -> Result<Option<BinaryFreshness>> {
    let Some(binary_mtime) = binary_mtime_unix(binary_path)? else {
        return Ok(None);
    };
    let Some(head_ts) = head_commit_ts(repo_root)? else {
        return Ok(None);
    };
    Ok(Some(evaluate_with_stamps(
        repo_root,
        binary_mtime,
        head_ts,
        DEFAULT_STALE_THRESHOLD_SECS,
    )?))
}

/// Test-friendly variant that accepts pre-computed timestamps and
/// a configurable stale threshold.
pub fn evaluate_with_stamps(
    repo_root: &Path,
    binary_mtime: i64,
    head_ts: i64,
    stale_threshold_secs: i64,
) -> Result<BinaryFreshness> {
    // HEAD older than or very close to the binary — nothing to flag.
    if head_ts <= binary_mtime + stale_threshold_secs {
        return Ok(BinaryFreshness::fresh_with_stamps(binary_mtime, head_ts));
    }

    // HEAD is meaningfully newer. Count commits that touched src/** and
    // whose author timestamp is newer than the binary.
    let (commits_behind, last_subject, last_hash) =
        commits_touching_src_since(repo_root, binary_mtime)?;

    if commits_behind == 0 {
        // HEAD moved forward but only docs/** or similar — binary still
        // fresh from a runtime-behavior perspective.
        return Ok(BinaryFreshness::fresh_with_stamps(binary_mtime, head_ts));
    }
    let root_dirty = inspect_root_dirty_state(repo_root)?;
    let worktree_dirty = !root_dirty.source_paths.is_empty();

    Ok(BinaryFreshness {
        fresh: false,
        commits_behind,
        last_subject,
        last_hash,
        binary_mtime,
        head_ts,
        worktree_dirty,
    })
}

fn binary_mtime_unix(binary_path: &Path) -> Result<Option<i64>> {
    let metadata = match fs::metadata(binary_path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to stat daemon binary {}", binary_path.display())
            });
        }
    };
    let modified = metadata
        .modified()
        .with_context(|| format!("mtime unavailable for {}", binary_path.display()))?;
    let secs = modified
        .duration_since(UNIX_EPOCH)
        .context("binary mtime before unix epoch")?
        .as_secs() as i64;
    Ok(Some(secs))
}

fn head_commit_ts(repo_root: &Path) -> Result<Option<i64>> {
    let output = Command::new("git")
        .args(["log", "-1", "--format=%ct", "HEAD"])
        .current_dir(repo_root)
        .output()
        .with_context(|| format!("failed to invoke git in {}", repo_root.display()))?;
    if !output.status.success() {
        // Not a git worktree, or git unavailable — skip the check.
        return Ok(None);
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let ts: i64 = trimmed
        .parse()
        .with_context(|| format!("unparseable git HEAD timestamp: {trimmed:?}"))?;
    Ok(Some(ts))
}

/// Enumerate commits touching `src/**` whose committer timestamp is
/// strictly greater than `since_ts`. Returns (count, last_subject,
/// last_hash) — "last" meaning the newest commit, which is what the
/// operator most likely wants to see.
fn commits_touching_src_since(repo_root: &Path, since_ts: i64) -> Result<(u32, String, String)> {
    // --format=%ct\t%h\t%s — tab-separated commit time, short hash,
    // subject. --no-merges keeps the count focused on source changes
    // rather than noise from merge commits.
    let output = Command::new("git")
        .args([
            "log",
            "HEAD",
            "--no-merges",
            "--format=%ct%x09%h%x09%s",
            "--",
            "src",
        ])
        .current_dir(repo_root)
        .output()
        .with_context(|| format!("failed to invoke git log in {}", repo_root.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(
            "git log failed while checking src/** commits in {}: {}",
            repo_root.display(),
            stderr
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut count: u32 = 0;
    let mut newest: Option<(i64, String, String)> = None;

    for line in stdout.lines() {
        let mut parts = line.splitn(3, '\t');
        let Some(ts_s) = parts.next() else { continue };
        let Some(hash) = parts.next() else { continue };
        let subject = parts.next().unwrap_or("").to_string();
        let ts: i64 = match ts_s.parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        if ts <= since_ts {
            // git log is in newest-first order; once we cross the binary
            // mtime we can stop iterating.
            break;
        }
        count += 1;
        if newest
            .as_ref()
            .map(|(existing_ts, ..)| ts > *existing_ts)
            .unwrap_or(true)
        {
            newest = Some((ts, hash.to_string(), subject));
        }
    }

    let (_, hash, subject) = newest.unwrap_or_default();
    Ok((count, subject, hash))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    fn init_repo(dir: &Path) {
        run_git(dir, &["init", "-q", "-b", "main"]);
        run_git(dir, &["config", "user.email", "test@example.com"]);
        run_git(dir, &["config", "user.name", "Test"]);
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git binary");
        assert!(status.success(), "git {args:?} failed in {dir:?}");
    }

    fn commit_file_with_time(dir: &Path, rel: &str, content: &str, unix_ts: i64) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
        run_git(dir, &["add", rel]);
        let date = format!("{unix_ts} +0000");
        let status = Command::new("git")
            .args(["commit", "-q", "-m", &format!("commit {rel}")])
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .current_dir(dir)
            .status()
            .expect("git commit");
        assert!(status.success());
    }

    #[test]
    fn fresh_when_head_within_threshold_of_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        init_repo(repo);
        commit_file_with_time(repo, "src/foo.rs", "fn a() {}", 1_700_000_000);

        let report = evaluate_with_stamps(repo, 1_700_000_000, 1_700_000_300, 600).unwrap();
        assert!(report.fresh, "delta 300s <= 600s threshold should be fresh");
        assert_eq!(report.commits_behind, 0);
        assert_eq!(report.status_line(), "Daemon Binary: fresh");
    }

    #[test]
    fn stale_when_src_commit_newer_than_binary_by_more_than_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        init_repo(repo);
        commit_file_with_time(repo, "src/foo.rs", "fn a() {}", 1_700_000_000);
        commit_file_with_time(repo, "src/bar.rs", "fn b() {}", 1_700_001_000);

        // Binary is 1800s older than HEAD; threshold 600s; one src commit
        // happened after the binary was built.
        let report = evaluate_with_stamps(repo, 1_700_000_000, 1_700_001_800, 600).unwrap();
        assert!(!report.fresh);
        assert_eq!(report.commits_behind, 1);
        assert!(!report.worktree_dirty);
        assert!(report.last_subject.contains("src/bar.rs"));
        assert!(
            report.status_line().contains("STALE"),
            "expected STALE in status line, got {:?}",
            report.status_line()
        );
        assert!(
            report.status_line().contains(STALE_RECOVERY_COMMAND),
            "stale status should include the recovery command, got {:?}",
            report.status_line()
        );
    }

    #[test]
    fn docs_only_commit_does_not_flip_binary_to_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        init_repo(repo);
        commit_file_with_time(repo, "src/foo.rs", "fn a() {}", 1_700_000_000);
        commit_file_with_time(repo, "docs/changelog.md", "# Changelog\n", 1_700_001_000);

        // HEAD ts is 1800s newer than binary, but the commit only touched
        // docs/**, so src-path filtering should report fresh.
        let report = evaluate_with_stamps(repo, 1_700_000_000, 1_700_001_800, 600).unwrap();
        assert!(
            report.fresh,
            "docs-only commit should not mark binary stale"
        );
        assert_eq!(report.commits_behind, 0);
    }

    #[test]
    fn counts_multiple_src_commits_since_binary_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        init_repo(repo);
        commit_file_with_time(repo, "src/foo.rs", "fn a() {}", 1_700_000_000);
        commit_file_with_time(repo, "src/bar.rs", "fn b() {}", 1_700_001_000);
        commit_file_with_time(repo, "src/baz.rs", "fn c() {}", 1_700_002_000);
        commit_file_with_time(repo, "src/qux.rs", "fn d() {}", 1_700_003_000);

        let report = evaluate_with_stamps(repo, 1_700_000_500, 1_700_003_000, 60).unwrap();
        assert!(!report.fresh);
        assert_eq!(report.commits_behind, 3);
        assert!(
            report.last_subject.contains("src/qux.rs"),
            "last subject should be newest src commit, got {:?}",
            report.last_subject
        );
        assert!(report.status_line().contains("3 commits behind"));
    }

    #[test]
    fn status_line_handles_single_commit_pluralization() {
        let report = BinaryFreshness {
            fresh: false,
            commits_behind: 1,
            last_subject: "fix: bug".to_string(),
            last_hash: "abc1234".to_string(),
            binary_mtime: 0,
            head_ts: 0,
            worktree_dirty: false,
        };
        assert!(
            report.status_line().contains("1 commit behind"),
            "expected singular 'commit', got {:?}",
            report.status_line()
        );
    }

    #[test]
    fn stale_status_refuses_auto_rebuild_when_worktree_is_dirty() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        init_repo(repo);
        commit_file_with_time(repo, "src/foo.rs", "fn a() {}", 1_700_000_000);
        commit_file_with_time(repo, "src/bar.rs", "fn b() {}", 1_700_001_000);
        fs::write(repo.join("scratch.txt"), "dirty\n").unwrap();

        let report = evaluate_with_stamps(repo, 1_700_000_000, 1_700_001_800, 600).unwrap();
        assert!(!report.fresh);
        assert!(report.worktree_dirty);
        assert!(
            report.status_line().contains("auto-restart refused"),
            "dirty stale report should refuse daemon-owned restart, got {:?}",
            report.status_line()
        );
        assert!(
            report.status_line().contains(STALE_MANUAL_RECOVERY_COMMAND),
            "dirty stale report should include manual fallback, got {:?}",
            report.status_line()
        );
    }

    #[test]
    fn runtime_only_dirty_state_does_not_refuse_auto_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        init_repo(repo);
        commit_file_with_time(repo, "src/foo.rs", "fn a() {}", 1_700_000_000);
        commit_file_with_time(repo, "src/bar.rs", "fn b() {}", 1_700_001_000);
        let telemetry = repo.join(".batty").join("telemetry.db");
        fs::create_dir_all(telemetry.parent().unwrap()).unwrap();
        fs::write(telemetry, "runtime noise\n").unwrap();

        let report = evaluate_with_stamps(repo, 1_700_000_000, 1_700_001_800, 600).unwrap();

        assert!(!report.fresh);
        assert!(
            !report.worktree_dirty,
            "runtime-only dirty state should not block safe daemon restart"
        );
        assert!(
            report.status_line().contains(STALE_RECOVERY_COMMAND),
            "stale runtime-only report should point to safe command, got {:?}",
            report.status_line()
        );
    }

    #[test]
    fn evaluate_binary_freshness_returns_none_when_binary_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        init_repo(repo);
        commit_file_with_time(repo, "src/foo.rs", "fn a() {}", 1_700_000_000);

        let result = evaluate_binary_freshness(&repo.join("does-not-exist"), repo).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn evaluate_binary_freshness_returns_none_outside_git_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("batty");
        fs::write(&bin, "fake binary").unwrap();

        let result = evaluate_binary_freshness(&bin, tmp.path()).unwrap();
        assert!(
            result.is_none(),
            "non-git dir should return None, got {result:?}"
        );
    }
}
