//! File-based merge lock and merge outcome types.
//!
//! `MergeLock` serializes concurrent merge attempts using an O_EXCL lock file.
//! `MergeOutcome` encodes the three possible results of a merge attempt.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

pub(crate) struct MergeLock {
    path: PathBuf,
}

impl MergeLock {
    pub fn acquire(project_root: &Path) -> Result<Self> {
        let path = project_root.join(".batty").join("merge.lock");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let start = std::time::Instant::now();
        loop {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if start.elapsed() > std::time::Duration::from_secs(60) {
                        bail!("merge lock timeout after 60s: {}", path.display());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
                Err(error) => bail!("failed to acquire merge lock: {error}"),
            }
        }
    }
}

impl Drop for MergeLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Debug)]
pub(crate) enum MergeOutcome {
    Success,
    RebaseConflict(String),
    MergeFailure(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
    };
    use std::thread;
    use std::time::Duration;

    #[test]
    fn merge_lock_acquire_release() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty")).unwrap();
        let lock_path = tmp.path().join(".batty").join("merge.lock");

        {
            let lock = MergeLock::acquire(tmp.path()).unwrap();
            assert!(lock_path.exists());
            drop(lock);
        }
        assert!(!lock_path.exists());
    }

    #[test]
    fn merge_lock_second_acquire_waits_for_release() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty")).unwrap();

        let first_lock = MergeLock::acquire(tmp.path()).unwrap();
        let project_root = tmp.path().to_path_buf();
        let barrier = Arc::new(Barrier::new(2));
        let acquired = Arc::new(AtomicBool::new(false));

        let thread_barrier = Arc::clone(&barrier);
        let thread_acquired = Arc::clone(&acquired);
        let handle = thread::spawn(move || {
            thread_barrier.wait();
            let second_lock = MergeLock::acquire(&project_root).unwrap();
            thread_acquired.store(true, Ordering::SeqCst);
            drop(second_lock);
        });

        barrier.wait();
        thread::sleep(Duration::from_millis(600));
        assert!(!acquired.load(Ordering::SeqCst));

        drop(first_lock);
        handle.join().unwrap();
        assert!(acquired.load(Ordering::SeqCst));
    }
}
