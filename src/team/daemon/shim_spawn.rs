//! Shim subprocess spawning: create a socketpair, fork/exec `batty shim`,
//! pass the child socket on fd 3, and return an `AgentHandle`.

use std::os::unix::io::IntoRawFd;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use super::agent_handle::AgentHandle;
use crate::shim::protocol::{self, Channel};

/// Kill any orphaned shim processes from previous daemon sessions.
///
/// DISABLED: cross-project kills when multiple batty projects run simultaneously.
/// The pgrep pattern matches shims from ALL projects, not just this one.
/// Shim lifecycle is managed by auto_respawn_on_crash instead.
#[allow(dead_code)]
pub(in crate::team) fn kill_orphan_shims(member_name: &str) {
    let pattern = format!("batty shim --id {member_name}");
    let output = match std::process::Command::new("pgrep")
        .args(["-f", &pattern])
        .output()
    {
        Ok(o) => o,
        Err(_) => return,
    };
    if !output.status.success() {
        return; // no matches
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let my_pid = std::process::id();
    for line in stdout.lines() {
        if let Ok(pid) = line.trim().parse::<u32>() {
            if pid == my_pid {
                continue;
            }
            warn!(
                member = member_name,
                pid, "killing orphan shim process from previous session"
            );
            unsafe {
                // Kill the process group to also terminate the child agent
                libc::kill(-(pid as i32), libc::SIGTERM);
            }
            // Give it a moment, then force kill
            std::thread::sleep(std::time::Duration::from_millis(500));
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
    }
}

/// Query the tmux pane dimensions for a member by looking up their pane ID
/// in the current session.
fn query_pane_size(member_name: &str) -> Option<(u16, u16)> {
    // Find the pane for this member by checking tmux pane titles or the layout
    let output = std::process::Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{pane_title} #{pane_width} #{pane_height}",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 3 && parts[0] == member_name {
            let cols: u16 = parts[1].parse().ok()?;
            let rows: u16 = parts[2].parse().ok()?;
            return Some((cols, rows));
        }
    }
    None
}

/// Resolve the path to the `batty` binary.
fn batty_binary() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "batty".to_string())
}

/// Spawn a shim subprocess for the given agent.
///
/// Creates a socketpair, passes the child end as fd 3, and launches
/// `batty shim` with the appropriate arguments. Kills any orphan shim
/// processes for this member from previous sessions first.
///
/// Returns an `AgentHandle` holding the parent channel and child PID.
pub(in crate::team) fn spawn_shim(
    member_name: &str,
    agent_type: &str,
    agent_cmd: &str,
    work_dir: &Path,
    pty_log_path: Option<&Path>,
    graceful_shutdown_timeout_secs: u64,
    auto_commit_on_restart: bool,
    sdk_mode: bool,
) -> Result<AgentHandle> {
    let (parent_sock, child_sock) =
        protocol::socketpair().context("failed to create socketpair for shim")?;

    let child_fd = child_sock.into_raw_fd();

    let batty = batty_binary();

    let mut cmd = Command::new(&batty);
    cmd.arg("shim")
        .arg("--id")
        .arg(member_name)
        .arg("--agent-type")
        .arg(agent_type)
        .arg("--cmd")
        .arg(agent_cmd)
        .arg("--cwd")
        .arg(work_dir.to_string_lossy().as_ref());

    if let Some(log_path) = pty_log_path {
        cmd.arg("--pty-log-path")
            .arg(log_path.to_string_lossy().as_ref());
    }
    cmd.arg("--graceful-shutdown-timeout-secs")
        .arg(graceful_shutdown_timeout_secs.to_string());
    cmd.arg("--auto-commit-on-restart")
        .arg(auto_commit_on_restart.to_string());

    if sdk_mode {
        cmd.arg("--sdk-mode");
    }

    // Query the tmux pane size for this member and pass it to the shim
    // so the agent's PTY matches the actual display dimensions.
    if let Some((cols, rows)) = query_pane_size(member_name) {
        cmd.arg("--rows").arg(rows.to_string());
        cmd.arg("--cols").arg(cols.to_string());
        debug!(
            member = member_name,
            rows, cols, "passing pane size to shim"
        );
    }

    // Set BATTY_MEMBER so detect_sender() works in SDK mode subprocesses
    cmd.env("BATTY_MEMBER", member_name);
    cmd.env(
        "BATTY_GRACEFUL_SHUTDOWN_TIMEOUT_SECS",
        graceful_shutdown_timeout_secs.to_string(),
    );
    cmd.env(
        "BATTY_AUTO_COMMIT_ON_RESTART",
        if auto_commit_on_restart {
            "true"
        } else {
            "false"
        },
    );

    // Pass child socket as fd 3
    cmd.stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    // Safety: we're passing the child fd as fd 3 to the child process.
    // The `pre_exec` closure runs after fork and before exec.
    unsafe {
        use std::os::unix::process::CommandExt;
        let child_fd_copy = child_fd;
        cmd.pre_exec(move || {
            // Dup the child socket to fd 3
            if child_fd_copy != 3 {
                libc::dup2(child_fd_copy, 3);
                libc::close(child_fd_copy);
            }
            Ok(())
        });
    }

    let child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn shim for {member_name}"))?;

    let child_pid = child.id();
    info!(
        member = member_name,
        pid = child_pid,
        "spawned shim subprocess"
    );

    // Close the child side of the socket in the parent
    // (already moved into the child process via pre_exec dup2)
    unsafe {
        libc::close(child_fd);
    }

    let parent_channel = Channel::new(parent_sock);
    let mut parent_channel = parent_channel;
    parent_channel
        .set_read_timeout(Some(std::time::Duration::from_millis(25)))
        .context("failed to set shim parent channel read timeout")?;
    let handle = AgentHandle::new(
        member_name.to_string(),
        parent_channel,
        child_pid,
        agent_type.to_string(),
        agent_cmd.to_string(),
        work_dir.to_path_buf(),
    );

    debug!(
        member = member_name,
        pid = child_pid,
        "shim handle created, waiting for Ready event"
    );

    Ok(handle)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn batty_binary_returns_a_path() {
        let path = batty_binary();
        assert!(!path.is_empty());
    }

    #[test]
    fn parent_channel_timeout_is_applied() {
        let (parent_sock, _child_sock) = protocol::socketpair().unwrap();
        let mut channel = Channel::new(parent_sock);
        channel
            .set_read_timeout(Some(Duration::from_millis(5)))
            .unwrap();
        let result: anyhow::Result<Option<crate::shim::protocol::Event>> = channel.recv();
        let io_error = result.unwrap_err();
        assert!(
            io_error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ))
        );
    }
}
