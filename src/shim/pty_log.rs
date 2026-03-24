//! PTY log writer: streams raw PTY bytes to a log file so tmux panes can
//! `tail -f` the output for display.
//!
//! Each shim writes to `.batty/shim-logs/<agent-id>.pty.log`. The log is
//! truncated on shim start and rotated when it exceeds `MAX_LOG_BYTES`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Maximum log size before rotation (50 MB).
const MAX_LOG_BYTES: u64 = 50 * 1024 * 1024;

/// A writer that appends raw PTY bytes to a log file with size-based rotation.
pub struct PtyLogWriter {
    path: PathBuf,
    file: File,
    bytes_written: u64,
}

impl PtyLogWriter {
    /// Create a new PTY log writer. Truncates any existing log file.
    pub fn new(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)?;

        Ok(Self {
            path: path.to_path_buf(),
            file,
            bytes_written: 0,
        })
    }

    /// Append raw bytes to the log. Rotates if the file exceeds `MAX_LOG_BYTES`.
    pub fn write(&mut self, data: &[u8]) -> io::Result<()> {
        if self.bytes_written + data.len() as u64 > MAX_LOG_BYTES {
            self.rotate()?;
        }
        self.file.write_all(data)?;
        self.file.flush()?;
        self.bytes_written += data.len() as u64;
        Ok(())
    }

    /// Rotate: truncate the file and reset the counter. Viewers using `tail -F`
    /// (capital F) will follow the new file automatically.
    fn rotate(&mut self) -> io::Result<()> {
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.bytes_written = 0;
        Ok(())
    }

    /// Return the log file path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn new_creates_file_and_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("sub").join("deep").join("agent.pty.log");
        let writer = PtyLogWriter::new(&log_path).unwrap();
        assert!(log_path.exists());
        assert_eq!(writer.path(), log_path);
    }

    #[test]
    fn new_truncates_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("agent.pty.log");
        fs::write(&log_path, "old content").unwrap();

        let _writer = PtyLogWriter::new(&log_path).unwrap();
        let content = fs::read_to_string(&log_path).unwrap();
        assert!(content.is_empty());
    }

    #[test]
    fn write_appends_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("agent.pty.log");
        let mut writer = PtyLogWriter::new(&log_path).unwrap();

        writer.write(b"hello ").unwrap();
        writer.write(b"world").unwrap();

        let content = fs::read_to_string(&log_path).unwrap();
        assert_eq!(content, "hello world");
        assert_eq!(writer.bytes_written, 11);
    }

    #[test]
    fn write_preserves_ansi_escapes() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("agent.pty.log");
        let mut writer = PtyLogWriter::new(&log_path).unwrap();

        let ansi = b"\x1b[31mred\x1b[0m normal";
        writer.write(ansi).unwrap();

        let mut content = Vec::new();
        File::open(&log_path)
            .unwrap()
            .read_to_end(&mut content)
            .unwrap();
        assert_eq!(content, ansi);
    }

    #[test]
    fn rotate_truncates_and_resets_counter() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("agent.pty.log");
        let mut writer = PtyLogWriter::new(&log_path).unwrap();

        writer.write(b"some data").unwrap();
        assert!(writer.bytes_written > 0);

        writer.rotate().unwrap();
        assert_eq!(writer.bytes_written, 0);

        let content = fs::read_to_string(&log_path).unwrap();
        assert!(content.is_empty());
    }

    #[test]
    fn auto_rotation_at_size_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("agent.pty.log");
        let mut writer = PtyLogWriter::new(&log_path).unwrap();

        // Fake near-limit state
        writer.bytes_written = MAX_LOG_BYTES - 1;
        writer.write(b"x").unwrap(); // still fits
        assert_eq!(writer.bytes_written, MAX_LOG_BYTES);

        // Next write triggers rotation
        writer.write(b"overflow").unwrap();
        assert_eq!(writer.bytes_written, 8); // "overflow" length after rotation

        let content = fs::read_to_string(&log_path).unwrap();
        assert_eq!(content, "overflow");
    }

    #[test]
    fn path_returns_correct_path() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("test.pty.log");
        let writer = PtyLogWriter::new(&log_path).unwrap();
        assert_eq!(writer.path(), log_path);
    }

    #[test]
    fn empty_write_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("agent.pty.log");
        let mut writer = PtyLogWriter::new(&log_path).unwrap();
        writer.write(b"").unwrap();
        assert_eq!(writer.bytes_written, 0);
    }
}
