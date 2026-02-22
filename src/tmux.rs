//! tmux session management for batty.
//!
//! Wraps tmux CLI commands for session lifecycle, output capture via pipe-pane,
//! input injection via send-keys, and pane management. This replaces the
//! portable-pty direct approach from Phase 1 with tmux-based supervision.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use tracing::{debug, info};

/// Check that tmux is installed and reachable.
pub fn check_tmux() -> Result<String> {
    let output = Command::new("tmux").arg("-V").output().context(
        "tmux not found — install tmux (e.g., `apt install tmux` or `brew install tmux`)",
    )?;

    if !output.status.success() {
        bail!(
            "tmux -V failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    debug!(version = %version, "tmux found");
    Ok(version)
}

/// Convention for session names: `batty-<phase>`.
pub fn session_name(phase: &str) -> String {
    // tmux target parsing treats '.' as pane separators, so session names
    // should avoid dots and other punctuation that can be interpreted.
    let sanitized: String = phase
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("batty-{sanitized}")
}

/// Check if a tmux session exists.
pub fn session_exists(session: &str) -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", session])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Check if a specific tmux pane target exists.
pub fn pane_exists(target: &str) -> bool {
    Command::new("tmux")
        .args(["display-message", "-p", "-t", target, "#{pane_id}"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Check whether a pane target is dead (`remain-on-exit` pane).
pub fn pane_dead(target: &str) -> Result<bool> {
    let output = Command::new("tmux")
        .args(["display-message", "-p", "-t", target, "#{pane_dead}"])
        .output()
        .with_context(|| format!("failed to query pane_dead for target '{target}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux display-message pane_dead failed: {stderr}");
    }

    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(value == "1")
}

/// Get the active pane id for a session target (for example: `%3`).
pub fn pane_id(target: &str) -> Result<String> {
    let output = Command::new("tmux")
        .args(["display-message", "-p", "-t", target, "#{pane_id}"])
        .output()
        .with_context(|| format!("failed to resolve pane id for target '{target}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux display-message failed: {stderr}");
    }

    let pane = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if pane.is_empty() {
        bail!("tmux returned empty pane id for target '{target}'");
    }
    Ok(pane)
}

/// Create a detached tmux session running the given command.
///
/// The session is created with `new-session -d` so it starts in the background.
/// The executor command is the initial command the session runs.
pub fn create_session(session: &str, program: &str, args: &[String], work_dir: &str) -> Result<()> {
    if session_exists(session) {
        bail!(
            "tmux session '{session}' already exists — use `batty attach` to reconnect, \
             or kill it with `tmux kill-session -t {session}`"
        );
    }

    // Build the full command string for tmux
    // tmux new-session -d -s <name> -c <work_dir> <program> <args...>
    let mut cmd = Command::new("tmux");
    cmd.args(["new-session", "-d", "-s", session, "-c", work_dir]);
    // Set a generous size so the PTY isn't tiny
    cmd.args(["-x", "220", "-y", "50"]);
    cmd.arg(program);
    for arg in args {
        cmd.arg(arg);
    }

    let output = cmd
        .output()
        .with_context(|| format!("failed to create tmux session '{session}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux new-session failed: {stderr}");
    }

    info!(session = session, "tmux session created");
    Ok(())
}

/// Set up pipe-pane to capture all output from the target pane to a log file.
///
/// Uses `tmux pipe-pane -t <session> "cat >> <log_path>"` to stream all PTY
/// output to a file. This is the foundation for event extraction.
pub fn setup_pipe_pane(target: &str, log_path: &Path) -> Result<()> {
    // Ensure log directory exists
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create log directory: {}", parent.display()))?;
    }

    let pipe_cmd = format!("cat >> {}", log_path.display());
    let output = Command::new("tmux")
        .args(["pipe-pane", "-t", target, &pipe_cmd])
        .output()
        .with_context(|| format!("failed to set up pipe-pane for target '{target}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux pipe-pane failed: {stderr}");
    }

    info!(target = target, log = %log_path.display(), "pipe-pane configured");
    Ok(())
}

/// Attach to an existing tmux session (blocks until detach/exit).
pub fn attach(session: &str) -> Result<()> {
    if !session_exists(session) {
        bail!(
            "tmux session '{session}' not found — is batty running? \
             Start with `batty work <phase>`"
        );
    }

    let status = Command::new("tmux")
        .args(["attach-session", "-t", session])
        .status()
        .with_context(|| format!("failed to attach to tmux session '{session}'"))?;

    if !status.success() {
        bail!("tmux attach exited with non-zero status");
    }

    Ok(())
}

/// Send keys to a tmux target (session or pane).
///
/// This is how batty injects responses into the executor's PTY.
/// The `keys` string is sent literally, followed by Enter if `press_enter` is true.
pub fn send_keys(target: &str, keys: &str, press_enter: bool) -> Result<()> {
    let mut cmd = Command::new("tmux");
    cmd.args(["send-keys", "-t", target, keys]);
    if press_enter {
        cmd.arg("Enter");
    }

    let output = cmd
        .output()
        .with_context(|| format!("failed to send keys to target '{target}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux send-keys failed: {stderr}");
    }

    debug!(target = target, keys = keys, "sent keys");
    Ok(())
}

/// Kill a tmux session.
pub fn kill_session(session: &str) -> Result<()> {
    if !session_exists(session) {
        return Ok(()); // already gone
    }

    let output = Command::new("tmux")
        .args(["kill-session", "-t", session])
        .output()
        .with_context(|| format!("failed to kill tmux session '{session}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux kill-session failed: {stderr}");
    }

    info!(session = session, "tmux session killed");
    Ok(())
}

/// Capture the current visible content of a tmux target (session or pane).
///
/// Returns the text currently shown in the pane (useful for prompt detection
/// when pipe-pane output has a lag).
pub fn capture_pane(target: &str) -> Result<String> {
    let output = Command::new("tmux")
        .args(["capture-pane", "-t", target, "-p"])
        .output()
        .with_context(|| format!("failed to capture pane for target '{target}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux capture-pane failed: {stderr}");
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Wait for the tmux session to end (executor process exits).
///
/// Polls `has-session` at the given interval. Returns when the session is gone
/// or an error occurs.
#[allow(dead_code)]
pub fn wait_for_session_end(session: &str, poll_interval: std::time::Duration) -> Result<()> {
    loop {
        if !session_exists(session) {
            info!(session = session, "tmux session ended");
            return Ok(());
        }
        std::thread::sleep(poll_interval);
    }
}

/// Set the tmux status bar left content.
#[allow(dead_code)]
pub fn set_status_left(session: &str, content: &str) -> Result<()> {
    tmux_set(session, "status-left", content)
}

/// Set the tmux status bar right content.
#[allow(dead_code)]
pub fn set_status_right(session: &str, content: &str) -> Result<()> {
    tmux_set(session, "status-right", content)
}

/// Set the tmux status bar style.
#[allow(dead_code)]
pub fn set_status_style(session: &str, style: &str) -> Result<()> {
    tmux_set(session, "status-style", style)
}

/// Set the terminal title via tmux.
#[allow(dead_code)]
pub fn set_title(session: &str, title: &str) -> Result<()> {
    tmux_set(session, "set-titles", "on")?;
    tmux_set(session, "set-titles-string", title)
}

/// Split the window to create a new pane.
///
/// Returns Ok(()) on success. The new pane is at the bottom (vertical split)
/// with the given percentage of height.
#[allow(dead_code)]
pub fn split_window_vertical(session: &str, percent: u32) -> Result<()> {
    let output = Command::new("tmux")
        .args([
            "split-window",
            "-v",
            "-p",
            &percent.to_string(),
            "-t",
            session,
        ])
        .output()
        .with_context(|| format!("failed to split window in session '{session}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux split-window failed: {stderr}");
    }

    Ok(())
}

/// List panes in a session.
///
/// Returns a list of pane IDs (e.g., ["%0", "%1"]).
pub fn list_panes(session: &str) -> Result<Vec<String>> {
    let output = Command::new("tmux")
        .args(["list-panes", "-t", session, "-F", "#{pane_id}"])
        .output()
        .with_context(|| format!("failed to list panes for session '{session}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux list-panes failed: {stderr}");
    }

    let panes = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|s| s.to_string())
        .collect();

    Ok(panes)
}

/// Helper: run `tmux set -t <session> <option> <value>`.
pub fn tmux_set(session: &str, option: &str, value: &str) -> Result<()> {
    let output = Command::new("tmux")
        .args(["set", "-t", session, option, value])
        .output()
        .with_context(|| format!("failed to set tmux option '{option}' for session '{session}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux set {option} failed: {stderr}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_name_convention() {
        assert_eq!(session_name("phase-1"), "batty-phase-1");
        assert_eq!(session_name("phase-2"), "batty-phase-2");
        assert_eq!(session_name("phase-2.5"), "batty-phase-2-5");
        assert_eq!(session_name("phase 3"), "batty-phase-3");
    }

    #[test]
    fn check_tmux_finds_binary() {
        let version = check_tmux().unwrap();
        assert!(
            version.starts_with("tmux"),
            "expected tmux version, got: {version}"
        );
    }

    #[test]
    fn nonexistent_session_does_not_exist() {
        assert!(!session_exists("batty-test-nonexistent-12345"));
    }

    #[test]
    fn create_and_kill_session() {
        let session = "batty-test-lifecycle";
        // Clean up in case a previous test left it
        let _ = kill_session(session);

        // Create
        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();
        assert!(session_exists(session));

        // Kill
        kill_session(session).unwrap();
        assert!(!session_exists(session));
    }

    #[test]
    fn duplicate_session_is_error() {
        let session = "batty-test-dup";
        let _ = kill_session(session);

        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        let result = create_session(session, "sleep", &["10".to_string()], "/tmp");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));

        kill_session(session).unwrap();
    }

    #[test]
    fn send_keys_to_session() {
        let session = "batty-test-sendkeys";
        let _ = kill_session(session);

        // Create a session running cat (waits for input)
        create_session(session, "cat", &[], "/tmp").unwrap();

        // Give it a moment to start
        std::thread::sleep(std::time::Duration::from_millis(200));

        // Send keys should succeed
        send_keys(session, "hello", true).unwrap();

        // Clean up
        kill_session(session).unwrap();
    }

    #[test]
    fn pipe_pane_captures_output() {
        let session = "batty-test-pipe";
        let _ = kill_session(session);

        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("pty-output.log");

        // Create session with bash, set up pipe-pane FIRST, then trigger output
        create_session(session, "bash", &[], "/tmp").unwrap();

        // Set up pipe-pane before generating output
        setup_pipe_pane(session, &log_path).unwrap();

        // Small delay to ensure pipe-pane is active
        std::thread::sleep(std::time::Duration::from_millis(200));

        // Now generate output that pipe-pane will capture
        send_keys(session, "echo pipe-test-output", true).unwrap();

        // Wait with retries for the output to appear in the log
        let mut found = false;
        for _ in 0..10 {
            std::thread::sleep(std::time::Duration::from_millis(200));
            if log_path.exists() {
                let content = std::fs::read_to_string(&log_path).unwrap_or_default();
                if !content.is_empty() {
                    found = true;
                    break;
                }
            }
        }

        kill_session(session).unwrap();
        assert!(found, "pipe-pane log should have captured output");
    }

    #[test]
    fn capture_pane_returns_content() {
        let session = "batty-test-capture";
        let _ = kill_session(session);

        create_session(
            session,
            "bash",
            &["-c".to_string(), "echo 'capture-test'; sleep 2".to_string()],
            "/tmp",
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));

        let content = capture_pane(session).unwrap();
        // Should have some content (at least the echo output or prompt)
        assert!(
            !content.trim().is_empty(),
            "capture-pane should return content"
        );

        kill_session(session).unwrap();
    }

    #[test]
    fn list_panes_returns_at_least_one() {
        let session = "batty-test-panes";
        let _ = kill_session(session);

        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        let panes = list_panes(session).unwrap();
        assert!(!panes.is_empty(), "session should have at least one pane");

        kill_session(session).unwrap();
    }

    #[test]
    fn kill_nonexistent_session_is_ok() {
        // Should not error — idempotent
        kill_session("batty-test-nonexistent-kill-99999").unwrap();
    }

    #[test]
    fn session_with_short_lived_process() {
        let session = "batty-test-shortlived";
        let _ = kill_session(session);

        // echo exits immediately
        create_session(session, "echo", &["done".to_string()], "/tmp").unwrap();

        // Give it a moment for the process to exit
        std::thread::sleep(std::time::Duration::from_millis(500));

        // Session may or may not still exist depending on tmux remain-on-exit
        // Either way, kill should be safe
        let _ = kill_session(session);
    }
}
