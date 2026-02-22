//! Claude Code adapter.
//!
//! Targets `claude` CLI in print mode (`-p --output-format stream-json`)
//! for reliable prompt detection via structured JSON output, with fallback
//! patterns for interactive mode.

use std::path::Path;

use crate::agent::{AgentAdapter, SpawnConfig};
use crate::prompt::PromptPatterns;

/// Adapter for Claude Code CLI.
pub struct ClaudeCodeAdapter {
    /// Override the claude binary name/path (default: "claude").
    program: String,
}

impl ClaudeCodeAdapter {
    pub fn new(program: Option<String>) -> Self {
        Self {
            program: program.unwrap_or_else(|| "claude".to_string()),
        }
    }
}

impl AgentAdapter for ClaudeCodeAdapter {
    fn name(&self) -> &str {
        "claude-code"
    }

    fn spawn_config(&self, task_description: &str, work_dir: &Path) -> SpawnConfig {
        SpawnConfig {
            program: self.program.clone(),
            args: vec![
                "-p".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                task_description.to_string(),
            ],
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
    fn spawn_uses_print_mode() {
        let adapter = ClaudeCodeAdapter::new(None);
        let config = adapter.spawn_config("Fix the auth bug", Path::new("/work"));
        assert!(config.args.contains(&"-p".to_string()));
        assert!(config.args.contains(&"stream-json".to_string()));
    }

    #[test]
    fn spawn_sets_work_dir() {
        let adapter = ClaudeCodeAdapter::new(None);
        let config = adapter.spawn_config("task", Path::new("/my/worktree"));
        assert_eq!(config.work_dir, "/my/worktree");
    }

    #[test]
    fn prompt_patterns_detect_claude_prompts() {
        let adapter = ClaudeCodeAdapter::new(None);
        let patterns = adapter.prompt_patterns();
        // Verify patterns work (detailed tests are in prompt module)
        assert!(patterns.detect("Allow tool Read?").is_some());
        assert!(patterns.detect("just normal output").is_none());
    }

    #[test]
    fn name_is_claude_code() {
        let adapter = ClaudeCodeAdapter::new(None);
        assert_eq!(adapter.name(), "claude-code");
    }
}
