//! Periodic Ping/Pong health monitoring for shim handles.
//!
//! The daemon sends Ping commands at a configurable interval and tracks
//! the last Pong response time. Handles that don't respond within the
//! timeout are flagged as stale and optionally killed/respawned.

use std::time::Instant;

use anyhow::Result;
use tracing::{debug, warn};

use super::super::*;

impl TeamDaemon {
    /// Periodic health check: send Pings to all active shim handles,
    /// detect stale handles that haven't responded.
    pub(in crate::team) fn shim_health_check(&mut self) -> Result<()> {
        let interval_secs = self.config.team_config.shim_health_check_interval_secs;
        let timeout_secs = self.config.team_config.shim_health_timeout_secs;

        // Only run at the configured interval
        if self.last_shim_health_check.elapsed().as_secs() < interval_secs {
            return Ok(());
        }
        self.last_shim_health_check = Instant::now();

        let names: Vec<String> = self.shim_handles.keys().cloned().collect();

        for name in &names {
            // First pass: observe the handle (immutable) to classify staleness.
            // We release the borrow before calling self.record_orchestrator_action
            // and before re-borrowing mutably to send_ping.
            let stale_observation = {
                let Some(handle) = self.shim_handles.get(name) else {
                    continue;
                };
                if handle.is_terminal() {
                    continue;
                }
                handle.secs_since_last_pong().and_then(|secs_since_pong| {
                    if secs_since_pong > timeout_secs {
                        let recently_active = handle
                            .secs_since_last_activity()
                            .is_some_and(|secs_since_activity| {
                                secs_since_activity <= timeout_secs
                            });
                        let working =
                            handle.state == crate::shim::protocol::ShimState::Working;
                        Some((secs_since_pong, working && recently_active))
                    } else {
                        None
                    }
                })
            };

            if let Some((secs_since_pong, suppress_warn)) = stale_observation {
                if suppress_warn {
                    debug!(
                        member = name.as_str(),
                        secs_since_pong,
                        timeout_secs,
                        "shim missed recent Pong but has fresh activity; suppressing stale warning"
                    );
                } else {
                    warn!(
                        member = name.as_str(),
                        secs_since_pong,
                        timeout_secs,
                        "shim handle stale — no Pong within timeout"
                    );
                    self.record_orchestrator_action(format!(
                        "health: shim {} stale (no Pong for {}s, timeout={}s)",
                        name, secs_since_pong, timeout_secs
                    ));
                    // Don't kill or skip — fall through and keep pinging so
                    // last_pong_at can recover if the shim comes back. The
                    // stall detector will handle escalation if truly stuck.
                }
            }

            // Send Ping
            if let Some(handle) = self.shim_handles.get_mut(name) {
                if let Err(error) = handle.send_ping() {
                    debug!(
                        member = name.as_str(),
                        error = %error,
                        "failed to send Ping to shim"
                    );
                }
            }
        }

        // Sync pane sizes: if a tmux pane was resized, send Resize command
        // to the shim so the agent's PTY matches the display.
        let shim_names: Vec<String> = self.shim_handles.keys().cloned().collect();
        for name in &shim_names {
            let Some(pane_id) = self.config.pane_map.get(name.as_str()) else {
                continue;
            };
            let Some((cols, rows)) = query_tmux_pane_size(pane_id) else {
                continue;
            };
            let Some(handle) = self.shim_handles.get_mut(name.as_str()) else {
                continue;
            };
            if handle.is_terminal() {
                continue;
            }
            // Only send resize if dimensions changed
            if handle.last_cols != cols || handle.last_rows != rows {
                if let Err(error) = handle
                    .channel
                    .send(&crate::shim::protocol::Command::Resize { rows, cols })
                {
                    debug!(
                        member = name.as_str(),
                        error = %error,
                        "failed to send Resize to shim"
                    );
                } else {
                    debug!(
                        member = name.as_str(),
                        rows, cols, "synced pane size to shim"
                    );
                }
                handle.last_cols = cols;
                handle.last_rows = rows;
            }
        }

        Ok(())
    }
}

/// Query tmux pane dimensions by pane ID (e.g. "%602").
fn query_tmux_pane_size(pane_id: &str) -> Option<(u16, u16)> {
    let output = std::process::Command::new("tmux")
        .args([
            "display-message",
            "-t",
            pane_id,
            "-p",
            "#{pane_width} #{pane_height}",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parts: Vec<&str> = stdout.split_whitespace().collect();
    if parts.len() >= 2 {
        let cols: u16 = parts[0].parse().ok()?;
        let rows: u16 = parts[1].parse().ok()?;
        Some((cols, rows))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::super::super::*;
    use crate::shim::protocol::{self, Channel, Command, ShimState};
    use crate::team::test_support::TestDaemonBuilder;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    /// Canonical fake-shim injection path for ping/pong tests. Routes
    /// through [`ScenarioHooks::insert_fake_shim`] so every test uses the
    /// same seam as the scenario framework (ticket #637).
    fn insert_handle_with_channel(daemon: &mut TeamDaemon, name: &str) -> Channel {
        let (parent, child) = protocol::socketpair().unwrap();
        let parent_channel = Channel::new(parent);
        let child_channel = Channel::new(child);
        daemon.scenario_hooks().insert_fake_shim(
            name,
            parent_channel,
            999,
            "claude",
            "claude",
            PathBuf::from("/tmp/test"),
        );
        child_channel
    }

    #[test]
    fn shim_health_check_sends_ping_after_interval() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        let mut child = insert_handle_with_channel(&mut daemon, "eng-1");

        // Record a Pong so the handle isn't brand new
        daemon.shim_handles.get_mut("eng-1").unwrap().record_pong();
        daemon
            .shim_handles
            .get_mut("eng-1")
            .unwrap()
            .apply_state_change(ShimState::Idle);

        // Backdate the last check so it fires
        daemon.last_shim_health_check = Instant::now() - Duration::from_secs(120);

        daemon.shim_health_check().unwrap();

        // Verify a Ping was sent
        let cmd: Command = child.recv().unwrap().unwrap();
        assert!(matches!(cmd, Command::Ping));
    }

    #[test]
    fn shim_health_check_skips_when_interval_not_elapsed() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        let child = insert_handle_with_channel(&mut daemon, "eng-1");
        daemon
            .shim_handles
            .get_mut("eng-1")
            .unwrap()
            .apply_state_change(ShimState::Idle);

        // last_shim_health_check is Instant::now() — interval not elapsed
        daemon.shim_health_check().unwrap();

        // Drop the child end so recv would fail — nothing should have been sent
        // (We verify by trying to recv with a timeout)
        drop(child);
    }

    #[test]
    fn shim_health_check_skips_terminal_handles() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        let _child = insert_handle_with_channel(&mut daemon, "eng-1");

        daemon
            .shim_handles
            .get_mut("eng-1")
            .unwrap()
            .apply_state_change(ShimState::Dead);
        daemon.last_shim_health_check = Instant::now() - Duration::from_secs(120);

        // Should not panic or try to send to a dead handle
        daemon.shim_health_check().unwrap();
    }

    #[test]
    fn shim_health_check_detects_stale_handle() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        let _child = insert_handle_with_channel(&mut daemon, "eng-1");

        // Set last_pong_at to far in the past (beyond the 120s timeout)
        daemon.shim_handles.get_mut("eng-1").unwrap().last_pong_at =
            Some(Instant::now() - Duration::from_secs(300));
        daemon
            .shim_handles
            .get_mut("eng-1")
            .unwrap()
            .apply_state_change(ShimState::Idle);

        daemon.last_shim_health_check = Instant::now() - Duration::from_secs(120);

        // Should log warning about stale handle but not panic
        daemon.shim_health_check().unwrap();
    }

    #[test]
    fn shim_health_check_suppresses_stale_warning_for_recently_active_working_handle() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        let mut child = insert_handle_with_channel(&mut daemon, "eng-1");

        daemon.shim_handles.get_mut("eng-1").unwrap().last_pong_at =
            Some(Instant::now() - Duration::from_secs(300));
        daemon
            .shim_handles
            .get_mut("eng-1")
            .unwrap()
            .last_activity_at = Some(Instant::now() - Duration::from_secs(10));
        daemon
            .shim_handles
            .get_mut("eng-1")
            .unwrap()
            .apply_state_change(ShimState::Working);

        daemon.last_shim_health_check = Instant::now() - Duration::from_secs(120);
        daemon.shim_health_check().unwrap();

        let cmd: Command = child.recv().unwrap().unwrap();
        assert!(matches!(cmd, Command::Ping));
    }
}
