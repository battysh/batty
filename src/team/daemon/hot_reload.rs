//! Binary hot-reload: fingerprint tracking, marker files, and exec-based restart.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use tracing::warn;

use super::{TeamDaemon, now_unix};

pub(super) const HOT_RELOAD_CHECK_INTERVAL: Duration = Duration::from_secs(30);
pub(super) const HOT_RELOAD_MIN_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BinaryFingerprint {
    pub path: PathBuf,
    pub modified: SystemTime,
    pub len: u64,
    #[cfg(unix)]
    pub inode: u64,
}

impl BinaryFingerprint {
    pub fn capture(path: &Path) -> Result<Self> {
        let metadata =
            fs::metadata(path).with_context(|| format!("failed to stat {}", path.display()))?;
        let modified = metadata
            .modified()
            .with_context(|| format!("failed to read mtime for {}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            modified,
            len: metadata.len(),
            #[cfg(unix)]
            inode: std::os::unix::fs::MetadataExt::ino(&metadata),
        })
    }

    pub fn changed_from(&self, previous: &Self) -> bool {
        self.modified != previous.modified || self.len != previous.len || {
            #[cfg(unix)]
            {
                self.inode != previous.inode
            }
            #[cfg(not(unix))]
            {
                false
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct HotReloadMonitor {
    binary: BinaryFingerprint,
    last_checked: Instant,
    last_reload_attempt: Option<Instant>,
}

impl HotReloadMonitor {
    pub fn new(binary: BinaryFingerprint) -> Self {
        Self {
            binary,
            last_checked: Instant::now(),
            last_reload_attempt: None,
        }
    }

    pub fn for_current_exe() -> Result<Self> {
        let path = std::env::current_exe().context("failed to resolve current executable")?;
        Ok(Self::new(BinaryFingerprint::capture(&path)?))
    }

    pub fn should_check(&self) -> bool {
        self.last_checked.elapsed() >= HOT_RELOAD_CHECK_INTERVAL
    }

    pub fn changed_binary(&mut self) -> Result<Option<BinaryFingerprint>> {
        self.last_checked = Instant::now();
        let current = BinaryFingerprint::capture(&self.binary.path)?;
        Ok(current.changed_from(&self.binary).then_some(current))
    }

    pub fn can_attempt_reload(&self) -> bool {
        self.last_reload_attempt
            .map(|instant| instant.elapsed() >= HOT_RELOAD_MIN_INTERVAL)
            .unwrap_or(true)
    }

    pub fn mark_reload_attempt(&mut self) {
        self.last_reload_attempt = Some(Instant::now());
    }
}

impl TeamDaemon {
    pub(super) fn maybe_hot_reload_binary(
        &mut self,
        monitor: Option<&mut HotReloadMonitor>,
    ) -> Result<()> {
        let Some(monitor) = monitor else {
            return Ok(());
        };
        if !monitor.should_check() {
            return Ok(());
        }

        let Some(updated_binary) = monitor.changed_binary()? else {
            return Ok(());
        };

        if !monitor.can_attempt_reload() {
            warn!(
                path = %updated_binary.path.display(),
                "binary changed again but reload attempt is rate-limited"
            );
            return Ok(());
        }

        if !binary_is_reloadable(&updated_binary.path) {
            warn!(
                path = %updated_binary.path.display(),
                "binary changed but is not safe to hot-reload yet"
            );
            return Ok(());
        }

        monitor.mark_reload_attempt();
        self.persist_runtime_state(false)?;
        self.record_daemon_reloading();
        self.record_orchestrator_action(format!(
            "runtime: daemon reloading after binary change ({})",
            updated_binary.path.display()
        ));
        write_hot_reload_marker(&self.config.project_root)?;

        if let Err(error) = exec_reloaded_daemon(&updated_binary.path, &self.config.project_root) {
            let _ = clear_hot_reload_marker(&self.config.project_root);
            warn!(
                path = %updated_binary.path.display(),
                error = %error,
                "failed to exec updated daemon binary; continuing on existing process"
            );
            self.record_orchestrator_action(format!("runtime: daemon reload failed ({error})"));
        }

        Ok(())
    }
}

pub(super) fn hot_reload_marker_path(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("reload")
}

pub(super) fn write_hot_reload_marker(project_root: &Path) -> Result<()> {
    let path = hot_reload_marker_path(project_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(&path, now_unix().to_string())
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

pub(super) fn clear_hot_reload_marker(project_root: &Path) -> Result<()> {
    let path = hot_reload_marker_path(project_root);
    if !path.exists() {
        return Ok(());
    }
    fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
    Ok(())
}

pub(super) fn consume_hot_reload_marker(project_root: &Path) -> bool {
    let path = hot_reload_marker_path(project_root);
    if !path.exists() {
        return false;
    }
    clear_hot_reload_marker(project_root).is_ok()
}

pub(super) fn hot_reload_daemon_args(project_root: &Path) -> Vec<String> {
    let root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf())
        .to_string_lossy()
        .to_string();
    vec![
        "-v".to_string(),
        "daemon".to_string(),
        "--project-root".to_string(),
        root,
        "--resume".to_string(),
    ]
}

pub(super) fn binary_is_reloadable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return false;
        }
    }

    #[cfg(target_os = "macos")]
    {
        let Ok(status) = std::process::Command::new("codesign")
            .args(["--verify", path.to_string_lossy().as_ref()])
            .status()
        else {
            return false;
        };
        if !status.success() {
            return false;
        }
    }

    true
}

#[cfg(unix)]
pub(super) fn exec_reloaded_daemon(executable: &Path, project_root: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let error = std::process::Command::new(executable)
        .args(hot_reload_daemon_args(project_root))
        .exec();
    Err(anyhow::Error::new(error).context(format!("failed to exec {}", executable.display())))
}

#[cfg(not(unix))]
pub(super) fn exec_reloaded_daemon(_executable: &Path, _project_root: &Path) -> Result<()> {
    bail!("daemon hot reload via exec is only supported on unix")
}
