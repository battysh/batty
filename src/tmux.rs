//! tmux session management for batty.
//!
//! Wraps tmux CLI commands for session lifecycle, output capture via pipe-pane,
//! input injection via send-keys, and pane management. This replaces the
//! portable-pty direct approach from Phase 1 with tmux-based supervision.

use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use tracing::{debug, info, warn};

static PROBE_COUNTER: AtomicU64 = AtomicU64::new(1);
const SUPERVISOR_CONTROL_OPTION: &str = "@batty_supervisor_control";

/// Default tmux-prefix hotkey for pausing Batty supervision.
pub const SUPERVISOR_PAUSE_HOTKEY: &str = "C-b P";
/// Default tmux-prefix hotkey for resuming Batty supervision.
pub const SUPERVISOR_RESUME_HOTKEY: &str = "C-b R";

/// Known split strategies for creating the orchestrator log pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitMode {
    Lines,
    Percent,
    Disabled,
}

/// tmux capability probe result used by orchestrator startup.
#[derive(Debug, Clone)]
pub struct TmuxCapabilities {
    pub version_raw: String,
    pub version: Option<(u32, u32)>,
    pub pipe_pane: bool,
    pub pipe_pane_only_if_missing: bool,
    pub status_style: bool,
    pub split_mode: SplitMode,
}

impl TmuxCapabilities {
    /// Known-good range documented for Batty runtime behavior.
    ///
    /// Current matrix:
    /// - 3.2+ known-good
    /// - 3.1 supported with fallbacks
    /// - older versions unsupported
    pub fn known_good(&self) -> bool {
        matches!(self.version, Some((major, minor)) if major > 3 || (major == 3 && minor >= 2))
    }

    pub fn remediation_message(&self) -> String {
        format!(
            "tmux capability check failed (detected '{}'). Batty requires working `pipe-pane` support. \
Install or upgrade tmux (recommended >= 3.2) and re-run `batty work` or `batty resume`.",
            self.version_raw
        )
    }
}

/// Metadata for a tmux pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneDetails {
    pub id: String,
    pub command: String,
    pub active: bool,
    pub dead: bool,
}

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

fn parse_tmux_version(version_raw: &str) -> Option<(u32, u32)> {
    let raw = version_raw.trim();
    let ver = raw.strip_prefix("tmux ")?;
    let mut chars = ver.chars().peekable();

    let mut major = String::new();
    while let Some(c) = chars.peek() {
        if c.is_ascii_digit() {
            major.push(*c);
            chars.next();
        } else {
            break;
        }
    }
    if major.is_empty() {
        return None;
    }

    if chars.next()? != '.' {
        return None;
    }

    let mut minor = String::new();
    while let Some(c) = chars.peek() {
        if c.is_ascii_digit() {
            minor.push(*c);
            chars.next();
        } else {
            break;
        }
    }
    if minor.is_empty() {
        return None;
    }

    Some((major.parse().ok()?, minor.parse().ok()?))
}

fn run_tmux<I, S>(args: I) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    Command::new("tmux")
        .args(args)
        .output()
        .context("failed to run tmux command")
}

/// Probe tmux capabilities used by Batty and choose compatible behavior.
pub fn probe_capabilities() -> Result<TmuxCapabilities> {
    let version_raw = check_tmux()?;
    let version = parse_tmux_version(&version_raw);

    let probe_id = PROBE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let session = format!("batty-cap-probe-{}-{probe_id}", std::process::id());
    let _ = kill_session(&session);
    create_session(&session, "sleep", &["20".to_string()], "/tmp")
        .with_context(|| format!("failed to create tmux probe session '{session}'"))?;
    let pane = pane_id(&session)?;

    let cleanup = || {
        let _ = kill_session(&session);
    };

    let pipe_cmd = "cat >/dev/null";
    let pipe_pane = match run_tmux(["pipe-pane", "-t", pane.as_str(), pipe_cmd]) {
        Ok(out) => out.status.success(),
        Err(_) => false,
    };

    let pipe_pane_only_if_missing =
        match run_tmux(["pipe-pane", "-o", "-t", pane.as_str(), pipe_cmd]) {
            Ok(out) => out.status.success(),
            Err(_) => false,
        };

    // Stop piping in probe session (best-effort).
    let _ = run_tmux(["pipe-pane", "-t", pane.as_str()]);

    let status_style = match run_tmux([
        "set",
        "-t",
        session.as_str(),
        "status-style",
        "bg=colour235,fg=colour136",
    ]) {
        Ok(out) => out.status.success(),
        Err(_) => false,
    };

    let split_lines = match run_tmux([
        "split-window",
        "-v",
        "-l",
        "3",
        "-t",
        session.as_str(),
        "sleep",
        "1",
    ]) {
        Ok(out) => out.status.success(),
        Err(_) => false,
    };
    let split_percent = if split_lines {
        false
    } else {
        match run_tmux([
            "split-window",
            "-v",
            "-p",
            "20",
            "-t",
            session.as_str(),
            "sleep",
            "1",
        ]) {
            Ok(out) => out.status.success(),
            Err(_) => false,
        }
    };

    cleanup();

    let split_mode = if split_lines {
        SplitMode::Lines
    } else if split_percent {
        SplitMode::Percent
    } else {
        SplitMode::Disabled
    };

    Ok(TmuxCapabilities {
        version_raw,
        version,
        pipe_pane,
        pipe_pane_only_if_missing,
        status_style,
        split_mode,
    })
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

/// Check whether a pane currently has an active `pipe-pane` command.
pub fn pane_pipe_enabled(target: &str) -> Result<bool> {
    let output = Command::new("tmux")
        .args(["display-message", "-p", "-t", target, "#{pane_pipe}"])
        .output()
        .with_context(|| format!("failed to query pane_pipe for target '{target}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux display-message pane_pipe failed: {stderr}");
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

/// Get the configured session working directory path.
pub fn session_path(session: &str) -> Result<String> {
    let output = Command::new("tmux")
        .args(["display-message", "-p", "-t", session, "#{session_path}"])
        .output()
        .with_context(|| format!("failed to resolve session path for '{session}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux display-message session_path failed: {stderr}");
    }

    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        bail!("tmux returned empty session path for '{session}'");
    }
    Ok(path)
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

    if let Err(e) = set_mouse(session, true) {
        warn!(
            session = session,
            error = %e,
            "failed to enable tmux mouse mode"
        );
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

/// Set up pipe-pane only if none is configured yet (`tmux pipe-pane -o`).
pub fn setup_pipe_pane_if_missing(target: &str, log_path: &Path) -> Result<()> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create log directory: {}", parent.display()))?;
    }

    let pipe_cmd = format!("cat >> {}", log_path.display());
    let output = Command::new("tmux")
        .args(["pipe-pane", "-o", "-t", target, &pipe_cmd])
        .output()
        .with_context(|| format!("failed to set up pipe-pane (-o) for target '{target}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux pipe-pane -o failed: {stderr}");
    }

    info!(
        target = target,
        log = %log_path.display(),
        "pipe-pane ensured (only-if-missing)"
    );
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
    if !keys.is_empty() {
        // `-l` sends text literally so punctuation/symbols are not interpreted as
        // tmux key names.
        let output = Command::new("tmux")
            .args(["send-keys", "-t", target, "-l", "--", keys])
            .output()
            .with_context(|| format!("failed to send keys to target '{target}'"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("tmux send-keys failed: {stderr}");
        }
    }

    if press_enter {
        // Send Enter as an explicit second action so supervisor injections are
        // always submitted.
        let output = Command::new("tmux")
            .args(["send-keys", "-t", target, "C-m"])
            .output()
            .with_context(|| format!("failed to send Enter to target '{target}'"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("tmux send-keys Enter failed: {stderr}");
        }
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

/// Enable/disable tmux mouse mode for a session.
#[allow(dead_code)]
pub fn set_mouse(session: &str, enabled: bool) -> Result<()> {
    let value = if enabled { "on" } else { "off" };
    tmux_set(session, "mouse", value)
}

fn bind_supervisor_hotkey(session: &str, key: &str, action: &str) -> Result<()> {
    let output = Command::new("tmux")
        .args([
            "bind-key",
            "-T",
            "prefix",
            key,
            "set-option",
            "-t",
            session,
            SUPERVISOR_CONTROL_OPTION,
            action,
        ])
        .output()
        .with_context(|| format!("failed to bind supervisor hotkey '{key}' for '{session}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux bind-key {key} failed: {stderr}");
    }

    Ok(())
}

/// Configure per-session supervisor hotkeys:
/// - `Prefix + Shift+P` -> pause automation
/// - `Prefix + Shift+R` -> resume automation
pub fn configure_supervisor_hotkeys(session: &str) -> Result<()> {
    tmux_set(session, SUPERVISOR_CONTROL_OPTION, "")?;
    bind_supervisor_hotkey(session, "P", "pause")?;
    bind_supervisor_hotkey(session, "R", "resume")?;
    Ok(())
}

/// Read and clear a queued supervisor hotkey action for the session.
///
/// Returns `Some("pause")` / `Some("resume")` when set, or `None` when idle.
pub fn take_supervisor_hotkey_action(session: &str) -> Result<Option<String>> {
    let output = Command::new("tmux")
        .args([
            "show-options",
            "-v",
            "-t",
            session,
            SUPERVISOR_CONTROL_OPTION,
        ])
        .output()
        .with_context(|| {
            format!("failed to read supervisor control option for session '{session}'")
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux show-options supervisor control failed: {stderr}");
    }

    let action = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if action.is_empty() {
        return Ok(None);
    }

    tmux_set(session, SUPERVISOR_CONTROL_OPTION, "")?;
    Ok(Some(action))
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

/// Split the window vertically by fixed line count.
pub fn split_window_vertical_lines(session: &str, lines: u32, command: &[String]) -> Result<()> {
    let mut cmd = Command::new("tmux");
    cmd.args([
        "split-window",
        "-v",
        "-l",
        &lines.to_string(),
        "-t",
        session,
    ]);
    for arg in command {
        cmd.arg(arg);
    }

    let output = cmd
        .output()
        .with_context(|| format!("failed to split window in session '{session}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux split-window -l failed: {stderr}");
    }

    Ok(())
}

/// Split the window vertically by percentage and run command in the new pane.
pub fn split_window_vertical_percent(
    session: &str,
    percent: u32,
    command: &[String],
) -> Result<()> {
    let mut cmd = Command::new("tmux");
    cmd.args([
        "split-window",
        "-v",
        "-p",
        &percent.to_string(),
        "-t",
        session,
    ]);
    for arg in command {
        cmd.arg(arg);
    }

    let output = cmd
        .output()
        .with_context(|| format!("failed to split window in session '{session}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux split-window -p failed: {stderr}");
    }

    Ok(())
}

/// List panes in a session.
///
/// Returns a list of pane IDs (e.g., ["%0", "%1"]).
#[cfg(test)]
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

/// List panes in a session with command/active/dead metadata.
pub fn list_pane_details(session: &str) -> Result<Vec<PaneDetails>> {
    let output = Command::new("tmux")
        .args([
            "list-panes",
            "-t",
            session,
            "-F",
            "#{pane_id}\t#{pane_current_command}\t#{pane_active}\t#{pane_dead}",
        ])
        .output()
        .with_context(|| format!("failed to list pane details for session '{session}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux list-panes details failed: {stderr}");
    }

    let mut panes = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut parts = line.split('\t');
        let Some(id) = parts.next() else { continue };
        let Some(command) = parts.next() else {
            continue;
        };
        let Some(active) = parts.next() else { continue };
        let Some(dead) = parts.next() else { continue };
        panes.push(PaneDetails {
            id: id.to_string(),
            command: command.to_string(),
            active: active == "1",
            dead: dead == "1",
        });
    }

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
    fn parse_tmux_version_supports_minor_suffixes() {
        assert_eq!(parse_tmux_version("tmux 3.4"), Some((3, 4)));
        assert_eq!(parse_tmux_version("tmux 3.3a"), Some((3, 3)));
        assert_eq!(parse_tmux_version("tmux 2.9"), Some((2, 9)));
        assert_eq!(parse_tmux_version("tmux unknown"), None);
    }

    #[test]
    fn capabilities_known_good_matrix() {
        let good = TmuxCapabilities {
            version_raw: "tmux 3.2".to_string(),
            version: Some((3, 2)),
            pipe_pane: true,
            pipe_pane_only_if_missing: true,
            status_style: true,
            split_mode: SplitMode::Lines,
        };
        assert!(good.known_good());

        let fallback = TmuxCapabilities {
            version_raw: "tmux 3.1".to_string(),
            version: Some((3, 1)),
            pipe_pane: true,
            pipe_pane_only_if_missing: false,
            status_style: true,
            split_mode: SplitMode::Percent,
        };
        assert!(!fallback.known_good());
    }

    #[test]
    fn capability_probe_reports_pipe_pane() {
        let caps = probe_capabilities().unwrap();
        assert!(
            caps.pipe_pane,
            "pipe-pane should be available for batty runtime"
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
    fn session_path_returns_working_directory() {
        let session = "batty-test-session-path";
        let _ = kill_session(session);

        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();
        let path = session_path(session).unwrap();
        assert_eq!(path, "/tmp");

        kill_session(session).unwrap();
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
    fn create_session_enables_mouse_mode() {
        let session = "batty-test-mouse";
        let _ = kill_session(session);

        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        let output = Command::new("tmux")
            .args(["show-options", "-t", session, "-v", "mouse"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        assert_eq!(value, "on", "expected tmux mouse mode to be enabled");

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
    fn send_keys_with_enter_submits_line() {
        let session = "batty-test-sendkeys-enter";
        let _ = kill_session(session);

        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("sendkeys.log");

        create_session(session, "cat", &[], "/tmp").unwrap();
        setup_pipe_pane(session, &log_path).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));

        send_keys(session, "supervisor ping", true).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));

        let content = std::fs::read_to_string(&log_path).unwrap_or_default();
        assert!(
            content.contains("supervisor ping"),
            "expected injected text in pane log, got: {content:?}"
        );
        assert!(
            content.contains("supervisor ping\r\n") || content.contains("supervisor ping\n"),
            "expected submitted line ending in pane log, got: {content:?}"
        );

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
    fn list_pane_details_includes_active_flag() {
        let session = "batty-test-pane-details";
        let _ = kill_session(session);

        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        let panes = list_pane_details(session).unwrap();
        assert!(
            !panes.is_empty(),
            "expected at least one pane detail record"
        );
        assert!(
            panes.iter().any(|p| p.active),
            "expected one active pane, got: {panes:?}"
        );

        kill_session(session).unwrap();
    }

    #[test]
    fn configure_supervisor_hotkeys_initializes_control_option() {
        let session = "batty-test-supervisor-hotkeys";
        let _ = kill_session(session);

        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();
        configure_supervisor_hotkeys(session).unwrap();

        let output = Command::new("tmux")
            .args([
                "show-options",
                "-v",
                "-t",
                session,
                SUPERVISOR_CONTROL_OPTION,
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).trim().is_empty());

        kill_session(session).unwrap();
    }

    #[test]
    fn take_supervisor_hotkey_action_reads_and_clears() {
        let session = "batty-test-supervisor-action";
        let _ = kill_session(session);

        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();
        configure_supervisor_hotkeys(session).unwrap();
        tmux_set(session, SUPERVISOR_CONTROL_OPTION, "pause").unwrap();

        let first = take_supervisor_hotkey_action(session).unwrap();
        assert_eq!(first.as_deref(), Some("pause"));

        let second = take_supervisor_hotkey_action(session).unwrap();
        assert!(second.is_none(), "expected action to be cleared");

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
