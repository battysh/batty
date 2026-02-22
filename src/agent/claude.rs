//! Claude Code adapter.
//!
//! Supports two modes:
//! - **Print mode** (`-p --output-format stream-json`): for automated runs
//!   where structured JSON output enables reliable completion/error detection.
//! - **Interactive mode** (no `-p`): for supervised runs where the user can
//!   see and type into Claude's native TUI. Batty supervises on top without
//!   breaking the interactive experience.
//!
//! The supervisor decides which mode to use. The adapter provides spawn
//! configs and prompt patterns for both.

use std::path::Path;

use crate::agent::{AgentAdapter, SpawnConfig};
use crate::prompt::PromptPatterns;

/// How to run Claude Code.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub enum ClaudeMode {
    /// Print mode: `-p --output-format stream-json`.
    /// Best for fully automated runs. Structured JSON output.
    #[allow(dead_code)]
    Print,
    /// Interactive mode: user sees the full TUI.
    /// Batty supervises via PTY pattern matching on ANSI-stripped output.
    #[default]
    Interactive,
}

/// Adapter for Claude Code CLI.
pub struct ClaudeCodeAdapter {
    /// Override the claude binary name/path (default: "claude").
    program: String,
    /// Which mode to run Claude in.
    mode: ClaudeMode,
}

impl ClaudeCodeAdapter {
    pub fn new(program: Option<String>) -> Self {
        Self {
            program: program.unwrap_or_else(|| "claude".to_string()),
            mode: ClaudeMode::default(),
        }
    }

    #[allow(dead_code)]
    pub fn with_mode(mut self, mode: ClaudeMode) -> Self {
        self.mode = mode;
        self
    }

    #[allow(dead_code)]
    pub fn mode(&self) -> ClaudeMode {
        self.mode
    }
}

impl AgentAdapter for ClaudeCodeAdapter {
    fn name(&self) -> &str {
        "claude-code"
    }

    fn spawn_config(&self, task_description: &str, work_dir: &Path) -> SpawnConfig {
        let mut args = Vec::new();

        match self.mode {
            ClaudeMode::Print => {
                args.push("-p".to_string());
                args.push("--output-format".to_string());
                args.push("stream-json".to_string());
                args.push(task_description.to_string());
            }
            ClaudeMode::Interactive => {
                // In interactive mode, we pass the task as the initial prompt
                // via --prompt so Claude starts working immediately.
                // The user can still type into the session at any time.
                args.push("--prompt".to_string());
                args.push(task_description.to_string());
            }
        }

        SpawnConfig {
            program: self.program.clone(),
            args,
            work_dir: work_dir.to_string_lossy().to_string(),
            env: vec![],
        }
    }

    fn prompt_patterns(&self) -> PromptPatterns {
        PromptPatterns::claude_code()
    }

    fn format_input(&self, response: &str) -> String {
        format!("{response}\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_program_is_claude() {
        let adapter = ClaudeCodeAdapter::new(None);
        let config = adapter.spawn_config("test", Path::new("/tmp"));
        assert_eq!(config.program, "claude");
    }

    #[test]
    fn custom_program_path() {
        let adapter = ClaudeCodeAdapter::new(Some("/usr/local/bin/claude".to_string()));
        let config = adapter.spawn_config("test", Path::new("/tmp"));
        assert_eq!(config.program, "/usr/local/bin/claude");
    }

    #[test]
    fn default_mode_is_interactive() {
        let adapter = ClaudeCodeAdapter::new(None);
        assert_eq!(adapter.mode(), ClaudeMode::Interactive);
    }

    #[test]
    fn print_mode_uses_p_flag_and_stream_json() {
        let adapter = ClaudeCodeAdapter::new(None).with_mode(ClaudeMode::Print);
        let config = adapter.spawn_config("Fix the auth bug", Path::new("/work"));
        assert!(config.args.contains(&"-p".to_string()));
        assert!(config.args.contains(&"stream-json".to_string()));
        assert!(config.args.contains(&"Fix the auth bug".to_string()));
    }

    #[test]
    fn interactive_mode_uses_prompt_flag() {
        let adapter = ClaudeCodeAdapter::new(None).with_mode(ClaudeMode::Interactive);
        let config = adapter.spawn_config("Fix the auth bug", Path::new("/work"));
        assert!(!config.args.contains(&"-p".to_string()));
        assert!(config.args.contains(&"--prompt".to_string()));
        assert!(config.args.contains(&"Fix the auth bug".to_string()));
    }

    #[test]
    fn spawn_sets_work_dir() {
        let adapter = ClaudeCodeAdapter::new(None);
        let config = adapter.spawn_config("task", Path::new("/my/worktree"));
        assert_eq!(config.work_dir, "/my/worktree");
    }

    #[test]
    fn prompt_patterns_detect_permission() {
        let adapter = ClaudeCodeAdapter::new(None);
        let patterns = adapter.prompt_patterns();
        let d = patterns.detect("Allow tool Read on /home/user/file.rs?");
        assert!(d.is_some());
        assert!(matches!(
            d.unwrap().kind,
            crate::prompt::PromptKind::Permission { .. }
        ));
    }

    #[test]
    fn prompt_patterns_detect_continuation() {
        let adapter = ClaudeCodeAdapter::new(None);
        let patterns = adapter.prompt_patterns();
        let d = patterns.detect("Continue? [y/n]");
        assert!(d.is_some());
        assert!(matches!(
            d.unwrap().kind,
            crate::prompt::PromptKind::Confirmation { .. }
        ));
    }

    #[test]
    fn prompt_patterns_detect_completion_in_json() {
        let adapter = ClaudeCodeAdapter::new(None);
        let patterns = adapter.prompt_patterns();
        let d = patterns.detect(r#"{"type": "result", "subtype": "success"}"#);
        assert!(d.is_some());
        assert_eq!(d.unwrap().kind, crate::prompt::PromptKind::Completion);
    }

    #[test]
    fn prompt_patterns_detect_error_in_json() {
        let adapter = ClaudeCodeAdapter::new(None);
        let patterns = adapter.prompt_patterns();
        let d = patterns.detect(r#"{"type": "result", "is_error": true}"#);
        assert!(d.is_some());
        assert!(matches!(
            d.unwrap().kind,
            crate::prompt::PromptKind::Error { .. }
        ));
    }

    #[test]
    fn prompt_patterns_no_match_on_normal_output() {
        let adapter = ClaudeCodeAdapter::new(None);
        let patterns = adapter.prompt_patterns();
        assert!(
            patterns
                .detect("Writing function to parse YAML...")
                .is_none()
        );
    }

    #[test]
    fn format_input_appends_newline() {
        let adapter = ClaudeCodeAdapter::new(None);
        assert_eq!(adapter.format_input("y"), "y\n");
        assert_eq!(adapter.format_input("yes"), "yes\n");
    }

    #[test]
    fn name_is_claude_code() {
        let adapter = ClaudeCodeAdapter::new(None);
        assert_eq!(adapter.name(), "claude-code");
    }
}
