//! Team config hot-reload: poll team.yaml for changes and trigger topology
//! reconciliation when the file is modified.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use super::TeamDaemon;
use crate::team::config::TeamConfig;
use crate::team::config_diff;
use crate::team::events::TeamEvent;
use crate::team::hierarchy;
use crate::team::reload;

/// How often the daemon checks team.yaml for changes.
pub(super) const CONFIG_RELOAD_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// Minimum interval between config reload attempts (rate limiter).
pub(super) const CONFIG_RELOAD_MIN_INTERVAL: Duration = Duration::from_secs(10);

/// Tracks the team.yaml file fingerprint for change detection.
#[derive(Debug, Clone)]
pub(super) struct ConfigReloadMonitor {
    config_path: PathBuf,
    last_modified: SystemTime,
    last_len: u64,
    last_checked: Instant,
    last_reload_attempt: Option<Instant>,
}

impl ConfigReloadMonitor {
    /// Create a new monitor for the given team.yaml path.
    pub fn new(config_path: &Path) -> Result<Self> {
        let metadata = std::fs::metadata(config_path)
            .with_context(|| format!("failed to stat {}", config_path.display()))?;
        let modified = metadata
            .modified()
            .with_context(|| format!("failed to read mtime for {}", config_path.display()))?;
        Ok(Self {
            config_path: config_path.to_path_buf(),
            last_modified: modified,
            last_len: metadata.len(),
            last_checked: Instant::now(),
            last_reload_attempt: None,
        })
    }

    /// Whether enough time has passed to warrant a check.
    pub fn should_check(&self) -> bool {
        self.last_checked.elapsed() >= CONFIG_RELOAD_CHECK_INTERVAL
    }

    /// Check if the config file has changed. Returns true if modified.
    pub fn has_changed(&mut self) -> Result<bool> {
        self.last_checked = Instant::now();
        let metadata = std::fs::metadata(&self.config_path)
            .with_context(|| format!("failed to stat {}", self.config_path.display()))?;
        let modified = metadata
            .modified()
            .with_context(|| format!("failed to read mtime for {}", self.config_path.display()))?;
        let len = metadata.len();

        if modified != self.last_modified || len != self.last_len {
            self.last_modified = modified;
            self.last_len = len;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Whether enough time has passed since the last reload attempt.
    pub fn can_attempt_reload(&self) -> bool {
        self.last_reload_attempt
            .map(|instant| instant.elapsed() >= CONFIG_RELOAD_MIN_INTERVAL)
            .unwrap_or(true)
    }

    /// Record that a reload was attempted.
    pub fn mark_reload_attempt(&mut self) {
        self.last_reload_attempt = Some(Instant::now());
    }
}

impl TeamDaemon {
    /// Poll-based config reload check. Called from the main daemon loop.
    ///
    /// If team.yaml has changed, parses the new config, computes the diff,
    /// and triggers reconciliation.
    pub(super) fn maybe_hot_reload_config(
        &mut self,
        monitor: Option<&mut ConfigReloadMonitor>,
    ) -> Result<()> {
        let Some(monitor) = monitor else {
            return Ok(());
        };
        if !monitor.should_check() {
            return Ok(());
        }
        let forced = match reload::consume_topology_reload_request(&self.config.project_root) {
            Ok(forced) => forced,
            Err(e) => {
                debug!(error = %e, "failed to consume topology reload marker");
                false
            }
        };
        let changed = match monitor.has_changed() {
            Ok(changed) => changed,
            Err(e) => {
                debug!(error = %e, "failed to check config file for changes");
                return Ok(());
            }
        };
        if !changed && !forced {
            return Ok(());
        }

        if !monitor.can_attempt_reload() {
            debug!("config file changed but reload is rate-limited");
            return Ok(());
        }

        monitor.mark_reload_attempt();
        info!(forced, changed, "topology reload requested — computing topology diff");

        // Parse the new config
        let new_config = match TeamConfig::load(&monitor.config_path) {
            Ok(config) => config,
            Err(e) => {
                warn!(error = %e, "failed to parse updated team.yaml — ignoring change");
                self.record_orchestrator_action(format!("runtime: config reload failed — {e}"));
                return Ok(());
            }
        };

        // Resolve new hierarchy
        let new_members = match hierarchy::resolve_hierarchy(&new_config) {
            Ok(members) => members,
            Err(e) => {
                warn!(error = %e, "failed to resolve hierarchy from updated config");
                self.record_orchestrator_action(format!(
                    "runtime: config reload hierarchy resolution failed — {e}"
                ));
                return Ok(());
            }
        };

        // Compute diff
        let diff = config_diff::diff_members(&self.config.members, &new_members);
        if diff.is_empty() {
            debug!(
                forced,
                "topology reload found no membership change — no reconciliation needed"
            );
            return Ok(());
        }

        info!(
            added = diff.added.len(),
            removed = diff.removed.len(),
            "topology diff computed — reconciling"
        );

        // Log event
        let reason = format!(
            "+{} added, -{} removed",
            diff.added.len(),
            diff.removed.len()
        );
        self.record_orchestrator_action(format!("runtime: topology changed — {reason}"));
        if let Err(e) = self.event_sink.emit(TeamEvent::topology_changed(
            diff.added.len() as u32,
            diff.removed.len() as u32,
            &reason,
        )) {
            warn!(error = %e, "failed to emit topology_changed event");
        }

        // Reconcile: apply the diff
        self.reconcile_topology(diff, new_config, new_members)?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_config(f: &mut NamedTempFile, content: &str) {
        f.as_file_mut().set_len(0).unwrap();
        f.seek(std::io::SeekFrom::Start(0)).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.flush().unwrap();
    }

    use std::io::Seek;

    #[test]
    fn monitor_detects_no_change() {
        let mut f = NamedTempFile::new().unwrap();
        write_config(&mut f, "name: test\nroles: []\n");
        let mut monitor = ConfigReloadMonitor::new(f.path()).unwrap();
        // Force check
        monitor.last_checked = Instant::now() - Duration::from_secs(10);
        assert!(!monitor.has_changed().unwrap());
    }

    #[test]
    fn monitor_detects_content_change() {
        let mut f = NamedTempFile::new().unwrap();
        write_config(&mut f, "name: test\nroles: []\n");
        let mut monitor = ConfigReloadMonitor::new(f.path()).unwrap();

        // Modify the file
        std::thread::sleep(Duration::from_millis(50));
        write_config(&mut f, "name: test\nroles: []\n# changed\n");

        monitor.last_checked = Instant::now() - Duration::from_secs(10);
        assert!(monitor.has_changed().unwrap());
    }

    #[test]
    fn should_check_respects_interval() {
        let f = NamedTempFile::new().unwrap();
        std::fs::write(f.path(), "name: test\nroles: []\n").unwrap();
        let monitor = ConfigReloadMonitor::new(f.path()).unwrap();
        // Just created — should not check yet
        assert!(!monitor.should_check());
    }

    #[test]
    fn can_attempt_reload_rate_limits() {
        let f = NamedTempFile::new().unwrap();
        std::fs::write(f.path(), "name: test\nroles: []\n").unwrap();
        let mut monitor = ConfigReloadMonitor::new(f.path()).unwrap();
        assert!(monitor.can_attempt_reload());
        monitor.mark_reload_attempt();
        assert!(!monitor.can_attempt_reload());
    }
}
