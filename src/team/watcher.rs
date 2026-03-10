//! Disk-based session monitoring — polls agent output via tmux capture-pane.
//!
//! Detects agent completion, crashes, and staleness by periodically capturing
//! pane output and checking for state changes.

use std::time::{Duration, Instant};

use anyhow::Result;

use crate::tmux;

/// State of a watched agent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatcherState {
    /// Agent is actively producing output.
    Active,
    /// Agent completed its task (returned to shell or exited).
    Completed,
    /// No agent running in pane (idle / waiting for assignment).
    Idle,
    /// No new output for longer than the stale threshold.
    Stale,
}

pub struct SessionWatcher {
    pub pane_id: String,
    pub member_name: String,
    pub state: WatcherState,
    last_output_hash: u64,
    last_change: Instant,
    last_capture: String,
    stale_threshold: Duration,
}

impl SessionWatcher {
    pub fn new(pane_id: &str, member_name: &str, stale_secs: u64) -> Self {
        Self {
            pane_id: pane_id.to_string(),
            member_name: member_name.to_string(),
            state: WatcherState::Idle,
            last_output_hash: 0,
            last_change: Instant::now(),
            last_capture: String::new(),
            stale_threshold: Duration::from_secs(stale_secs),
        }
    }

    /// Poll the pane and update state.
    pub fn poll(&mut self) -> Result<WatcherState> {
        // Check if pane still exists
        if !tmux::pane_exists(&self.pane_id) {
            self.state = WatcherState::Completed;
            return Ok(self.state);
        }

        // Check if pane process died
        if tmux::pane_dead(&self.pane_id).unwrap_or(false) {
            self.state = WatcherState::Completed;
            return Ok(self.state);
        }

        // If idle, stay idle until explicitly activated
        if self.state == WatcherState::Idle {
            return Ok(self.state);
        }

        // Capture current pane content
        let capture = tmux::capture_pane(&self.pane_id).unwrap_or_default();
        let hash = simple_hash(&capture);

        if hash != self.last_output_hash {
            self.last_output_hash = hash;
            self.last_change = Instant::now();
            self.last_capture = capture;
            self.state = WatcherState::Active;
        } else {
            // Output hasn't changed — check if agent is back at idle prompt
            let idle_secs = self.last_change.elapsed().as_secs();
            if idle_secs >= 15 && is_at_agent_prompt(&self.last_capture) {
                // Agent finished work and is sitting at prompt
                self.state = WatcherState::Completed;
            } else if self.last_change.elapsed() > self.stale_threshold {
                self.state = WatcherState::Stale;
            }
        }

        Ok(self.state)
    }

    /// Mark this watcher as actively working.
    pub fn activate(&mut self) {
        self.state = WatcherState::Active;
        self.last_change = Instant::now();
        self.last_output_hash = 0;
    }

    /// Mark this watcher as idle.
    pub fn deactivate(&mut self) {
        self.state = WatcherState::Idle;
    }

    /// Get the last captured pane output.
    pub fn last_output(&self) -> &str {
        &self.last_capture
    }

    /// Get the last N lines of captured output.
    pub fn last_lines(&self, n: usize) -> String {
        let lines: Vec<&str> = self.last_capture.lines().collect();
        let start = lines.len().saturating_sub(n);
        lines[start..].join("\n")
    }
}

/// Check if the captured pane output shows a Claude Code idle prompt.
///
/// Claude Code shows `❯` on an empty line when waiting for input.
/// We also check for the bash `$` prompt in case the agent exited.
fn is_at_agent_prompt(capture: &str) -> bool {
    let trimmed: Vec<&str> = capture
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(5)
        .collect();

    for line in &trimmed {
        let l = line.trim();
        // Claude Code idle prompt
        if l == "❯" || l.starts_with("❯ ") {
            return true;
        }
        // Fell back to shell
        if l.ends_with("$ ") || l == "$" {
            return true;
        }
    }
    false
}

fn simple_hash(s: &str) -> u64 {
    // FNV-1a style hash, good enough for change detection
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in s.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_hash_differs_for_different_input() {
        assert_ne!(simple_hash("hello"), simple_hash("world"));
        assert_eq!(simple_hash("same"), simple_hash("same"));
    }

    #[test]
    fn new_watcher_starts_idle() {
        let w = SessionWatcher::new("%0", "eng-1-1", 300);
        assert_eq!(w.state, WatcherState::Idle);
    }

    #[test]
    fn activate_sets_active() {
        let mut w = SessionWatcher::new("%0", "eng-1-1", 300);
        w.activate();
        assert_eq!(w.state, WatcherState::Active);
    }

    #[test]
    fn deactivate_sets_idle() {
        let mut w = SessionWatcher::new("%0", "eng-1-1", 300);
        w.activate();
        w.deactivate();
        assert_eq!(w.state, WatcherState::Idle);
    }

    #[test]
    fn last_lines_returns_tail() {
        let mut w = SessionWatcher::new("%0", "eng-1-1", 300);
        w.last_capture = "line1\nline2\nline3\nline4\nline5".to_string();
        assert_eq!(w.last_lines(3), "line3\nline4\nline5");
        assert_eq!(w.last_lines(10), "line1\nline2\nline3\nline4\nline5");
    }

    #[test]
    fn detects_claude_code_prompt() {
        let capture = "⏺ Done.\n\n❯ \n\n  bypass permissions\n";
        assert!(is_at_agent_prompt(capture));
    }

    #[test]
    fn detects_shell_prompt() {
        let capture = "some output\n$ \n";
        assert!(is_at_agent_prompt(capture));
    }

    #[test]
    fn no_prompt_when_working() {
        let capture = "⏺ Bash(python -m pytest)\n  ⎿  running tests...\n";
        assert!(!is_at_agent_prompt(capture));
    }
}
