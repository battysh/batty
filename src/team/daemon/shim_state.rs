//! Shim session state persistence: save agent handle metadata on stop,
//! restore on start for session resume.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::*;

/// Persisted state for a single shim agent handle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedShimHandle {
    pub id: String,
    pub agent_type: String,
    pub agent_cmd: String,
    pub work_dir: PathBuf,
}

/// Collection of persisted shim handles.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShimStateFile {
    pub handles: HashMap<String, PersistedShimHandle>,
}

/// Path to the shim state file.
fn shim_state_path(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("shim_state.json")
}

impl TeamDaemon {
    /// Save all active shim handle metadata to disk for session resume.
    pub(in crate::team) fn save_shim_state(&self) -> Result<()> {
        if self.shim_handles.is_empty() {
            // Clean up any stale state file
            let path = shim_state_path(&self.config.project_root);
            if path.exists() {
                let _ = std::fs::remove_file(&path);
            }
            return Ok(());
        }

        let mut state = ShimStateFile::default();
        for (name, handle) in &self.shim_handles {
            state.handles.insert(
                name.clone(),
                PersistedShimHandle {
                    id: handle.id.clone(),
                    agent_type: handle.agent_type.clone(),
                    agent_cmd: handle.agent_cmd.clone(),
                    work_dir: handle.work_dir.clone(),
                },
            );
        }

        let path = shim_state_path(&self.config.project_root);
        let json =
            serde_json::to_string_pretty(&state).context("failed to serialize shim state")?;
        std::fs::write(&path, json)
            .with_context(|| format!("failed to write shim state to {}", path.display()))?;

        info!(
            count = state.handles.len(),
            path = %path.display(),
            "saved shim state for session resume"
        );
        Ok(())
    }

    /// Load saved shim state and respawn shim handles.
    /// Returns the number of handles restored.
    #[allow(dead_code)] // Called from start flow when shim mode is fully wired
    pub(in crate::team) fn restore_shim_state(&mut self) -> Result<usize> {
        let path = shim_state_path(&self.config.project_root);
        if !path.exists() {
            return Ok(0);
        }

        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read shim state from {}", path.display()))?;
        let state: ShimStateFile =
            serde_json::from_str(&content).with_context(|| "failed to parse shim state file")?;

        let mut restored = 0;
        for (name, persisted) in &state.handles {
            info!(
                member = name.as_str(),
                agent_type = persisted.agent_type.as_str(),
                work_dir = %persisted.work_dir.display(),
                "restoring shim from saved state"
            );
            let sdk_mode = super::launcher::agent_supports_sdk_mode(&persisted.agent_type)
                && self.config.team_config.use_sdk_mode;
            match super::shim_spawn::spawn_shim(
                &persisted.id,
                &persisted.agent_type,
                &persisted.agent_cmd,
                &persisted.work_dir,
                None,
                self.config
                    .team_config
                    .workflow_policy
                    .graceful_shutdown_timeout_secs,
                self.config
                    .team_config
                    .workflow_policy
                    .auto_commit_on_restart,
                sdk_mode,
            ) {
                Ok(handle) => {
                    self.shim_handles.insert(name.clone(), handle);
                    restored += 1;
                }
                Err(error) => {
                    warn!(
                        member = name.as_str(),
                        error = %error,
                        "failed to restore shim handle"
                    );
                }
            }
        }

        // Clear the state file after successful restore
        if let Err(error) = std::fs::remove_file(&path) {
            warn!(error = %error, "failed to clean up shim state file");
        }

        info!(restored, total = state.handles.len(), "shim state restored");
        Ok(restored)
    }
}

/// Load saved shim state without spawning (for inspection/testing).
#[allow(dead_code)] // Used by tests and future status/inspection commands
pub fn load_shim_state(project_root: &Path) -> Result<ShimStateFile> {
    let path = shim_state_path(project_root);
    if !path.exists() {
        return Ok(ShimStateFile::default());
    }
    let content = std::fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&content)?)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::test_support::TestDaemonBuilder;

    #[test]
    fn save_shim_state_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty")).unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();

        // Insert a mock handle
        let (parent, _child) = crate::shim::protocol::socketpair().unwrap();
        let channel = crate::shim::protocol::Channel::new(parent);
        let handle = crate::team::daemon::agent_handle::AgentHandle::new(
            "eng-1".into(),
            channel,
            999,
            "claude".into(),
            "claude --dangerously-skip-permissions".into(),
            PathBuf::from("/tmp/worktree/eng-1"),
        );
        daemon.shim_handles.insert("eng-1".to_string(), handle);

        daemon.save_shim_state().unwrap();

        let path = shim_state_path(tmp.path());
        assert!(path.exists());

        let state = load_shim_state(tmp.path()).unwrap();
        assert_eq!(state.handles.len(), 1);
        let h = &state.handles["eng-1"];
        assert_eq!(h.id, "eng-1");
        assert_eq!(h.agent_type, "claude");
        assert_eq!(h.agent_cmd, "claude --dangerously-skip-permissions");
        assert_eq!(h.work_dir, PathBuf::from("/tmp/worktree/eng-1"));
    }

    #[test]
    fn save_shim_state_no_handles_removes_stale_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty")).unwrap();
        let daemon = TestDaemonBuilder::new(tmp.path()).build();

        // Pre-create a stale state file
        let path = shim_state_path(tmp.path());
        std::fs::write(&path, "{}").unwrap();
        assert!(path.exists());

        daemon.save_shim_state().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn load_shim_state_missing_file_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let state = load_shim_state(tmp.path()).unwrap();
        assert!(state.handles.is_empty());
    }

    #[test]
    fn shim_state_roundtrip() {
        let state = ShimStateFile {
            handles: HashMap::from([
                (
                    "eng-1".to_string(),
                    PersistedShimHandle {
                        id: "eng-1".to_string(),
                        agent_type: "claude".to_string(),
                        agent_cmd: "claude".to_string(),
                        work_dir: PathBuf::from("/tmp/eng-1"),
                    },
                ),
                (
                    "eng-2".to_string(),
                    PersistedShimHandle {
                        id: "eng-2".to_string(),
                        agent_type: "codex".to_string(),
                        agent_cmd: "codex".to_string(),
                        work_dir: PathBuf::from("/tmp/eng-2"),
                    },
                ),
            ]),
        };

        let json = serde_json::to_string_pretty(&state).unwrap();
        let loaded: ShimStateFile = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.handles.len(), 2);
        assert_eq!(loaded.handles["eng-1"].agent_type, "claude");
        assert_eq!(loaded.handles["eng-2"].agent_type, "codex");
    }
}
