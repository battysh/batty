//! Event extraction from piped tmux output.
//!
//! Reads the pipe-pane log file, strips ANSI escapes, pattern-matches against
//! known agent output patterns, and produces structured events. A rolling event
//! buffer provides a compact summary of the executor's recent activity for the
//! supervisor's context window.

use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use regex::Regex;
use serde::Serialize;
use tracing::debug;

use crate::prompt::strip_ansi;

/// Default rolling buffer size.
#[cfg(test)]
const DEFAULT_BUFFER_SIZE: usize = 50;

/// Structured events extracted from executor output.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PipeEvent {
    /// Executor picked a new task.
    TaskStarted { task_id: String, title: String },
    /// Executor created a file.
    FileCreated { path: String },
    /// Executor modified a file.
    FileModified { path: String },
    /// Executor ran a command.
    CommandRan {
        command: String,
        success: Option<bool>,
    },
    /// Test execution detected.
    TestRan { passed: bool, detail: String },
    /// Executor is asking a question (prompt detected).
    PromptDetected { prompt: String },
    /// Executor marked a task done.
    TaskCompleted { task_id: String },
    /// Executor made a git commit.
    CommitMade { hash: String, message: String },
    /// Raw output line (for lines that don't match any pattern).
    #[allow(dead_code)] // Retained for optional verbose event buffering and dedicated tests.
    OutputLine { line: String },
}

/// Compiled regex patterns for extracting events from executor output.
pub struct EventPatterns {
    patterns: Vec<(Regex, EventClassifier)>,
}

type EventClassifier = fn(&regex::Captures) -> PipeEvent;

impl EventPatterns {
    /// Build default event extraction patterns.
    ///
    /// These patterns target common agent output after ANSI stripping.
    /// They work across Claude Code, Codex, and Aider output.
    pub fn default_patterns() -> Self {
        Self {
            patterns: vec![
                // Task started: "Picked and moved task #N" or kanban-md output
                (
                    Regex::new(r"(?i)(?:picked|claimed|starting|working on)\s+(?:and moved\s+)?task\s+#?(\d+)(?::\s+(.+))?").unwrap(),
                    |caps| PipeEvent::TaskStarted {
                        task_id: caps.get(1).map(|m| m.as_str().to_string()).unwrap_or_default(),
                        title: caps.get(2).map(|m| m.as_str().to_string()).unwrap_or_default(),
                    },
                ),
                // Task completed: "Moved task #N" to done, or "task #N done"
                (
                    Regex::new(r"(?i)(?:moved task\s+#?(\d+).*(?:done|complete)|task\s+#?(\d+)\s+(?:done|complete))").unwrap(),
                    |caps| PipeEvent::TaskCompleted {
                        task_id: caps.get(1)
                            .or_else(|| caps.get(2))
                            .map(|m| m.as_str().to_string())
                            .unwrap_or_default(),
                    },
                ),
                // Git commit: "[main abc1234] message" or "commit abc1234"
                (
                    Regex::new(r"(?:\[[\w/-]+\s+([0-9a-f]{7,40})\]\s+(.+)|commit\s+([0-9a-f]{7,40}))").unwrap(),
                    |caps| PipeEvent::CommitMade {
                        hash: caps.get(1)
                            .or_else(|| caps.get(3))
                            .map(|m| m.as_str().to_string())
                            .unwrap_or_default(),
                        message: caps.get(2).map(|m| m.as_str().to_string()).unwrap_or_default(),
                    },
                ),
                // Test result: "test result: ok. N passed" or "test result: FAILED"
                (
                    Regex::new(r"test result:\s*(ok|FAILED)").unwrap(),
                    |caps| {
                        let result = caps.get(1).map(|m| m.as_str()).unwrap_or("FAILED");
                        PipeEvent::TestRan {
                            passed: result == "ok",
                            detail: caps.get(0).map(|m| m.as_str().to_string()).unwrap_or_default(),
                        }
                    },
                ),
                // File created: "Created file X" or "Write tool" or "File created"
                (
                    Regex::new(r"(?i)(?:created?\s+(?:file\s+)?|wrote\s+|writing\s+to\s+)([\w/.+\-]+\.\w+)").unwrap(),
                    |caps| PipeEvent::FileCreated {
                        path: caps.get(1).map(|m| m.as_str().to_string()).unwrap_or_default(),
                    },
                ),
                // File modified: "Edited X" or "Modified X" or "Edit tool"
                (
                    Regex::new(r"(?i)(?:edit(?:ed|ing)?\s+|modif(?:ied|ying)\s+)([\w/.+\-]+\.\w+)").unwrap(),
                    |caps| PipeEvent::FileModified {
                        path: caps.get(1).map(|m| m.as_str().to_string()).unwrap_or_default(),
                    },
                ),
                // Command ran: "$ command" or "Running: command" or exit code pattern
                (
                    Regex::new(r"(?:^\$\s+(.+)|Running:\s+(.+))").unwrap(),
                    |caps| PipeEvent::CommandRan {
                        command: caps.get(1)
                            .or_else(|| caps.get(2))
                            .map(|m| m.as_str().to_string())
                            .unwrap_or_default(),
                        success: None,
                    },
                ),
                // Prompt patterns: "Allow tool", "[y/n]", "Continue?"
                (
                    Regex::new(r"(?i)(?:allow\s+tool|continue\?|\[y/n\]|do you want to proceed)").unwrap(),
                    |caps| PipeEvent::PromptDetected {
                        prompt: caps.get(0).map(|m| m.as_str().to_string()).unwrap_or_default(),
                    },
                ),
            ],
        }
    }

    /// Try to classify a line of ANSI-stripped output as a structured event.
    /// Returns None if no pattern matches.
    pub fn classify(&self, line: &str) -> Option<PipeEvent> {
        for (regex, classify) in &self.patterns {
            if let Some(caps) = regex.captures(line) {
                return Some(classify(&caps));
            }
        }
        None
    }
}

/// Rolling buffer of recent events.
///
/// Thread-safe via Arc<Mutex<_>> for sharing between the watcher thread
/// and the supervisor's context composition.
#[derive(Debug, Clone)]
pub struct EventBuffer {
    inner: Arc<Mutex<EventBufferInner>>,
}

#[derive(Debug)]
struct EventBufferInner {
    events: VecDeque<PipeEvent>,
    max_size: usize,
}

impl EventBuffer {
    /// Create a new event buffer with the given capacity.
    pub fn new(max_size: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(EventBufferInner {
                events: VecDeque::with_capacity(max_size),
                max_size,
            })),
        }
    }

    /// Create a buffer with the default size (50 events).
    #[cfg(test)]
    pub fn default_size() -> Self {
        Self::new(DEFAULT_BUFFER_SIZE)
    }

    /// Push an event into the buffer, evicting the oldest if full.
    pub fn push(&self, event: PipeEvent) {
        let mut inner = self.inner.lock().unwrap();
        if inner.events.len() >= inner.max_size {
            inner.events.pop_front();
        }
        inner.events.push_back(event);
    }

    /// Get a snapshot of all events in the buffer.
    pub fn snapshot(&self) -> Vec<PipeEvent> {
        let inner = self.inner.lock().unwrap();
        inner.events.iter().cloned().collect()
    }

    /// Get the number of events in the buffer.
    pub fn len(&self) -> usize {
        let inner = self.inner.lock().unwrap();
        inner.events.len()
    }

    /// Check if the buffer is empty.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Clear all events.
    #[allow(dead_code)]
    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.events.clear();
    }

    /// Format the buffer as a compact summary for the supervisor's context.
    pub fn format_summary(&self) -> String {
        let events = self.snapshot();
        if events.is_empty() {
            return "(no events yet)".to_string();
        }

        let mut summary = String::new();
        for event in &events {
            let line = match event {
                PipeEvent::TaskStarted { task_id, title } => {
                    format!("→ task #{task_id} started: {title}")
                }
                PipeEvent::TaskCompleted { task_id } => {
                    format!("✓ task #{task_id} completed")
                }
                PipeEvent::FileCreated { path } => {
                    format!("+ {path}")
                }
                PipeEvent::FileModified { path } => {
                    format!("~ {path}")
                }
                PipeEvent::CommandRan { command, success } => {
                    let status = match success {
                        Some(true) => " ✓",
                        Some(false) => " ✗",
                        None => "",
                    };
                    format!("$ {command}{status}")
                }
                PipeEvent::TestRan { passed, detail } => {
                    let icon = if *passed { "✓" } else { "✗" };
                    format!("{icon} test: {detail}")
                }
                PipeEvent::PromptDetected { prompt } => {
                    format!("? {prompt}")
                }
                PipeEvent::CommitMade { hash, message } => {
                    let short_hash = &hash[..7.min(hash.len())];
                    format!("⊕ commit {short_hash}: {message}")
                }
                PipeEvent::OutputLine { line } => {
                    // Truncate long output lines
                    if line.len() > 80 {
                        format!("  {}...", &line[..77])
                    } else {
                        format!("  {line}")
                    }
                }
            };
            summary.push_str(&line);
            summary.push('\n');
        }
        summary
    }
}

/// Watches a pipe-pane log file and extracts events from new content.
///
/// Uses polling (seek to last position, read new bytes) which is simple
/// and portable. The polling interval is configurable.
pub struct PipeWatcher {
    path: PathBuf,
    patterns: EventPatterns,
    buffer: EventBuffer,
    position: u64,
    line_buffer: String,
}

impl PipeWatcher {
    /// Create a new pipe watcher for the given log file.
    pub fn new(path: &Path, buffer: EventBuffer) -> Self {
        Self::new_with_position(path, buffer, 0)
    }

    /// Create a new pipe watcher starting from a specific byte offset.
    ///
    /// Offsets beyond EOF are clamped during polling.
    pub fn new_with_position(path: &Path, buffer: EventBuffer, position: u64) -> Self {
        Self {
            path: path.to_path_buf(),
            patterns: EventPatterns::default_patterns(),
            buffer,
            position,
            line_buffer: String::new(),
        }
    }

    /// Poll for new content in the log file and extract events.
    ///
    /// Returns the number of new events extracted. Call this periodically
    /// from the supervisor loop.
    pub fn poll(&mut self) -> Result<usize> {
        let mut file = match std::fs::File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(0); // file doesn't exist yet
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("failed to open pipe log: {}", self.path.display()));
            }
        };

        // Clamp stale checkpoints (for example after truncation/rotation)
        let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
        if self.position > file_len {
            self.position = file_len;
        }

        // Seek to where we left off
        file.seek(SeekFrom::Start(self.position))
            .context("failed to seek in pipe log")?;

        // Read new content
        let mut new_bytes = Vec::new();
        let n = file
            .read_to_end(&mut new_bytes)
            .context("failed to read pipe log")?;

        if n == 0 {
            return Ok(0);
        }

        self.position += n as u64;

        // Convert to string (lossy for binary data in PTY output)
        let new_text = String::from_utf8_lossy(&new_bytes);
        self.line_buffer.push_str(&new_text);

        // Process complete lines
        let mut event_count = 0;
        while let Some(newline_pos) = self.line_buffer.find('\n') {
            let line = self.line_buffer[..newline_pos].to_string();
            self.line_buffer = self.line_buffer[newline_pos + 1..].to_string();

            let stripped = strip_ansi(&line);
            let trimmed = stripped.trim();
            if trimmed.is_empty() {
                continue;
            }

            if let Some(event) = self.patterns.classify(trimmed) {
                debug!(event = ?event, "extracted event");
                self.buffer.push(event);
                event_count += 1;
            }
        }

        Ok(event_count)
    }

    /// Get a reference to the event buffer.
    #[allow(dead_code)]
    pub fn buffer(&self) -> &EventBuffer {
        &self.buffer
    }

    /// Get the time of last activity (useful for silence detection).
    /// Returns None if no file modification detected.
    #[allow(dead_code)]
    pub fn last_activity(&self) -> Option<Instant> {
        if self.position > 0 {
            Some(Instant::now())
        } else {
            None
        }
    }

    /// Resume-safe checkpoint offset.
    ///
    /// This rewinds by the currently buffered partial line bytes so a resumed
    /// watcher can re-read any incomplete line safely.
    pub fn checkpoint_offset(&self) -> u64 {
        self.position
            .saturating_sub(self.line_buffer.len().try_into().unwrap_or(0))
    }
}

/// Run the pipe watcher in a polling loop until the stop flag is set.
///
/// This is meant to be spawned in a thread. It polls the log file at the
/// given interval and pushes events into the shared buffer.
#[allow(dead_code)]
pub fn run_watcher_loop(
    path: &Path,
    buffer: EventBuffer,
    poll_interval: std::time::Duration,
    stop: Arc<std::sync::atomic::AtomicBool>,
) -> Result<()> {
    let mut watcher = PipeWatcher::new(path, buffer);

    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        watcher.poll()?;
        std::thread::sleep(poll_interval);
    }

    // Final poll to catch any remaining content
    watcher.poll()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    // ── EventPatterns ──

    #[test]
    fn detect_task_started() {
        let patterns = EventPatterns::default_patterns();
        let event = patterns
            .classify("Picked and moved task #3: kanban reader")
            .unwrap();
        match event {
            PipeEvent::TaskStarted { task_id, title } => {
                assert_eq!(task_id, "3");
                assert_eq!(title, "kanban reader");
            }
            other => panic!("expected TaskStarted, got: {other:?}"),
        }
    }

    #[test]
    fn detect_task_started_claim() {
        let patterns = EventPatterns::default_patterns();
        let event = patterns.classify("Claimed task #5").unwrap();
        assert!(matches!(event, PipeEvent::TaskStarted { .. }));
    }

    #[test]
    fn detect_task_completed() {
        let patterns = EventPatterns::default_patterns();
        let event = patterns
            .classify("Moved task #3: in-progress -> done")
            .unwrap();
        match event {
            PipeEvent::TaskCompleted { task_id } => assert_eq!(task_id, "3"),
            other => panic!("expected TaskCompleted, got: {other:?}"),
        }
    }

    #[test]
    fn detect_commit() {
        let patterns = EventPatterns::default_patterns();
        let event = patterns
            .classify("[main abc1234] fix the auth bug")
            .unwrap();
        match event {
            PipeEvent::CommitMade { hash, message } => {
                assert_eq!(hash, "abc1234");
                assert_eq!(message, "fix the auth bug");
            }
            other => panic!("expected CommitMade, got: {other:?}"),
        }
    }

    #[test]
    fn detect_test_passed() {
        let patterns = EventPatterns::default_patterns();
        let event = patterns
            .classify("test result: ok. 42 passed; 0 failed")
            .unwrap();
        match event {
            PipeEvent::TestRan { passed, .. } => assert!(passed),
            other => panic!("expected TestRan, got: {other:?}"),
        }
    }

    #[test]
    fn detect_test_failed() {
        let patterns = EventPatterns::default_patterns();
        let event = patterns
            .classify("test result: FAILED. 40 passed; 2 failed")
            .unwrap();
        match event {
            PipeEvent::TestRan { passed, .. } => assert!(!passed),
            other => panic!("expected TestRan, got: {other:?}"),
        }
    }

    #[test]
    fn detect_file_created() {
        let patterns = EventPatterns::default_patterns();
        let event = patterns.classify("Created file src/tmux.rs").unwrap();
        match event {
            PipeEvent::FileCreated { path } => assert_eq!(path, "src/tmux.rs"),
            other => panic!("expected FileCreated, got: {other:?}"),
        }
    }

    #[test]
    fn detect_file_modified() {
        let patterns = EventPatterns::default_patterns();
        let event = patterns.classify("Edited src/main.rs").unwrap();
        match event {
            PipeEvent::FileModified { path } => assert_eq!(path, "src/main.rs"),
            other => panic!("expected FileModified, got: {other:?}"),
        }
    }

    #[test]
    fn detect_command_ran() {
        let patterns = EventPatterns::default_patterns();
        let event = patterns.classify("$ cargo test").unwrap();
        match event {
            PipeEvent::CommandRan { command, .. } => assert_eq!(command, "cargo test"),
            other => panic!("expected CommandRan, got: {other:?}"),
        }
    }

    #[test]
    fn detect_prompt() {
        let patterns = EventPatterns::default_patterns();
        let event = patterns
            .classify("Allow tool Read on /home/user/file.rs?")
            .unwrap();
        assert!(matches!(event, PipeEvent::PromptDetected { .. }));
    }

    #[test]
    fn no_match_on_normal_output() {
        let patterns = EventPatterns::default_patterns();
        assert!(
            patterns
                .classify("Writing function to parse YAML...")
                .is_none()
        );
    }

    // ── EventBuffer ──

    #[test]
    fn buffer_push_and_snapshot() {
        let buf = EventBuffer::new(3);
        buf.push(PipeEvent::OutputLine {
            line: "a".to_string(),
        });
        buf.push(PipeEvent::OutputLine {
            line: "b".to_string(),
        });

        let snap = buf.snapshot();
        assert_eq!(snap.len(), 2);
    }

    #[test]
    fn buffer_evicts_oldest_when_full() {
        let buf = EventBuffer::new(2);
        buf.push(PipeEvent::OutputLine {
            line: "a".to_string(),
        });
        buf.push(PipeEvent::OutputLine {
            line: "b".to_string(),
        });
        buf.push(PipeEvent::OutputLine {
            line: "c".to_string(),
        });

        let snap = buf.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(
            snap[0],
            PipeEvent::OutputLine {
                line: "b".to_string()
            }
        );
        assert_eq!(
            snap[1],
            PipeEvent::OutputLine {
                line: "c".to_string()
            }
        );
    }

    #[test]
    fn buffer_default_size() {
        let buf = EventBuffer::default_size();
        assert_eq!(buf.len(), 0);

        // Push 60 events — should keep only the last 50
        for i in 0..60 {
            buf.push(PipeEvent::OutputLine {
                line: format!("line {i}"),
            });
        }
        assert_eq!(buf.len(), 50);

        let snap = buf.snapshot();
        // First event should be line 10 (0-9 evicted)
        assert_eq!(
            snap[0],
            PipeEvent::OutputLine {
                line: "line 10".to_string()
            }
        );
    }

    #[test]
    fn buffer_clear() {
        let buf = EventBuffer::new(10);
        buf.push(PipeEvent::OutputLine {
            line: "x".to_string(),
        });
        assert_eq!(buf.len(), 1);

        buf.clear();
        assert!(buf.is_empty());
    }

    #[test]
    fn buffer_format_summary_empty() {
        let buf = EventBuffer::new(10);
        assert_eq!(buf.format_summary(), "(no events yet)");
    }

    #[test]
    fn buffer_format_summary_has_events() {
        let buf = EventBuffer::new(10);
        buf.push(PipeEvent::TaskStarted {
            task_id: "3".to_string(),
            title: "foo".to_string(),
        });
        buf.push(PipeEvent::FileCreated {
            path: "src/x.rs".to_string(),
        });
        buf.push(PipeEvent::TestRan {
            passed: true,
            detail: "ok".to_string(),
        });
        buf.push(PipeEvent::CommitMade {
            hash: "abc1234".to_string(),
            message: "fix".to_string(),
        });

        let summary = buf.format_summary();
        assert!(summary.contains("→ task #3 started: foo"));
        assert!(summary.contains("+ src/x.rs"));
        assert!(summary.contains("✓ test: ok"));
        assert!(summary.contains("⊕ commit abc1234: fix"));
    }

    #[test]
    fn buffer_is_thread_safe() {
        let buf = EventBuffer::new(100);
        let buf2 = buf.clone();

        let handle = std::thread::spawn(move || {
            for i in 0..50 {
                buf2.push(PipeEvent::OutputLine {
                    line: format!("thread {i}"),
                });
            }
        });

        for i in 0..50 {
            buf.push(PipeEvent::OutputLine {
                line: format!("main {i}"),
            });
        }

        handle.join().unwrap();
        assert_eq!(buf.len(), 100);
    }

    // ── PipeWatcher ──

    #[test]
    fn watcher_reads_new_content() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("pty-output.log");

        // Create the log file with some content
        {
            let mut f = fs::File::create(&log_path).unwrap();
            writeln!(f, "Picked and moved task #3: reader").unwrap();
            writeln!(f, "some normal output").unwrap();
            writeln!(f, "test result: ok. 5 passed; 0 failed").unwrap();
        }

        let buffer = EventBuffer::new(50);
        let mut watcher = PipeWatcher::new(&log_path, buffer.clone());

        let count = watcher.poll().unwrap();
        assert!(count >= 2, "expected at least 2 events, got {count}");

        let events = buffer.snapshot();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, PipeEvent::TaskStarted { .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, PipeEvent::TestRan { passed: true, .. }))
        );
    }

    #[test]
    fn watcher_tracks_position() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("pty-output.log");

        // Write initial content
        {
            let mut f = fs::File::create(&log_path).unwrap();
            writeln!(f, "test result: ok. 5 passed").unwrap();
        }

        let buffer = EventBuffer::new(50);
        let mut watcher = PipeWatcher::new(&log_path, buffer.clone());

        watcher.poll().unwrap();
        let count1 = buffer.len();

        // Poll again with no new content
        let count = watcher.poll().unwrap();
        assert_eq!(count, 0, "no new content should yield 0 events");
        assert_eq!(buffer.len(), count1);

        // Append new content
        {
            let mut f = fs::OpenOptions::new().append(true).open(&log_path).unwrap();
            writeln!(f, "[main abc1234] fix bug").unwrap();
        }

        let count = watcher.poll().unwrap();
        assert!(count >= 1, "expected at least 1 new event");
    }

    #[test]
    fn watcher_handles_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("nonexistent.log");

        let buffer = EventBuffer::new(50);
        let mut watcher = PipeWatcher::new(&log_path, buffer);

        // Should not error — just returns 0 events
        let count = watcher.poll().unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn watcher_resume_from_position_reads_only_new_content() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("resume.log");

        {
            let mut f = fs::File::create(&log_path).unwrap();
            writeln!(f, "test result: ok. 1 passed").unwrap();
        }

        let file_len = fs::metadata(&log_path).unwrap().len();
        let buffer = EventBuffer::new(50);
        let mut watcher = PipeWatcher::new_with_position(&log_path, buffer.clone(), file_len);

        {
            let mut f = fs::OpenOptions::new().append(true).open(&log_path).unwrap();
            writeln!(f, "[main abc1234] resume").unwrap();
        }

        let count = watcher.poll().unwrap();
        assert!(count >= 1);
        let events = buffer.snapshot();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, PipeEvent::CommitMade { .. }))
        );
    }

    #[test]
    fn watcher_checkpoint_offset_rewinds_partial_line() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("partial.log");

        {
            let mut f = fs::File::create(&log_path).unwrap();
            write!(f, "test result: ok").unwrap(); // no trailing newline
        }

        let buffer = EventBuffer::new(50);
        let mut watcher = PipeWatcher::new(&log_path, buffer.clone());
        let _ = watcher.poll().unwrap();

        assert_eq!(
            watcher.checkpoint_offset(),
            0,
            "partial line should be re-read on resume"
        );
        assert_eq!(buffer.len(), 0);
    }

    #[test]
    fn watcher_strips_ansi() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("ansi.log");

        {
            let mut f = fs::File::create(&log_path).unwrap();
            // Write ANSI-escaped "test result: ok"
            writeln!(f, "\x1b[32mtest result: ok. 5 passed\x1b[0m").unwrap();
        }

        let buffer = EventBuffer::new(50);
        let mut watcher = PipeWatcher::new(&log_path, buffer.clone());

        watcher.poll().unwrap();
        let events = buffer.snapshot();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, PipeEvent::TestRan { passed: true, .. }))
        );
    }

    // ── PipeEvent serialization ──

    #[test]
    fn pipe_event_serializes_to_json() {
        let event = PipeEvent::TaskStarted {
            task_id: "3".to_string(),
            title: "test".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"task_started\""));
        assert!(json.contains("\"task_id\":\"3\""));
    }

    #[test]
    fn all_pipe_events_serialize() {
        let events = vec![
            PipeEvent::TaskStarted {
                task_id: "1".to_string(),
                title: "test".to_string(),
            },
            PipeEvent::FileCreated {
                path: "x.rs".to_string(),
            },
            PipeEvent::FileModified {
                path: "y.rs".to_string(),
            },
            PipeEvent::CommandRan {
                command: "ls".to_string(),
                success: Some(true),
            },
            PipeEvent::TestRan {
                passed: true,
                detail: "ok".to_string(),
            },
            PipeEvent::PromptDetected {
                prompt: "y/n".to_string(),
            },
            PipeEvent::TaskCompleted {
                task_id: "1".to_string(),
            },
            PipeEvent::CommitMade {
                hash: "abc".to_string(),
                message: "fix".to_string(),
            },
            PipeEvent::OutputLine {
                line: "hi".to_string(),
            },
        ];

        for event in events {
            let json = serde_json::to_string(&event);
            assert!(json.is_ok(), "failed to serialize: {event:?}");
        }
    }

    // ── Coverage: additional events tests ──

    #[test]
    fn pipe_watcher_extracts_events_from_file() {
        let tmp = tempfile::tempdir().unwrap();
        let pipe_log = tmp.path().join("pipe.log");
        std::fs::write(
            &pipe_log,
            "some normal output\nRunning: cargo test\ntest result: ok. 5 passed\n",
        )
        .unwrap();

        let buffer = EventBuffer::new(10);
        let mut watcher = PipeWatcher::new(&pipe_log, buffer);
        let count = watcher.poll().unwrap();
        assert!(count > 0, "expected at least one event extracted");
    }

    #[test]
    fn pipe_watcher_handles_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let pipe_log = tmp.path().join("nonexistent.log");

        let buffer = EventBuffer::new(10);
        let mut watcher = PipeWatcher::new(&pipe_log, buffer);
        let count = watcher.poll().unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn pipe_watcher_incremental_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let pipe_log = tmp.path().join("pipe.log");
        std::fs::write(&pipe_log, "Running: first command\n").unwrap();

        let buffer = EventBuffer::new(10);
        let mut watcher = PipeWatcher::new(&pipe_log, buffer);
        let count1 = watcher.poll().unwrap();
        assert!(count1 > 0, "should extract events from first write");

        // Append more content
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&pipe_log)
            .unwrap();
        writeln!(f, "Running: second command").unwrap();

        let count2 = watcher.poll().unwrap();
        assert!(count2 > 0, "should extract events from appended content");
    }

    #[test]
    fn pipe_watcher_clamps_stale_position() {
        let tmp = tempfile::tempdir().unwrap();
        let pipe_log = tmp.path().join("pipe.log");
        std::fs::write(&pipe_log, "short\n").unwrap();

        let buffer = EventBuffer::new(10);
        // Start with position beyond file
        let mut watcher = PipeWatcher::new_with_position(&pipe_log, buffer, 99999);
        let count = watcher.poll().unwrap();
        assert_eq!(count, 0, "should clamp and read nothing new");
    }

    #[test]
    fn pipe_watcher_partial_line_buffering() {
        let tmp = tempfile::tempdir().unwrap();
        let pipe_log = tmp.path().join("pipe.log");
        // Write content WITHOUT trailing newline
        std::fs::write(&pipe_log, "Running: partial").unwrap();

        let buffer = EventBuffer::new(10);
        let mut watcher = PipeWatcher::new(&pipe_log, buffer);
        let count1 = watcher.poll().unwrap();
        assert_eq!(
            count1, 0,
            "incomplete line should be buffered, not processed"
        );

        // Now complete the line
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&pipe_log)
            .unwrap();
        writeln!(f, " command").unwrap();

        let count2 = watcher.poll().unwrap();
        assert!(
            count2 > 0,
            "completed line should now be processed as event"
        );
    }

    #[test]
    fn format_summary_truncates_long_output_line() {
        let buffer = EventBuffer::new(10);
        let long_line = "x".repeat(100);
        buffer.push(PipeEvent::OutputLine {
            line: long_line.clone(),
        });
        let summary = buffer.format_summary();
        assert!(
            summary.contains("..."),
            "long output line should be truncated with ..."
        );
        assert!(summary.len() < long_line.len() + 20);
    }

    #[test]
    fn format_summary_includes_all_event_types() {
        let buffer = EventBuffer::new(10);
        buffer.push(PipeEvent::CommandRan {
            command: "ls".to_string(),
            success: Some(true),
        });
        buffer.push(PipeEvent::TestRan {
            passed: true,
            detail: "all good".to_string(),
        });
        buffer.push(PipeEvent::CommitMade {
            hash: "abc1234def".to_string(),
            message: "fix bug".to_string(),
        });
        buffer.push(PipeEvent::OutputLine {
            line: "hello".to_string(),
        });

        let summary = buffer.format_summary();
        assert!(summary.contains("$ ls"));
        assert!(summary.contains("✓ test:"));
        assert!(summary.contains("⊕ commit abc1234:"));
        assert!(summary.contains("hello"));
    }

    #[test]
    fn event_buffer_respects_capacity() {
        let buffer = EventBuffer::new(3);
        buffer.push(PipeEvent::OutputLine {
            line: "a".to_string(),
        });
        buffer.push(PipeEvent::OutputLine {
            line: "b".to_string(),
        });
        buffer.push(PipeEvent::OutputLine {
            line: "c".to_string(),
        });
        buffer.push(PipeEvent::OutputLine {
            line: "d".to_string(),
        });

        let summary = buffer.format_summary();
        assert!(!summary.contains("  a"), "oldest event should be evicted");
        assert!(summary.contains("  d"), "newest event should be present");
    }
}
