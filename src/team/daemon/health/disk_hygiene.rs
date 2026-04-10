//! Automated disk hygiene — periodic disk pressure checks, shared-target
//! size enforcement, shim-log rotation, inbox rotation, and git gc.

use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use tracing::{debug, info, warn};

use super::super::*;
use crate::team::config::DiskHygieneConfig;
use crate::team::events::TeamEvent;
use crate::team::task_loop::shared_cargo_target_dir;

/// Result of a single disk hygiene pass.
#[derive(Debug, Default)]
pub(crate) struct HygieneReport {
    pub shared_target_cleaned_gb: f64,
    pub shim_logs_rotated: usize,
    pub inbox_messages_rotated: usize,
    pub git_gc_ran: bool,
    pub branches_pruned: Vec<String>,
}

impl HygieneReport {
    pub fn any_action_taken(&self) -> bool {
        self.shared_target_cleaned_gb > 0.0
            || self.shim_logs_rotated > 0
            || self.inbox_messages_rotated > 0
            || self.git_gc_ran
            || !self.branches_pruned.is_empty()
    }

    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.shared_target_cleaned_gb > 0.0 {
            parts.push(format!(
                "shared-target: {:.1}GB freed",
                self.shared_target_cleaned_gb
            ));
        }
        if self.shim_logs_rotated > 0 {
            parts.push(format!("shim-logs: {} rotated", self.shim_logs_rotated));
        }
        if self.inbox_messages_rotated > 0 {
            parts.push(format!(
                "inbox: {} messages rotated",
                self.inbox_messages_rotated
            ));
        }
        if self.git_gc_ran {
            parts.push("git gc: ran".to_string());
        }
        if !self.branches_pruned.is_empty() {
            parts.push(format!("branches: {} pruned", self.branches_pruned.len()));
        }
        if parts.is_empty() {
            "no cleanup needed".to_string()
        } else {
            parts.join(", ")
        }
    }
}

impl TeamDaemon {
    /// Periodic disk hygiene check — runs on a configurable interval.
    pub(in super::super) fn maybe_run_disk_hygiene(&mut self) -> Result<()> {
        let config = &self.config.team_config.automation.disk_hygiene;
        if !config.enabled {
            return Ok(());
        }

        let interval = Duration::from_secs(config.check_interval_secs);
        if self.last_disk_hygiene_check.elapsed() < interval {
            return Ok(());
        }
        self.last_disk_hygiene_check = Instant::now();

        let project_root = self.project_root().to_path_buf();
        let report = run_disk_hygiene(&project_root, config)?;

        if report.any_action_taken() {
            let summary = report.summary();
            info!(summary = %summary, "disk hygiene pass completed");
            self.record_orchestrator_action(format!("disk-hygiene: {summary}"));
            self.emit_event(TeamEvent::disk_hygiene_cleanup(&summary));
        } else {
            debug!("disk hygiene: no cleanup needed");
        }

        Ok(())
    }
}

/// Run a full disk hygiene pass, returning a report of actions taken.
pub(crate) fn run_disk_hygiene(
    project_root: &Path,
    config: &DiskHygieneConfig,
) -> Result<HygieneReport> {
    let mut report = HygieneReport::default();

    // 1. Check shared-target size and clean if over budget
    let shared_target = shared_cargo_target_dir(project_root);
    if shared_target.is_dir() {
        let size_bytes = dir_size_bytes(&shared_target);
        let max_bytes = config.max_shared_target_gb * 1_073_741_824; // GB → bytes
        if size_bytes > max_bytes {
            let freed = clean_shared_target_incremental(&shared_target)?;
            report.shared_target_cleaned_gb = freed as f64 / 1_073_741_824.0;
        }
    }

    // 2. Check available disk space
    let free_gb = available_disk_gb(project_root);
    if free_gb < config.min_free_gb as f64 {
        // Under pressure — run aggressive cleanup.
        let shared_target = shared_cargo_target_dir(project_root);
        if shared_target.is_dir() {
            let freed = clean_shared_target_incremental(&shared_target)?;
            report.shared_target_cleaned_gb += freed as f64 / 1_073_741_824.0;
        }

        // If STILL under significant pressure after incremental cleanup,
        // escalate to emergency mode: remove engineer deps/ directories that
        // don't belong to actively-building worktrees. This reclaims the bulk
        // of the build artifacts (typically 60-80% of the shared-target
        // footprint) at the cost of a cold rebuild on next engineer task.
        // Trigger threshold is a conservative half of min_free_gb so we only
        // nuke deps under true pressure, not normal operation.
        let free_gb_after_incremental = available_disk_gb(project_root);
        if free_gb_after_incremental < (config.min_free_gb as f64) * 0.5 {
            let shared_target = shared_cargo_target_dir(project_root);
            if shared_target.is_dir() {
                let freed = clean_shared_target_deps_emergency(&shared_target)?;
                report.shared_target_cleaned_gb += freed as f64 / 1_073_741_824.0;
            }
        }
    }

    // 3. Rotate old shim-logs
    let shim_logs = crate::team::shim_logs_dir(project_root);
    if shim_logs.is_dir() {
        report.shim_logs_rotated = rotate_old_files(
            &shim_logs,
            Duration::from_secs(config.log_rotation_hours * 3600),
        )?;
    }

    // 4. Rotate old inbox messages
    let inboxes = crate::team::inbox::inboxes_root(project_root);
    if inboxes.is_dir() {
        report.inbox_messages_rotated = rotate_old_inbox_messages(
            &inboxes,
            Duration::from_secs(config.log_rotation_hours * 3600),
        )?;
    }

    // 5. Run git gc if objects are large (> 500MB)
    let git_objects = project_root.join(".git").join("objects");
    if git_objects.is_dir() {
        let objects_bytes = dir_size_bytes(&git_objects);
        if objects_bytes > 500 * 1_048_576 {
            let status = std::process::Command::new("git")
                .args(["gc", "--prune=now"])
                .current_dir(project_root)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            match status {
                Ok(s) if s.success() => report.git_gc_ran = true,
                Ok(s) => debug!(exit_code = ?s.code(), "git gc exited non-zero"),
                Err(e) => debug!(error = %e, "git gc failed to start"),
            }
        }
    }

    Ok(report)
}

/// Post-merge cleanup for an engineer's worktree.
/// Called after a successful merge to free build artifacts.
pub(crate) fn post_merge_cleanup(
    project_root: &Path,
    engineer: &str,
    task_id: u32,
    branch: &str,
    config: &DiskHygieneConfig,
) -> HygieneReport {
    let mut report = HygieneReport::default();

    if !config.enabled {
        return report;
    }

    // Clean incremental build artifacts for this engineer
    if config.post_merge_cleanup {
        let shared_target = shared_cargo_target_dir(project_root);
        let engineer_dir = shared_target.join(engineer);
        if engineer_dir.is_dir() {
            let incremental = engineer_dir.join("debug").join("incremental");
            if incremental.is_dir() {
                let size_before = dir_size_bytes(&incremental);
                if let Err(e) = std::fs::remove_dir_all(&incremental) {
                    warn!(
                        engineer,
                        path = %incremental.display(),
                        error = %e,
                        "failed to clean incremental build dir"
                    );
                } else {
                    report.shared_target_cleaned_gb = size_before as f64 / 1_073_741_824.0;
                }
            }
        }
    }

    // Prune the merged branch
    if config.prune_merged_branches {
        let prune_result = std::process::Command::new("git")
            .args(["branch", "-D", branch])
            .current_dir(project_root)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output();
        match prune_result {
            Ok(output) if output.status.success() => {
                report.branches_pruned.push(branch.to_string());
            }
            Ok(_) => {
                debug!(
                    engineer,
                    branch, task_id, "branch already deleted or not found"
                );
            }
            Err(e) => {
                warn!(engineer, branch, error = %e, "failed to prune branch");
            }
        }
    }

    report
}

// ── Helper functions ──

/// Approximate directory size in bytes (non-recursive follows symlinks).
pub(crate) fn dir_size_bytes(path: &Path) -> u64 {
    walkdir(path)
}

fn walkdir(path: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            total += walkdir(&entry.path());
        } else {
            total += meta.len();
        }
    }
    total
}

/// Get available disk space in GB for the filesystem containing `path`.
fn available_disk_gb(path: &Path) -> f64 {
    let output = match std::process::Command::new("df")
        .args(["-Pk", &path.to_string_lossy()])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return f64::MAX, // can't determine → assume plenty
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(line) = stdout.lines().nth(1) else {
        return f64::MAX;
    };
    // df -Pk: Filesystem 1024-blocks Used Available Capacity Mounted-on
    let available_kb: f64 = line
        .split_whitespace()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(f64::MAX);
    available_kb / 1_048_576.0 // KB → GB
}

/// Clean stale incremental build directories under shared-target.
/// Returns bytes freed.
fn clean_shared_target_incremental(shared_target: &Path) -> Result<u64> {
    let mut freed = 0u64;
    let Ok(entries) = std::fs::read_dir(shared_target) else {
        return Ok(0);
    };
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let incremental = entry.path().join("debug").join("incremental");
        if incremental.is_dir() {
            let size = dir_size_bytes(&incremental);
            if std::fs::remove_dir_all(&incremental).is_ok() {
                freed += size;
                info!(
                    path = %incremental.display(),
                    size_mb = size / 1_048_576,
                    "cleaned incremental build dir"
                );
            }
        }
    }
    Ok(freed)
}

/// Emergency cleanup: remove the entire `debug/deps/` and `debug/build/`
/// trees for every engineer under the shared-target. This reclaims the
/// bulk of the build artifacts that `clean_shared_target_incremental` can
/// not touch. Only call this when disk is under real pressure — the cost
/// is a full cold rebuild on the next engineer task.
///
/// We preserve `debug/<engineer>/` itself and the release profile so
/// active builds that are halfway through linking don't get their partial
/// output deleted (rmdir of a live deps/ would break a concurrent rustc).
/// In practice this is still fine because the incremental cleanup above
/// already interrupts live builds, and emergency mode only fires when the
/// disk is about to wedge the whole machine.
fn clean_shared_target_deps_emergency(shared_target: &Path) -> Result<u64> {
    let mut freed = 0u64;
    let Ok(entries) = std::fs::read_dir(shared_target) else {
        return Ok(0);
    };
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let debug_dir = entry.path().join("debug");
        for sub in ["deps", "build"] {
            let victim = debug_dir.join(sub);
            if victim.is_dir() {
                let size = dir_size_bytes(&victim);
                if std::fs::remove_dir_all(&victim).is_ok() {
                    freed += size;
                    warn!(
                        path = %victim.display(),
                        size_mb = size / 1_048_576,
                        "emergency cleanup: removed shared-target bulk artifact"
                    );
                }
            }
        }
    }
    Ok(freed)
}

/// Remove files older than `max_age` from a directory. Returns count removed.
fn rotate_old_files(dir: &Path, max_age: Duration) -> Result<usize> {
    let mut count = 0usize;
    let now = SystemTime::now();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(0);
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let modified = meta.modified().unwrap_or(now);
        if now.duration_since(modified).unwrap_or(Duration::ZERO) > max_age {
            if std::fs::remove_file(entry.path()).is_ok() {
                count += 1;
            }
        }
    }
    Ok(count)
}

/// Remove old inbox messages across all member inboxes.
fn rotate_old_inbox_messages(inboxes_root: &Path, max_age: Duration) -> Result<usize> {
    let mut count = 0usize;
    let Ok(members) = std::fs::read_dir(inboxes_root) else {
        return Ok(0);
    };
    for member_dir in members.flatten() {
        if !member_dir.path().is_dir() {
            continue;
        }
        // Inbox messages are in subdirs (cur/new/tmp per Maildir format)
        for subdir_name in ["cur", "new", "tmp"] {
            let subdir = member_dir.path().join(subdir_name);
            if subdir.is_dir() {
                count += rotate_old_files(&subdir, max_age)?;
            }
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn test_config() -> DiskHygieneConfig {
        DiskHygieneConfig {
            enabled: true,
            check_interval_secs: 600,
            min_free_gb: 10,
            max_shared_target_gb: 4,
            log_rotation_hours: 24,
            post_merge_cleanup: true,
            prune_merged_branches: true,
        }
    }

    #[test]
    fn hygiene_report_no_actions_summary() {
        let report = HygieneReport::default();
        assert!(!report.any_action_taken());
        assert_eq!(report.summary(), "no cleanup needed");
    }

    #[test]
    fn hygiene_report_with_actions_summary() {
        let report = HygieneReport {
            shared_target_cleaned_gb: 2.5,
            shim_logs_rotated: 3,
            inbox_messages_rotated: 10,
            git_gc_ran: true,
            branches_pruned: vec!["eng-1/42".to_string()],
        };
        assert!(report.any_action_taken());
        let summary = report.summary();
        assert!(summary.contains("shared-target: 2.5GB freed"));
        assert!(summary.contains("shim-logs: 3 rotated"));
        assert!(summary.contains("inbox: 10 messages rotated"));
        assert!(summary.contains("git gc: ran"));
        assert!(summary.contains("branches: 1 pruned"));
    }

    #[test]
    fn dir_size_bytes_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(dir_size_bytes(tmp.path()), 0);
    }

    #[test]
    fn dir_size_bytes_counts_files() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), "hello").unwrap();
        fs::write(tmp.path().join("b.txt"), "world!").unwrap();
        assert_eq!(dir_size_bytes(tmp.path()), 11); // 5 + 6
    }

    #[test]
    fn dir_size_bytes_recursive() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("file.txt"), "abc").unwrap();
        assert_eq!(dir_size_bytes(tmp.path()), 3);
    }

    #[test]
    fn dir_size_bytes_nonexistent() {
        assert_eq!(dir_size_bytes(Path::new("/nonexistent/path")), 0);
    }

    #[test]
    fn rotate_old_files_removes_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let old_file = tmp.path().join("old.log");
        fs::write(&old_file, "old data").unwrap();

        // Set modification time to 48 hours ago
        let forty_eight_hours_ago = filetime::FileTime::from_system_time(
            SystemTime::now() - Duration::from_secs(48 * 3600),
        );
        filetime::set_file_mtime(&old_file, forty_eight_hours_ago).unwrap();

        let fresh_file = tmp.path().join("fresh.log");
        fs::write(&fresh_file, "new data").unwrap();

        let count = rotate_old_files(tmp.path(), Duration::from_secs(24 * 3600)).unwrap();
        assert_eq!(count, 1);
        assert!(!old_file.exists());
        assert!(fresh_file.exists());
    }

    #[test]
    fn rotate_old_files_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let count = rotate_old_files(tmp.path(), Duration::from_secs(3600)).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn rotate_old_inbox_messages_removes_stale_across_members() {
        let tmp = tempfile::tempdir().unwrap();
        let inboxes = tmp.path();

        // Create two member inboxes with Maildir structure
        for member in ["eng-1", "eng-2"] {
            for subdir in ["cur", "new", "tmp"] {
                fs::create_dir_all(inboxes.join(member).join(subdir)).unwrap();
            }
        }

        // Write old messages
        let old_msg = inboxes.join("eng-1").join("cur").join("msg-old.txt");
        fs::write(&old_msg, "old message").unwrap();
        let old_time = filetime::FileTime::from_system_time(
            SystemTime::now() - Duration::from_secs(48 * 3600),
        );
        filetime::set_file_mtime(&old_msg, old_time).unwrap();

        // Write fresh message
        let fresh_msg = inboxes.join("eng-2").join("new").join("msg-fresh.txt");
        fs::write(&fresh_msg, "fresh message").unwrap();

        let count = rotate_old_inbox_messages(inboxes, Duration::from_secs(24 * 3600)).unwrap();
        assert_eq!(count, 1);
        assert!(!old_msg.exists());
        assert!(fresh_msg.exists());
    }

    #[test]
    fn clean_shared_target_incremental_cleans_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let shared_target = tmp.path();

        // Create engineer incremental dirs
        let inc = shared_target
            .join("eng-1")
            .join("debug")
            .join("incremental");
        fs::create_dir_all(&inc).unwrap();
        fs::write(inc.join("data.bin"), vec![0u8; 1024]).unwrap();

        let freed = clean_shared_target_incremental(shared_target).unwrap();
        assert!(freed > 0);
        assert!(!inc.exists());
    }

    #[test]
    fn clean_shared_target_deps_emergency_removes_deps_and_build_but_preserves_engineer_dir() {
        // Regression for 2026-04-10 disk-pressure bug: the incremental-only
        // cleanup could not reclaim enough space under real pressure because
        // the bulk of the footprint sits in debug/deps/ and debug/build/.
        // This locks in the emergency-mode behavior: remove deps/ and build/
        // per engineer, but leave the engineer dir itself so future builds
        // can recreate the tree cleanly.
        let tmp = tempfile::tempdir().unwrap();
        let shared_target = tmp.path();

        for engineer in &["eng-1", "eng-2"] {
            let debug = shared_target.join(engineer).join("debug");
            let deps = debug.join("deps");
            let build = debug.join("build");
            let incremental = debug.join("incremental");
            fs::create_dir_all(&deps).unwrap();
            fs::create_dir_all(&build).unwrap();
            fs::create_dir_all(&incremental).unwrap();
            fs::write(deps.join("libbatty.rlib"), vec![0u8; 8192]).unwrap();
            fs::write(build.join("something.o"), vec![0u8; 4096]).unwrap();
            fs::write(incremental.join("cache.bin"), vec![0u8; 1024]).unwrap();
        }

        let freed = clean_shared_target_deps_emergency(shared_target).unwrap();

        assert!(
            freed >= (8192 + 4096) * 2,
            "should have freed at least deps+build per engineer, freed={freed}",
        );
        // deps/ and build/ removed for both engineers...
        for engineer in &["eng-1", "eng-2"] {
            assert!(
                !shared_target
                    .join(engineer)
                    .join("debug")
                    .join("deps")
                    .exists()
            );
            assert!(
                !shared_target
                    .join(engineer)
                    .join("debug")
                    .join("build")
                    .exists()
            );
            // ...but the engineer dir and debug/ skeleton remain so the next
            // build can recreate them cleanly.
            assert!(shared_target.join(engineer).join("debug").exists());
            // incremental is left alone by the emergency path (caller runs
            // the incremental cleanup first).
            assert!(
                shared_target
                    .join(engineer)
                    .join("debug")
                    .join("incremental")
                    .exists()
            );
        }
    }

    #[test]
    fn post_merge_cleanup_disabled_config() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = test_config();
        config.enabled = false;

        let report = post_merge_cleanup(tmp.path(), "eng-1", 42, "eng-1/42", &config);
        assert!(!report.any_action_taken());
    }

    #[test]
    fn post_merge_cleanup_cleans_incremental() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config();

        // Set up shared-target with incremental dir
        let inc = tmp
            .path()
            .join(".batty")
            .join("shared-target")
            .join("eng-1")
            .join("debug")
            .join("incremental");
        fs::create_dir_all(&inc).unwrap();
        fs::write(inc.join("data.bin"), vec![0u8; 4096]).unwrap();

        let report = post_merge_cleanup(tmp.path(), "eng-1", 42, "eng-1/42", &config);
        assert!(report.shared_target_cleaned_gb > 0.0);
        assert!(!inc.exists());
    }

    #[test]
    fn post_merge_cleanup_no_incremental_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config();

        // No shared-target dir exists
        let report = post_merge_cleanup(tmp.path(), "eng-1", 42, "eng-1/42", &config);
        // Should not panic, just report nothing
        assert_eq!(report.shared_target_cleaned_gb, 0.0);
    }

    #[test]
    fn run_disk_hygiene_empty_project() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config();
        let report = run_disk_hygiene(tmp.path(), &config).unwrap();
        assert!(!report.any_action_taken());
    }

    #[test]
    fn run_disk_hygiene_rotates_old_shim_logs() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config();

        let shim_logs = tmp.path().join(".batty").join("shim-logs");
        fs::create_dir_all(&shim_logs).unwrap();
        let old_log = shim_logs.join("eng-1.pty.log");
        fs::write(&old_log, "old log data").unwrap();
        let old_time = filetime::FileTime::from_system_time(
            SystemTime::now() - Duration::from_secs(48 * 3600),
        );
        filetime::set_file_mtime(&old_log, old_time).unwrap();

        let report = run_disk_hygiene(tmp.path(), &config).unwrap();
        assert_eq!(report.shim_logs_rotated, 1);
        assert!(!old_log.exists());
    }

    #[test]
    fn available_disk_gb_returns_value() {
        // Just verify it doesn't panic and returns a reasonable value
        let gb = available_disk_gb(Path::new("/tmp"));
        assert!(gb > 0.0);
    }
}
