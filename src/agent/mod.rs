//! Agent adapter layer.
//!
//! Each coding agent (Claude Code, Codex CLI, Aider) is wrapped in an adapter
//! that knows how to:
//! - Build the command to spawn the agent in a PTY
//! - Provide prompt detection patterns for its output
//! - Format input to inject into the agent's stdin
//!
//! The supervisor uses this trait to control agents without knowing their
//! specific CLI conventions.

pub mod claude;
pub mod codex;

use std::path::Path;

use crate::prompt::PromptPatterns;

/// Configuration for spawning an agent process.
#[derive(Debug, Clone)]
pub struct SpawnConfig {
    /// The program to execute (e.g., "claude", "codex", "aider").
    pub program: String,
    /// Arguments to pass to the program.
    pub args: Vec<String>,
    /// Working directory for the agent process.
    pub work_dir: String,
    /// Environment variables to set (key, value pairs).
    pub env: Vec<(String, String)>,
}

/// Trait that all agent adapters must implement.
///
/// An adapter translates between Batty's supervisor and a specific agent CLI.
/// It does not own the PTY or process — the supervisor does that. The adapter
/// only provides the configuration and patterns needed to drive the agent.
pub trait AgentAdapter: Send + Sync {
    /// Human-readable name of the agent (e.g., "claude-code", "codex", "aider").
    fn name(&self) -> &str;

    /// Build the spawn configuration for this agent.
    ///
    /// `task_description` is the task text to pass to the agent.
    /// `work_dir` is the worktree path where the agent should operate.
    fn spawn_config(&self, task_description: &str, work_dir: &Path) -> SpawnConfig;

    /// Get the compiled prompt detection patterns for this agent.
    fn prompt_patterns(&self) -> PromptPatterns;

    /// Format a response to send to the agent's stdin.
    ///
    /// Some agents need a trailing newline, some don't. The adapter handles it.
    #[allow(dead_code)]
    fn format_input(&self, response: &str) -> String;
}

/// Look up an agent adapter by name.
///
/// Returns `None` if the agent name is not recognized. New adapters are
/// registered here as they're implemented.
pub fn adapter_from_name(name: &str) -> Option<Box<dyn AgentAdapter>> {
    match name {
        "claude" | "claude-code" => Some(Box::new(claude::ClaudeCodeAdapter::new(None))),
        "codex" | "codex-cli" => Some(Box::new(codex::CodexCliAdapter::new(None))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Verify the trait is object-safe (can be used as dyn AgentAdapter)
    #[test]
    fn trait_is_object_safe() {
        fn _accepts_dyn(_adapter: &dyn AgentAdapter) {}
        let adapter = claude::ClaudeCodeAdapter::new(None);
        _accepts_dyn(&adapter);
    }

    #[test]
    fn spawn_config_has_work_dir() {
        let adapter = claude::ClaudeCodeAdapter::new(None);
        let config = adapter.spawn_config("Fix the bug", Path::new("/tmp/worktree"));
        assert_eq!(config.work_dir, "/tmp/worktree");
    }

    #[test]
    fn spawn_config_includes_task_in_args() {
        let adapter = claude::ClaudeCodeAdapter::new(None);
        let config = adapter.spawn_config("Fix the bug", Path::new("/tmp/worktree"));
        let args_joined = config.args.join(" ");
        assert!(
            args_joined.contains("Fix the bug"),
            "task description should appear in args: {args_joined}"
        );
    }

    #[test]
    fn format_input_appends_newline() {
        let adapter = claude::ClaudeCodeAdapter::new(None);
        let input = adapter.format_input("y");
        assert_eq!(input, "y\n");
    }

    #[test]
    fn lookup_adapter_by_name() {
        let adapter = adapter_from_name("claude").unwrap();
        assert_eq!(adapter.name(), "claude-code");

        let adapter = adapter_from_name("claude-code").unwrap();
        assert_eq!(adapter.name(), "claude-code");

        let adapter = adapter_from_name("codex").unwrap();
        assert_eq!(adapter.name(), "codex-cli");

        let adapter = adapter_from_name("codex-cli").unwrap();
        assert_eq!(adapter.name(), "codex-cli");

        assert!(adapter_from_name("unknown-agent").is_none());
    }
}
