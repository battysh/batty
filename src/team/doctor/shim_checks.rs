//! Shim health checks for `batty doctor`.
//!
//! When `use_shim: true` in team config, these checks validate that the shim
//! subsystem prerequisites are met: PTY creation, socketpair, vt100 parser,
//! shim-logs directory, and agent classifiers.

use std::fs;
use std::path::Path;
use std::process::Command;

use super::util::check_line;
use super::{CheckLevel, CheckLine};

/// Results of all shim health checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ShimHealthReport {
    pub checks: Vec<CheckLine>,
}

/// Run all shim health checks. Returns check lines to be rendered in the
/// doctor report. When `fix` is true, attempts to create missing directories.
pub(super) fn build_shim_checks(project_root: &Path, fix: bool) -> ShimHealthReport {
    let mut checks = Vec::new();

    checks.push(check_pty_creation());
    checks.push(check_socketpair());
    checks.push(check_vt100_sanity());
    checks.push(check_shim_log_dir(project_root, fix));
    checks.push(check_shim_binary());
    checks.extend(check_agent_classifiers());

    ShimHealthReport { checks }
}

/// Verify portable-pty can open a PTY pair on this platform.
fn check_pty_creation() -> CheckLine {
    use portable_pty::{PtySize, native_pty_system};

    let pty_system = native_pty_system();
    match pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(_pair) => check_line(
            CheckLevel::Pass,
            "PTY creation: portable-pty opened successfully",
        ),
        Err(error) => check_line(
            CheckLevel::Fail,
            format!("PTY creation: failed to open PTY pair: {error}"),
        ),
    }
}

/// Verify Unix socketpair creation works.
fn check_socketpair() -> CheckLine {
    match std::os::unix::net::UnixStream::pair() {
        Ok(_pair) => check_line(
            CheckLevel::Pass,
            "socketpair: SOCK_STREAM Unix socketpair created successfully",
        ),
        Err(error) => check_line(
            CheckLevel::Fail,
            format!("socketpair: failed to create Unix socketpair: {error}"),
        ),
    }
}

/// Verify the vt100 parser initializes correctly with default dimensions.
fn check_vt100_sanity() -> CheckLine {
    let parser = vt100::Parser::new(50, 220, 0);
    let screen = parser.screen();
    let size = screen.size();
    if size == (50, 220) {
        check_line(
            CheckLevel::Pass,
            "vt100 parser: initialized correctly (50x220)",
        )
    } else {
        check_line(
            CheckLevel::Fail,
            format!(
                "vt100 parser: unexpected dimensions {:?}, expected (50, 220)",
                size
            ),
        )
    }
}

/// Verify `.batty/shim-logs/` exists and is writable (or can be created with --fix).
fn check_shim_log_dir(project_root: &Path, fix: bool) -> CheckLine {
    let shim_logs = project_root.join(".batty").join("shim-logs");

    if shim_logs.is_dir() {
        // Check writability by attempting to create and remove a temp file
        let probe = shim_logs.join(".batty-doctor-probe");
        match fs::write(&probe, b"probe") {
            Ok(()) => {
                let _ = fs::remove_file(&probe);
                check_line(
                    CheckLevel::Pass,
                    "shim-logs directory: exists and is writable",
                )
            }
            Err(error) => check_line(
                CheckLevel::Fail,
                format!("shim-logs directory: exists but not writable: {error}"),
            ),
        }
    } else if fix {
        match fs::create_dir_all(&shim_logs) {
            Ok(()) => check_line(CheckLevel::Pass, "shim-logs directory: created by --fix"),
            Err(error) => check_line(
                CheckLevel::Fail,
                format!("shim-logs directory: --fix failed to create: {error}"),
            ),
        }
    } else {
        check_line(
            CheckLevel::Warn,
            "shim-logs directory: missing (run with --fix to create)",
        )
    }
}

/// Verify `batty shim --help` executes successfully (validates binary can fork/exec).
fn check_shim_binary() -> CheckLine {
    let binary = std::env::current_exe().unwrap_or_else(|_| "batty".into());
    match Command::new(&binary).args(["shim", "--help"]).output() {
        Ok(output) if output.status.success() => check_line(
            CheckLevel::Pass,
            "shim binary: `batty shim --help` executed successfully",
        ),
        Ok(output) => check_line(
            CheckLevel::Fail,
            format!(
                "shim binary: `batty shim --help` exited with {}",
                output.status
            ),
        ),
        Err(error) => check_line(
            CheckLevel::Fail,
            format!("shim binary: failed to execute `batty shim --help`: {error}"),
        ),
    }
}

/// For each known agent type, verify the classifier recognizes at least one
/// known prompt pattern (sanity check).
fn check_agent_classifiers() -> Vec<CheckLine> {
    use crate::shim::classifier::{AgentType, ScreenVerdict, classify};

    let test_cases: &[(AgentType, &str, ScreenVerdict)] = &[
        (
            AgentType::Claude,
            "Some output\n\n\u{276F} ",
            ScreenVerdict::AgentIdle,
        ),
        (
            AgentType::Codex,
            "Done.\n\n\u{203A} ",
            ScreenVerdict::AgentIdle,
        ),
        (AgentType::Kiro, "Result\nKiro> ", ScreenVerdict::AgentIdle),
        (
            AgentType::Generic,
            "user@host:~$ ",
            ScreenVerdict::AgentIdle,
        ),
    ];

    test_cases
        .iter()
        .map(|(agent_type, content, expected)| {
            let mut parser = vt100::Parser::new(24, 80, 0);
            parser.process(content.as_bytes());
            let verdict = classify(*agent_type, parser.screen());
            if verdict == *expected {
                check_line(
                    CheckLevel::Pass,
                    format!(
                        "classifier {agent_type}: recognized known prompt pattern as {expected:?}"
                    ),
                )
            } else {
                check_line(
                    CheckLevel::Fail,
                    format!("classifier {agent_type}: expected {expected:?} but got {verdict:?}"),
                )
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_pty_creation_passes() {
        let result = check_pty_creation();
        assert_eq!(result.level, CheckLevel::Pass);
        assert!(result.message.contains("PTY creation"));
    }

    #[test]
    fn check_socketpair_passes() {
        let result = check_socketpair();
        assert_eq!(result.level, CheckLevel::Pass);
        assert!(result.message.contains("socketpair"));
    }

    #[test]
    fn check_vt100_sanity_passes() {
        let result = check_vt100_sanity();
        assert_eq!(result.level, CheckLevel::Pass);
        assert!(result.message.contains("vt100 parser"));
    }

    #[test]
    fn check_shim_log_dir_warns_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let result = check_shim_log_dir(tmp.path(), false);
        assert_eq!(result.level, CheckLevel::Warn);
        assert!(result.message.contains("missing"));
        assert!(result.message.contains("--fix"));
    }

    #[test]
    fn check_shim_log_dir_passes_when_exists() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("shim-logs")).unwrap();
        let result = check_shim_log_dir(tmp.path(), false);
        assert_eq!(result.level, CheckLevel::Pass);
        assert!(result.message.contains("writable"));
    }

    #[test]
    fn check_shim_log_dir_fix_creates_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let result = check_shim_log_dir(tmp.path(), true);
        assert_eq!(result.level, CheckLevel::Pass);
        assert!(result.message.contains("created by --fix"));
        assert!(tmp.path().join(".batty").join("shim-logs").is_dir());
    }

    #[test]
    fn check_agent_classifiers_all_pass() {
        let results = check_agent_classifiers();
        assert_eq!(results.len(), 4);
        for result in &results {
            assert_eq!(result.level, CheckLevel::Pass, "failed: {}", result.message);
        }
    }

    #[test]
    fn build_shim_checks_returns_all_checks() {
        let tmp = tempfile::tempdir().unwrap();
        let report = build_shim_checks(tmp.path(), false);
        // PTY + socketpair + vt100 + shim-logs + shim binary + 4 classifiers = 9
        assert!(
            report.checks.len() >= 8,
            "expected at least 8 checks, got {}",
            report.checks.len()
        );
    }

    #[test]
    fn build_shim_checks_with_fix_creates_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let report = build_shim_checks(tmp.path(), true);
        assert!(tmp.path().join(".batty").join("shim-logs").is_dir());
        let log_check = report
            .checks
            .iter()
            .find(|c| c.message.contains("shim-logs"))
            .unwrap();
        assert_eq!(log_check.level, CheckLevel::Pass);
    }
}
