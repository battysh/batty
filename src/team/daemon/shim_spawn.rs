//! Shim subprocess spawning: create a socketpair, fork/exec `batty shim`,
//! pass the child socket on fd 3, and return an `AgentHandle`.

use std::os::unix::io::IntoRawFd;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use tracing::{debug, info};

use super::agent_handle::AgentHandle;
use crate::shim::protocol::{self, Channel};

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
/// `batty shim` with the appropriate arguments.
///
/// Returns an `AgentHandle` holding the parent channel and child PID.
pub(in crate::team) fn spawn_shim(
    member_name: &str,
    agent_type: &str,
    agent_cmd: &str,
    work_dir: &Path,
    pty_log_path: Option<&Path>,
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
    let handle = AgentHandle::new(member_name.to_string(), parent_channel, child_pid);

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

    #[test]
    fn batty_binary_returns_a_path() {
        let path = batty_binary();
        assert!(!path.is_empty());
    }
}
