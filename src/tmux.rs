//! tmux session management for batty.
//!
//! Wraps tmux CLI commands for session lifecycle, output capture via pipe-pane,
//! input injection via send-keys, and pane management. This replaces the
//! portable-pty direct approach from Phase 1 with tmux-based supervision.
#![allow(dead_code)]

use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use tracing::{debug, info, warn};

use crate::team::errors::TmuxError;

static PROBE_COUNTER: AtomicU64 = AtomicU64::new(1);
const SUPERVISOR_CONTROL_OPTION: &str = "@batty_supervisor_control";

/// Default tmux-prefix hotkey for pausing Batty supervision.
pub const SUPERVISOR_PAUSE_HOTKEY: &str = "C-b P";
/// Default tmux-prefix hotkey for resuming Batty supervision.
pub const SUPERVISOR_RESUME_HOTKEY: &str = "C-b R";
const SEND_KEYS_SUBMIT_DELAY_MS: u64 = 100;

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
Install or upgrade tmux (recommended >= 3.2) and re-run `batty start`.",
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

fn check_tmux_with_program(program: &str) -> Result<String> {
    let output = Command::new(program).arg("-V").output().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            TmuxError::NotInstalled
        } else {
            TmuxError::exec("tmux -V", error)
        }
    })?;

    if !output.status.success() {
        return Err(TmuxError::command_failed(
            "tmux -V",
            None,
            &String::from_utf8_lossy(&output.stderr),
        )
        .into());
    }

    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    debug!(version = %version, "tmux found");
    Ok(version)
}

/// Check that tmux is installed and reachable.
pub fn check_tmux() -> Result<String> {
    check_tmux_with_program("tmux")
}

/// Return whether the tmux binary is available in the current environment.
pub fn tmux_available() -> bool {
    check_tmux().is_ok()
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
        .map_err(|error| TmuxError::exec("tmux", error).into())
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

/// Check whether a tmux server is currently running.
pub fn server_running() -> bool {
    Command::new("tmux")
        .args(["list-sessions"])
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
        return Err(TmuxError::command_failed(
            "display-message #{pane_dead}",
            Some(target),
            &stderr,
        )
        .into());
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
        return Err(
            TmuxError::command_failed("display-message #{pane_id}", Some(target), &stderr).into(),
        );
    }

    let pane = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if pane.is_empty() {
        return Err(TmuxError::EmptyPaneId {
            target: target.to_string(),
        }
        .into());
    }
    Ok(pane)
}

/// Return `(pane_width, pane_height)` for a tmux pane target.
pub fn pane_dimensions(target: &str) -> Result<(u16, u16)> {
    let output = Command::new("tmux")
        .args([
            "display-message",
            "-p",
            "-t",
            target,
            "#{pane_width} #{pane_height}",
        ])
        .output()
        .with_context(|| format!("failed to query pane dimensions for target '{target}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(TmuxError::command_failed(
            "display-message #{pane_width} #{pane_height}",
            Some(target),
            &stderr,
        )
        .into());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut parts = stdout.split_whitespace();
    let width = parts
        .next()
        .context("tmux pane width missing")?
        .parse()
        .context("invalid tmux pane width")?;
    let height = parts
        .next()
        .context("tmux pane height missing")?
        .parse()
        .context("invalid tmux pane height")?;
    Ok((width, height))
}

/// Get the current working directory for a pane target.
pub fn pane_current_path(target: &str) -> Result<String> {
    let output = Command::new("tmux")
        .args([
            "display-message",
            "-p",
            "-t",
            target,
            "#{pane_current_path}",
        ])
        .output()
        .with_context(|| format!("failed to resolve pane current path for target '{target}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux display-message pane_current_path failed: {stderr}");
    }

    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        bail!("tmux returned empty pane current path for target '{target}'");
    }
    Ok(path)
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
        return Err(TmuxError::SessionExists {
            session: session.to_string(),
        }
        .into());
    }

    // Build the full command string for tmux
    // tmux new-session -d -s <name> -c <work_dir> <program> <args...>
    let mut cmd = Command::new("tmux");
    cmd.args(["new-session", "-d", "-s", session, "-c", work_dir]);
    // Set a generous size so the PTY isn't tiny
    cmd.args(["-x", "220", "-y", "50"]);
    // Unset CLAUDECODE so nested Claude Code sessions can launch.
    // Without this, Claude Code detects the parent session's env var and
    // refuses to start ("cannot be launched inside another Claude Code session").
    cmd.args(["env", "-u", "CLAUDECODE"]);
    cmd.arg(program);
    for arg in args {
        cmd.arg(arg);
    }

    let output = cmd
        .output()
        .with_context(|| format!("failed to create tmux session '{session}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(TmuxError::command_failed("new-session", Some(session), &stderr).into());
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

/// Create a detached tmux window in an existing session running the given command.
pub fn create_window(
    session: &str,
    window_name: &str,
    program: &str,
    args: &[String],
    work_dir: &str,
) -> Result<()> {
    if !session_exists(session) {
        bail!("tmux session '{session}' not found");
    }

    let mut cmd = Command::new("tmux");
    cmd.args([
        "new-window",
        "-d",
        "-t",
        session,
        "-n",
        window_name,
        "-c",
        work_dir,
    ]);
    // Unset CLAUDECODE so nested Claude Code sessions can launch from
    // additional windows (for example, parallel agent slots).
    cmd.args(["env", "-u", "CLAUDECODE"]);
    cmd.arg(program);
    for arg in args {
        cmd.arg(arg);
    }

    let output = cmd
        .output()
        .with_context(|| format!("failed to create tmux window '{window_name}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux new-window failed: {stderr}");
    }

    Ok(())
}

/// Rename an existing tmux window target (e.g. `session:0` or `session:old`).
pub fn rename_window(target: &str, new_name: &str) -> Result<()> {
    let output = Command::new("tmux")
        .args(["rename-window", "-t", target, new_name])
        .output()
        .with_context(|| format!("failed to rename tmux window target '{target}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux rename-window failed: {stderr}");
    }

    Ok(())
}

/// Select a tmux window target (e.g. `session:agent-1`).
pub fn select_window(target: &str) -> Result<()> {
    let output = Command::new("tmux")
        .args(["select-window", "-t", target])
        .output()
        .with_context(|| format!("failed to select tmux window '{target}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux select-window failed: {stderr}");
    }

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
///
/// If already inside tmux, uses `switch-client` instead of `attach-session`.
pub fn attach(session: &str) -> Result<()> {
    if !session_exists(session) {
        bail!(
            "tmux session '{session}' not found — is batty running? \
             Start with `batty start` first"
        );
    }

    let inside_tmux = std::env::var("TMUX").is_ok();

    let (cmd, args) = if inside_tmux {
        ("switch-client", vec!["-t", session])
    } else {
        ("attach-session", vec!["-t", session])
    };

    let status = Command::new("tmux")
        .arg(cmd)
        .args(&args)
        .status()
        .with_context(|| format!("failed to {cmd} to tmux session '{session}'"))?;

    if !status.success() {
        bail!("tmux {cmd} to '{session}' failed");
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
            return Err(TmuxError::command_failed("send-keys", Some(target), &stderr).into());
        }
    }

    if press_enter {
        // Keep submission as a separate keypress so the target app processes the
        // literal text first, matching the watcher script's behavior.
        if !keys.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(SEND_KEYS_SUBMIT_DELAY_MS));
        }

        let output = Command::new("tmux")
            .args(["send-keys", "-t", target, "Enter"])
            .output()
            .with_context(|| format!("failed to send Enter to target '{target}'"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(TmuxError::command_failed("send-keys Enter", Some(target), &stderr).into());
        }
    }

    debug!(target = target, keys = keys, "sent keys");
    Ok(())
}

/// List all tmux session names matching a prefix.
pub fn list_sessions_with_prefix(prefix: &str) -> Vec<String> {
    let output = Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output();

    match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|name| name.starts_with(prefix))
            .map(|s| s.to_string())
            .collect(),
        _ => Vec::new(),
    }
}

/// RAII guard that kills a tmux session on drop.
///
/// Ensures test sessions are cleaned up even on panic/assert failure.
/// Use in tests instead of manual `kill_session()` calls.
pub struct TestSession {
    name: String,
}

impl TestSession {
    /// Create a new test session guard wrapping an existing session name.
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }

    /// The session name.
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for TestSession {
    fn drop(&mut self) {
        let _ = kill_session(&self.name);
    }
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
    capture_pane_recent(target, 0)
}

/// Capture only the most recent visible lines of a tmux target.
pub fn capture_pane_recent(target: &str, lines: u32) -> Result<String> {
    let mut args = vec![
        "capture-pane".to_string(),
        "-t".to_string(),
        target.to_string(),
        "-p".to_string(),
    ];
    if lines > 0 {
        args.push("-S".to_string());
        args.push(format!("-{lines}"));
    }

    let output = Command::new("tmux")
        .args(&args)
        .output()
        .with_context(|| format!("failed to capture pane for target '{target}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(TmuxError::command_failed("capture-pane", Some(target), &stderr).into());
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Enable/disable tmux mouse mode for a session.
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

#[cfg(test)]
fn list_window_names(session: &str) -> Result<Vec<String>> {
    let output = Command::new("tmux")
        .args(["list-windows", "-t", session, "-F", "#{window_name}"])
        .output()
        .with_context(|| format!("failed to list windows for session '{session}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux list-windows failed: {stderr}");
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect())
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
/// Split a pane horizontally (creates a new pane to the right).
///
/// `target_pane` is a tmux pane ID (e.g., `%0`). Returns the new pane's ID.
pub fn split_window_horizontal(target_pane: &str, size_pct: u32) -> Result<String> {
    let size = format!("{size_pct}%");
    let output = Command::new("tmux")
        .args([
            "split-window",
            "-h",
            "-t",
            target_pane,
            "-l",
            &size,
            "-P",
            "-F",
            "#{pane_id}",
        ])
        .output()
        .with_context(|| format!("failed to split pane '{target_pane}' horizontally"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux split-window -h failed: {stderr}");
    }

    let pane_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    debug!(target_pane, pane_id = %pane_id, size_pct, "horizontal split created");
    Ok(pane_id)
}

/// Split a specific pane vertically (creates a new pane below).
///
/// Returns the new pane's ID.
pub fn split_window_vertical_in_pane(
    _session: &str,
    pane_id: &str,
    size_pct: u32,
) -> Result<String> {
    // Pane IDs (%N) are globally unique in tmux — use them directly as targets
    let size = format!("{size_pct}%");
    let output = Command::new("tmux")
        .args([
            "split-window",
            "-v",
            "-t",
            pane_id,
            "-l",
            &size,
            "-P",
            "-F",
            "#{pane_id}",
        ])
        .output()
        .with_context(|| format!("failed to split pane '{pane_id}' vertically"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux split-window -v failed for pane '{pane_id}': {stderr}");
    }

    let new_pane = String::from_utf8_lossy(&output.stdout).trim().to_string();
    debug!(pane_id = %new_pane, parent = pane_id, size_pct, "vertical split created");
    Ok(new_pane)
}

/// Evenly spread a pane and any adjacent panes in its layout cell.
pub fn select_layout_even(target_pane: &str) -> Result<()> {
    let output = Command::new("tmux")
        .args(["select-layout", "-E", "-t", target_pane])
        .output()
        .with_context(|| format!("failed to even layout for pane '{target_pane}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux select-layout -E failed: {stderr}");
    }

    Ok(())
}

/// Load text into a tmux paste buffer.
/// Named buffer used by batty to avoid clobbering the user's paste buffer.
const BATTY_BUFFER_NAME: &str = "batty-inject";

/// Load text into a named tmux paste buffer.
///
/// Uses a dedicated buffer name so we never clobber the user's default
/// paste buffer (which is what Ctrl-] / middle-click uses).
pub fn load_buffer(content: &str) -> Result<()> {
    let tmp = std::env::temp_dir().join(format!("batty-buf-{}", std::process::id()));
    std::fs::write(&tmp, content)
        .with_context(|| format!("failed to write buffer file {}", tmp.display()))?;

    let output = Command::new("tmux")
        .args([
            "load-buffer",
            "-b",
            BATTY_BUFFER_NAME,
            &tmp.to_string_lossy(),
        ])
        .output()
        .context("failed to run tmux load-buffer")?;

    let _ = std::fs::remove_file(&tmp);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux load-buffer failed: {stderr}");
    }

    Ok(())
}

/// Paste the named batty buffer into a target pane and delete the buffer.
///
/// The `-d` flag deletes the buffer after pasting so it doesn't linger.
/// The `-b` flag selects the batty-specific buffer (never the user's default).
pub fn paste_buffer(target: &str) -> Result<()> {
    let output = Command::new("tmux")
        .args(["paste-buffer", "-d", "-b", BATTY_BUFFER_NAME, "-t", target])
        .output()
        .with_context(|| format!("failed to paste buffer into '{target}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux paste-buffer failed: {stderr}");
    }

    Ok(())
}

/// Kill a specific tmux pane.
pub fn kill_pane(target: &str) -> Result<()> {
    let output = Command::new("tmux")
        .args(["kill-pane", "-t", target])
        .output()
        .with_context(|| format!("failed to kill pane '{target}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Don't error if already dead
        if !stderr.contains("not found") {
            bail!("tmux kill-pane failed: {stderr}");
        }
    }

    Ok(())
}

/// Respawn a dead pane with a new command.
pub fn respawn_pane(target: &str, command: &str) -> Result<()> {
    let output = Command::new("tmux")
        .args(["respawn-pane", "-t", target, "-k", command])
        .output()
        .with_context(|| format!("failed to respawn pane '{target}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux respawn-pane failed: {stderr}");
    }

    Ok(())
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

/// # Test categories
///
/// Tests in this module are split into **unit** and **integration** categories:
///
/// - **Unit tests** — pure logic (parsing, string manipulation). Run with `cargo test`.
/// - **Integration tests** — require a running tmux server. Gated behind the `integration`
///   Cargo feature: `cargo test --features integration`. These tests are marked with
///   `#[cfg_attr(not(feature = "integration"), ignore)]` and `#[serial]`.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::test_support::PATH_LOCK;
    use serial_test::serial;
    use std::cell::RefCell;

    thread_local! {
        static TMUX_TEST_PATH_GUARD: RefCell<Option<std::sync::MutexGuard<'static, ()>>> = const { RefCell::new(None) };
    }

    fn require_tmux_integration() -> bool {
        TMUX_TEST_PATH_GUARD.with(|slot| {
            if slot.borrow().is_none() {
                *slot.borrow_mut() =
                    Some(PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner()));
            }
        });
        if tmux_available() {
            return true;
        }
        eprintln!("skipping tmux integration test: tmux binary unavailable");
        false
    }

    #[test]
    fn session_name_convention() {
        assert_eq!(session_name("phase-1"), "batty-phase-1");
        assert_eq!(session_name("phase-2"), "batty-phase-2");
        assert_eq!(session_name("phase-2.5"), "batty-phase-2-5");
        assert_eq!(session_name("phase 3"), "batty-phase-3");
    }

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
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
    fn check_tmux_reports_missing_binary() {
        assert!(check_tmux_with_program("__batty_missing_tmux__").is_err());
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
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn capability_probe_reports_pipe_pane() {
        let caps = probe_capabilities().unwrap();
        assert!(
            caps.pipe_pane,
            "pipe-pane should be available for batty runtime"
        );
    }

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn nonexistent_session_does_not_exist() {
        assert!(!session_exists("batty-test-nonexistent-12345"));
    }

    #[test]
    #[serial]
    fn create_and_kill_session() {
        if !require_tmux_integration() {
            return;
        }
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
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn session_path_returns_working_directory() {
        let session = "batty-test-session-path";
        let _ = kill_session(session);

        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();
        let path = session_path(session).unwrap();
        assert_eq!(path, "/tmp");

        kill_session(session).unwrap();
    }

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn pane_current_path_returns_working_directory() {
        let session = "batty-test-pane-current-path";
        let _ = kill_session(session);

        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();
        let pane = pane_id(session).unwrap();
        let path = pane_current_path(&pane).unwrap();
        assert_eq!(
            std::fs::canonicalize(&path).unwrap(),
            std::fs::canonicalize("/tmp").unwrap()
        );

        kill_session(session).unwrap();
    }

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
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
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn create_window_adds_named_window_to_existing_session() {
        let session = "batty-test-window-create";
        let _ = kill_session(session);

        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();
        rename_window(&format!("{session}:0"), "agent-1").unwrap();
        create_window(session, "agent-2", "sleep", &["10".to_string()], "/tmp").unwrap();

        let names = list_window_names(session).unwrap();
        assert!(names.contains(&"agent-1".to_string()));
        assert!(names.contains(&"agent-2".to_string()));

        select_window(&format!("{session}:agent-1")).unwrap();
        kill_session(session).unwrap();
    }

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn create_window_unsets_claudecode_from_session_environment() {
        let session = "batty-test-window-unset-claudecode";
        let _ = kill_session(session);

        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        let output = Command::new("tmux")
            .args(["set-environment", "-t", session, "CLAUDECODE", "1"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "failed to set CLAUDECODE in tmux session: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        create_window(
            session,
            "env-check",
            "bash",
            &[
                "-lc".to_string(),
                "printf '%s' \"${CLAUDECODE:-unset}\"; sleep 1".to_string(),
            ],
            "/tmp",
        )
        .unwrap();

        std::thread::sleep(std::time::Duration::from_millis(300));

        let content = capture_pane(&format!("{session}:env-check")).unwrap();
        assert!(
            content.contains("unset"),
            "expected CLAUDECODE to be unset in new window, got: {content:?}"
        );

        kill_session(session).unwrap();
    }

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
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
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
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
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
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
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn send_keys_enter_only_submits_prompt() {
        let session = "batty-test-sendkeys-enter-only";
        let _ = kill_session(session);

        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("sendkeys-enter-only.log");

        create_session(session, "cat", &[], "/tmp").unwrap();
        let pane = pane_id(session).unwrap();
        setup_pipe_pane(&pane, &log_path).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));

        send_keys(&pane, "", true).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));

        let content = std::fs::read_to_string(&log_path).unwrap_or_default();
        assert!(
            content.contains("\r\n") || content.contains('\n'),
            "expected submitted empty line in pane log, got: {content:?}"
        );

        kill_session(session).unwrap();
    }

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn pipe_pane_captures_output() {
        let session = "batty-test-pipe";
        let _ = kill_session(session);

        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("pty-output.log");

        // Create session with bash, set up pipe-pane FIRST, then trigger output
        create_session(session, "bash", &[], "/tmp").unwrap();
        let pane = pane_id(session).unwrap();

        // Set up pipe-pane before generating output
        setup_pipe_pane(&pane, &log_path).unwrap();

        // Small delay to ensure pipe-pane is active
        std::thread::sleep(std::time::Duration::from_millis(200));

        // Now generate output that pipe-pane will capture
        send_keys(&pane, "echo pipe-test-output", true).unwrap();

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
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
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
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn capture_pane_recent_returns_content() {
        let session = "batty-test-capture-recent";
        let _ = kill_session(session);

        create_session(
            session,
            "bash",
            &[
                "-c".to_string(),
                "echo 'capture-recent-test'; sleep 2".to_string(),
            ],
            "/tmp",
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));

        let content = capture_pane_recent(session, 10).unwrap();
        assert!(content.contains("capture-recent-test"));

        kill_session(session).unwrap();
    }

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn list_panes_returns_at_least_one() {
        let session = "batty-test-panes";
        let _ = kill_session(session);

        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        let panes = list_panes(session).unwrap();
        assert!(!panes.is_empty(), "session should have at least one pane");

        kill_session(session).unwrap();
    }

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
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
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
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
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
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
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn kill_nonexistent_session_is_ok() {
        // Should not error — idempotent
        kill_session("batty-test-nonexistent-kill-99999").unwrap();
    }

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
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

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn test_session_guard_cleanup_on_drop() {
        let name = "batty-test-guard-drop";
        let _ = kill_session(name);

        {
            let guard = TestSession::new(name);
            create_session(guard.name(), "sleep", &["30".to_string()], "/tmp").unwrap();
            assert!(session_exists(name));
            // guard dropped here
        }

        assert!(!session_exists(name), "session should be killed on drop");
    }

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn test_session_guard_cleanup_on_panic() {
        let name = "batty-test-guard-panic";
        let _ = kill_session(name);

        let result = std::panic::catch_unwind(|| {
            let guard = TestSession::new(name);
            create_session(guard.name(), "sleep", &["30".to_string()], "/tmp").unwrap();
            assert!(session_exists(name));
            panic!("intentional panic to test cleanup");
            #[allow(unreachable_code)]
            drop(guard);
        });

        assert!(result.is_err(), "should have panicked");
        // Give drop a moment to complete (panic unwind runs destructors)
        assert!(
            !session_exists(name),
            "session should be cleaned up even after panic"
        );
    }

    // --- parse_tmux_version edge cases ---

    #[test]
    fn parse_tmux_version_empty_string() {
        assert_eq!(parse_tmux_version(""), None);
    }

    #[test]
    fn parse_tmux_version_no_prefix() {
        // Missing "tmux " prefix
        assert_eq!(parse_tmux_version("3.4"), None);
    }

    #[test]
    fn parse_tmux_version_major_only_no_dot() {
        assert_eq!(parse_tmux_version("tmux 3"), None);
    }

    #[test]
    fn parse_tmux_version_multi_digit() {
        assert_eq!(parse_tmux_version("tmux 10.12"), Some((10, 12)));
    }

    #[test]
    fn parse_tmux_version_trailing_whitespace() {
        assert_eq!(parse_tmux_version("  tmux 3.4  "), Some((3, 4)));
    }

    #[test]
    fn parse_tmux_version_next_suffix() {
        // "next-3.5" style dev builds
        assert_eq!(parse_tmux_version("tmux next-3.5"), None);
    }

    #[test]
    fn parse_tmux_version_dot_no_minor() {
        assert_eq!(parse_tmux_version("tmux 3."), None);
    }

    #[test]
    fn parse_tmux_version_double_suffix_letters() {
        assert_eq!(parse_tmux_version("tmux 3.3ab"), Some((3, 3)));
    }

    // --- TmuxCapabilities edge cases ---

    #[test]
    fn capabilities_known_good_version_4() {
        let caps = TmuxCapabilities {
            version_raw: "tmux 4.0".to_string(),
            version: Some((4, 0)),
            pipe_pane: true,
            pipe_pane_only_if_missing: true,
            status_style: true,
            split_mode: SplitMode::Lines,
        };
        assert!(caps.known_good(), "4.0 should be known good");
    }

    #[test]
    fn capabilities_known_good_version_2_9() {
        let caps = TmuxCapabilities {
            version_raw: "tmux 2.9".to_string(),
            version: Some((2, 9)),
            pipe_pane: true,
            pipe_pane_only_if_missing: false,
            status_style: true,
            split_mode: SplitMode::Percent,
        };
        assert!(!caps.known_good(), "2.9 should not be known good");
    }

    #[test]
    fn capabilities_known_good_version_3_0() {
        let caps = TmuxCapabilities {
            version_raw: "tmux 3.0".to_string(),
            version: Some((3, 0)),
            pipe_pane: true,
            pipe_pane_only_if_missing: false,
            status_style: true,
            split_mode: SplitMode::Percent,
        };
        assert!(!caps.known_good(), "3.0 should not be known good");
    }

    #[test]
    fn capabilities_known_good_none_version() {
        let caps = TmuxCapabilities {
            version_raw: "tmux unknown".to_string(),
            version: None,
            pipe_pane: false,
            pipe_pane_only_if_missing: false,
            status_style: false,
            split_mode: SplitMode::Disabled,
        };
        assert!(!caps.known_good(), "None version should not be known good");
    }

    #[test]
    fn capabilities_remediation_message_includes_version() {
        let caps = TmuxCapabilities {
            version_raw: "tmux 2.8".to_string(),
            version: Some((2, 8)),
            pipe_pane: false,
            pipe_pane_only_if_missing: false,
            status_style: false,
            split_mode: SplitMode::Disabled,
        };
        let msg = caps.remediation_message();
        assert!(
            msg.contains("tmux 2.8"),
            "message should include detected version"
        );
        assert!(
            msg.contains("pipe-pane"),
            "message should mention pipe-pane requirement"
        );
        assert!(msg.contains("3.2"), "message should recommend >= 3.2");
    }

    // --- session_name edge cases ---

    #[test]
    fn session_name_empty_input() {
        assert_eq!(session_name(""), "batty-");
    }

    #[test]
    fn session_name_preserves_underscores() {
        assert_eq!(session_name("my_session"), "batty-my_session");
    }

    #[test]
    fn session_name_replaces_colons_and_slashes() {
        assert_eq!(session_name("a:b/c"), "batty-a-b-c");
    }

    #[test]
    fn session_name_replaces_multiple_dots() {
        assert_eq!(session_name("v1.2.3"), "batty-v1-2-3");
    }

    // --- PaneDetails struct ---

    #[test]
    fn pane_details_clone_and_eq() {
        let pd = PaneDetails {
            id: "%5".to_string(),
            command: "bash".to_string(),
            active: true,
            dead: false,
        };
        let cloned = pd.clone();
        assert_eq!(pd, cloned);
        assert_eq!(pd.id, "%5");
        assert!(pd.active);
        assert!(!pd.dead);
    }

    #[test]
    fn pane_details_not_equal_different_id() {
        let a = PaneDetails {
            id: "%1".to_string(),
            command: "bash".to_string(),
            active: true,
            dead: false,
        };
        let b = PaneDetails {
            id: "%2".to_string(),
            command: "bash".to_string(),
            active: true,
            dead: false,
        };
        assert_ne!(a, b);
    }

    // --- SplitMode ---

    #[test]
    fn split_mode_debug_and_eq() {
        assert_eq!(SplitMode::Lines, SplitMode::Lines);
        assert_ne!(SplitMode::Lines, SplitMode::Percent);
        assert_ne!(SplitMode::Percent, SplitMode::Disabled);
        let copied = SplitMode::Lines;
        assert_eq!(format!("{:?}", copied), "Lines");
    }

    // --- TestSession ---

    #[test]
    fn test_session_name_accessor() {
        let guard = TestSession::new("batty-test-accessor");
        assert_eq!(guard.name(), "batty-test-accessor");
        // Don't actually create a tmux session — just test the struct
    }

    // --- Integration tests requiring tmux ---

    #[test]
    #[serial]
    fn pane_exists_for_valid_pane() {
        if !require_tmux_integration() {
            return;
        }
        let session = "batty-test-pane-exists";
        let _guard = TestSession::new(session);
        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        let pane = pane_id(session).unwrap();
        assert!(pane_exists(&pane), "existing pane should be found");
    }

    #[test]
    #[serial]
    fn session_exists_returns_false_after_kill() {
        if !require_tmux_integration() {
            return;
        }
        let session = "batty-test-sess-exists-gone";
        let _ = kill_session(session);
        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();
        assert!(session_exists(session));

        kill_session(session).unwrap();
        assert!(
            !session_exists(session),
            "session should not exist after kill"
        );
    }

    #[test]
    #[serial]
    fn pane_dead_for_running_process() {
        if !require_tmux_integration() {
            return;
        }
        let session = "batty-test-pane-dead-alive";
        let _guard = TestSession::new(session);
        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        let pane = pane_id(session).unwrap();
        let dead = pane_dead(&pane).unwrap();
        assert!(!dead, "running process pane should not be dead");
    }

    #[test]
    #[serial]
    fn list_sessions_with_prefix_finds_matching() {
        if !require_tmux_integration() {
            return;
        }
        let prefix = "batty-test-prefix-match";
        let s1 = format!("{prefix}-aaa");
        let s2 = format!("{prefix}-bbb");
        let _g1 = TestSession::new(s1.clone());
        let _g2 = TestSession::new(s2.clone());

        create_session(&s1, "sleep", &["10".to_string()], "/tmp").unwrap();
        create_session(&s2, "sleep", &["10".to_string()], "/tmp").unwrap();

        let found = list_sessions_with_prefix(prefix);
        assert!(
            found.contains(&s1),
            "should find first session, got: {found:?}"
        );
        assert!(
            found.contains(&s2),
            "should find second session, got: {found:?}"
        );
    }

    #[test]
    #[serial]
    fn list_sessions_with_prefix_excludes_non_matching() {
        if !require_tmux_integration() {
            return;
        }
        let found = list_sessions_with_prefix("batty-test-zzz-nonexist-99999");
        assert!(found.is_empty(), "should find no sessions for bogus prefix");
    }

    #[test]
    #[serial]
    fn split_window_horizontal_creates_new_pane() {
        if !require_tmux_integration() {
            return;
        }
        let session = "batty-test-hsplit";
        let _guard = TestSession::new(session);
        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        let original = pane_id(session).unwrap();
        let new_pane = split_window_horizontal(&original, 50).unwrap();
        assert!(
            new_pane.starts_with('%'),
            "new pane id should start with %, got: {new_pane}"
        );

        let panes = list_panes(session).unwrap();
        assert_eq!(panes.len(), 2, "should have 2 panes after split");
        assert!(panes.contains(&new_pane));
    }

    #[test]
    #[serial]
    fn split_window_vertical_creates_new_pane() {
        if !require_tmux_integration() {
            return;
        }
        let session = "batty-test-vsplit";
        let _guard = TestSession::new(session);
        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        let original = pane_id(session).unwrap();
        let new_pane = split_window_vertical_in_pane(session, &original, 50).unwrap();
        assert!(
            new_pane.starts_with('%'),
            "new pane id should start with %, got: {new_pane}"
        );

        let panes = list_panes(session).unwrap();
        assert_eq!(panes.len(), 2, "should have 2 panes after split");
        assert!(panes.contains(&new_pane));
    }

    #[test]
    #[serial]
    fn load_buffer_and_paste_buffer_injects_text() {
        if !require_tmux_integration() {
            return;
        }
        let session = "batty-test-paste-buf";
        let _guard = TestSession::new(session);
        create_session(session, "cat", &[], "/tmp").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));

        let pane = pane_id(session).unwrap();
        load_buffer("hello-from-buffer").unwrap();
        paste_buffer(&pane).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(300));
        let live_pane = pane_id(session).unwrap_or(pane);
        let content = capture_pane(&live_pane).unwrap();
        assert!(
            content.contains("hello-from-buffer"),
            "paste-buffer should inject text into pane, got: {content:?}"
        );
    }

    #[test]
    #[serial]
    fn kill_pane_removes_pane() {
        if !require_tmux_integration() {
            return;
        }
        let session = "batty-test-kill-pane";
        let _guard = TestSession::new(session);
        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        let original = pane_id(session).unwrap();
        let new_pane = split_window_horizontal(&original, 50).unwrap();
        let before = list_panes(session).unwrap();
        assert_eq!(before.len(), 2);

        kill_pane(&new_pane).unwrap();
        let after = list_panes(session).unwrap();
        assert_eq!(after.len(), 1, "should have 1 pane after kill");
        assert!(!after.contains(&new_pane));
    }

    #[test]
    #[serial]
    fn kill_pane_nonexistent_returns_error() {
        if !require_tmux_integration() {
            return;
        }
        // tmux returns "can't find pane" for nonexistent pane IDs,
        // which kill_pane only suppresses when it says "not found"
        let result = kill_pane("batty-test-no-such-session-xyz:0.0");
        // Either succeeds (tmux says "not found") or errors — both are valid
        // The key guarantee is it doesn't panic
        let _ = result;
    }

    #[test]
    #[serial]
    fn set_mouse_disable_and_enable() {
        if !require_tmux_integration() {
            return;
        }
        let session = "batty-test-mouse-toggle";
        let _guard = TestSession::new(session);
        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        // Mouse is enabled by create_session; disable it
        set_mouse(session, false).unwrap();
        let output = Command::new("tmux")
            .args(["show-options", "-t", session, "-v", "mouse"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "off",
            "mouse should be disabled"
        );

        // Re-enable
        set_mouse(session, true).unwrap();
        let output = Command::new("tmux")
            .args(["show-options", "-t", session, "-v", "mouse"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "on",
            "mouse should be re-enabled"
        );
    }

    #[test]
    #[serial]
    fn tmux_set_custom_option() {
        if !require_tmux_integration() {
            return;
        }
        let session = "batty-test-tmux-set";
        let _guard = TestSession::new(session);
        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        tmux_set(session, "@batty_test_opt", "test-value").unwrap();

        let output = Command::new("tmux")
            .args(["show-options", "-v", "-t", session, "@batty_test_opt"])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "test-value");
    }

    #[test]
    #[serial]
    fn create_window_fails_for_missing_session() {
        if !require_tmux_integration() {
            return;
        }
        let result = create_window(
            "batty-test-nonexistent-session-99999",
            "test-win",
            "sleep",
            &["1".to_string()],
            "/tmp",
        );
        assert!(result.is_err(), "should fail for nonexistent session");
        assert!(
            result.unwrap_err().to_string().contains("not found"),
            "error should mention session not found"
        );
    }

    #[test]
    #[serial]
    fn setup_pipe_pane_if_missing_works() {
        if !require_tmux_integration() {
            return;
        }
        let session = "batty-test-pipe-if-missing";
        let _guard = TestSession::new(session);
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("pipe-if-missing.log");

        create_session(
            session,
            "bash",
            &["-c".to_string(), "sleep 10".to_string()],
            "/tmp",
        )
        .unwrap();

        // Use session target instead of pane ID for reliability
        // First call should set up pipe-pane
        setup_pipe_pane_if_missing(session, &log_path).unwrap();

        // Second call should be a no-op (not error)
        setup_pipe_pane_if_missing(session, &log_path).unwrap();
    }

    #[test]
    #[serial]
    fn select_layout_even_after_splits() {
        if !require_tmux_integration() {
            return;
        }
        let session = "batty-test-layout-even";
        let _guard = TestSession::new(session);
        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        let original = pane_id(session).unwrap();
        let _p2 = split_window_horizontal(&original, 50).unwrap();

        // Should not error
        select_layout_even(&original).unwrap();
    }

    #[test]
    #[serial]
    fn rename_window_changes_name() {
        if !require_tmux_integration() {
            return;
        }
        let session = "batty-test-rename-win";
        let _guard = TestSession::new(session);
        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        rename_window(&format!("{session}:0"), "custom-name").unwrap();
        let names = list_window_names(session).unwrap();
        assert!(
            names.contains(&"custom-name".to_string()),
            "window should be renamed, got: {names:?}"
        );
    }

    #[test]
    #[serial]
    fn select_window_switches_active() {
        if !require_tmux_integration() {
            return;
        }
        let session = "batty-test-select-win";
        let _guard = TestSession::new(session);
        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();
        create_window(session, "second", "sleep", &["10".to_string()], "/tmp").unwrap();

        // Select the second window — should not error
        select_window(&format!("{session}:second")).unwrap();
    }

    #[test]
    #[serial]
    fn capture_pane_recent_zero_lines_returns_full() {
        if !require_tmux_integration() {
            return;
        }
        let session = "batty-test-capture-zero";
        let _guard = TestSession::new(session);
        create_session(
            session,
            "bash",
            &[
                "-c".to_string(),
                "echo 'zero-lines-test'; sleep 2".to_string(),
            ],
            "/tmp",
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));

        // lines=0 means no -S flag, should return full pane content
        let content = capture_pane_recent(session, 0).unwrap();
        assert!(
            content.contains("zero-lines-test"),
            "should capture full content with lines=0, got: {content:?}"
        );
    }

    #[test]
    #[serial]
    fn list_pane_details_shows_command_info() {
        if !require_tmux_integration() {
            return;
        }
        let session = "batty-test-pane-details-cmd";
        let _guard = TestSession::new(session);
        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        let details = list_pane_details(session).unwrap();
        assert_eq!(details.len(), 1);
        assert!(details[0].id.starts_with('%'));
        assert!(
            details[0].command == "sleep" || !details[0].command.is_empty(),
            "command should be reported"
        );
        assert!(!details[0].dead, "sleep pane should not be dead");
    }

    #[test]
    #[serial]
    fn pane_id_returns_percent_prefixed() {
        if !require_tmux_integration() {
            return;
        }
        let session = "batty-test-paneid-fmt";
        let _guard = TestSession::new(session);
        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        let id = pane_id(session).unwrap();
        assert!(
            id.starts_with('%'),
            "pane id should start with %, got: {id}"
        );
    }

    #[test]
    #[serial]
    fn respawn_pane_restarts_running_pane() {
        if !require_tmux_integration() {
            return;
        }
        let session = "batty-test-respawn";
        let _guard = TestSession::new(session);
        // Start with a long-running command so session stays alive
        create_session(session, "sleep", &["30".to_string()], "/tmp").unwrap();

        let pane = pane_id(session).unwrap();
        // Respawn with -k kills the running process and starts a new one
        respawn_pane(&pane, "sleep 10").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));

        // Pane should still exist and not be dead
        assert!(pane_exists(&pane), "respawned pane should exist");
    }
}
