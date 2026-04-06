//! Explicit topology reload trigger for a running daemon.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use super::{TEAM_CONFIG_FILE, team_config_dir};

const TOPOLOGY_RELOAD_MARKER: &str = "reload-topology";

fn topology_reload_marker_path(project_root: &Path) -> PathBuf {
    team_config_dir(project_root).join(TOPOLOGY_RELOAD_MARKER)
}

pub(crate) fn consume_topology_reload_request(project_root: &Path) -> Result<bool> {
    let path = topology_reload_marker_path(project_root);
    if !path.exists() {
        return Ok(false);
    }
    std::fs::remove_file(&path)?;
    Ok(true)
}

pub fn request_topology_reload(project_root: &Path) -> Result<()> {
    let team_dir = team_config_dir(project_root);
    let config_path = team_dir.join(TEAM_CONFIG_FILE);
    if !config_path.exists() {
        bail!("no team config found at {}; run `batty init` first", config_path.display());
    }

    std::fs::create_dir_all(&team_dir)?;
    std::fs::write(topology_reload_marker_path(project_root), b"")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consume_reload_request_clears_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let team_dir = team_config_dir(tmp.path());
        std::fs::create_dir_all(&team_dir).unwrap();
        let marker = topology_reload_marker_path(tmp.path());
        std::fs::write(&marker, b"").unwrap();

        assert!(marker.exists());
        assert!(consume_topology_reload_request(tmp.path()).unwrap());
        assert!(!marker.exists());
        assert!(!consume_topology_reload_request(tmp.path()).unwrap());
    }
}
