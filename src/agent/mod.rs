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
#![cfg_attr(not(test), allow(dead_code))]

pub mod claude;
pub mod codex;
pub mod kiro;
pub mod mock;

use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::prompt::PromptPatterns;

/// Health state of an agent backend.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendHealth {
    /// Backend binary found and responsive.
    #[default]
    Healthy,
    /// Backend binary found but returning errors (e.g. API issues).
    Degraded,
    /// Backend binary not found or not executable.
    Unreachable,
    /// Backend quota/billing limit exhausted — agent cannot work until credits are added.
    QuotaExhausted,
}

impl BackendHealth {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::QuotaExhausted => "quota_exhausted",
            Self::Unreachable => "unreachable",
        }
    }

    pub fn is_healthy(self) -> bool {
        self == Self::Healthy
    }
}

/// Check if a binary is available on PATH.
fn check_binary_available(program: &str) -> BackendHealth {
    match Command::new("which").arg(program).output() {
        Ok(output) if output.status.success() => BackendHealth::Healthy,
        _ => BackendHealth::Unreachable,
    }
}

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

    /// Preferred project-root instruction file candidates for this agent.
    ///
    /// The first existing file is used as launch context. Adapters can
    /// override this to prefer agent-specific steering docs.
    fn instruction_candidates(&self) -> &'static [&'static str] {
        &["CLAUDE.md", "AGENTS.md"]
    }

    /// Allow adapters to wrap or transform the composed launch context.
    ///
    /// Default behavior is passthrough. Adapters can prepend guardrails or
    /// framing tailored to their CLI behavior.
    fn wrap_launch_prompt(&self, prompt: &str) -> String {
        prompt.to_string()
    }

    /// Format a response to send to the agent's stdin.
    ///
    /// Some agents need a trailing newline, some don't. The adapter handles it.
    fn format_input(&self, response: &str) -> String;

    // --- Launch lifecycle methods (Backend trait) ---

    /// Build the shell command to launch this agent.
    ///
    /// Returns the `exec <agent> ...` command string that will be written into
    /// the launch script. Each backend encodes its own CLI flags, resume
    /// semantics, and idle/prompt handling.
    fn launch_command(
        &self,
        prompt: &str,
        idle: bool,
        resume: bool,
        session_id: Option<&str>,
    ) -> anyhow::Result<String>;

    /// Generate a new session ID for this backend, if supported.
    ///
    /// Backends that support session resume (e.g., Claude Code) return
    /// `Some(uuid)`. Backends without session management return `None`.
    fn new_session_id(&self) -> Option<String> {
        None
    }

    /// Whether this backend supports resuming a previous session.
    fn supports_resume(&self) -> bool {
        false
    }

    /// Check if this agent's backend is healthy (binary available, etc.).
    fn health_check(&self) -> BackendHealth {
        BackendHealth::Healthy
    }
}

/// Known agent backend names (primary aliases only).
pub const KNOWN_AGENT_NAMES: &[&str] = &["claude", "codex", "kiro-cli"];

/// Look up an agent adapter by name.
///
/// Returns `None` if the agent name is not recognized. New adapters are
/// registered here as they're implemented.
pub fn adapter_from_name(name: &str) -> Option<Box<dyn AgentAdapter>> {
    match name {
        "claude" | "claude-code" => Some(Box::new(claude::ClaudeCodeAdapter::new(None))),
        "codex" | "codex-cli" => Some(Box::new(codex::CodexCliAdapter::new(None))),
        "kiro" | "kiro-cli" => Some(Box::new(kiro::KiroCliAdapter::new(None))),
        _ => None,
    }
}

/// Check backend health for a named agent.
///
/// Returns `None` if the agent name is not recognized.
pub fn health_check_by_name(agent_name: &str) -> Option<BackendHealth> {
    adapter_from_name(agent_name).map(|adapter| adapter.health_check())
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

        let adapter = adapter_from_name("kiro").unwrap();
        assert_eq!(adapter.name(), "kiro-cli");

        let adapter = adapter_from_name("kiro-cli").unwrap();
        assert_eq!(adapter.name(), "kiro-cli");

        assert!(adapter_from_name("unknown-agent").is_none());
    }

    #[test]
    fn default_instruction_candidates_include_claude_and_agents() {
        let adapter = claude::ClaudeCodeAdapter::new(None);
        assert_eq!(
            adapter.instruction_candidates(),
            &["CLAUDE.md", "AGENTS.md"]
        );
    }

    #[test]
    fn default_wrap_launch_prompt_is_passthrough() {
        let adapter = claude::ClaudeCodeAdapter::new(None);
        let prompt = "test launch prompt";
        assert_eq!(adapter.wrap_launch_prompt(prompt), prompt);
    }

    // --- Backend trait dispatch tests ---

    #[test]
    fn launch_command_dispatches_through_trait_object() {
        let backends: Vec<Box<dyn AgentAdapter>> = vec![
            Box::new(claude::ClaudeCodeAdapter::new(None)),
            Box::new(codex::CodexCliAdapter::new(None)),
            Box::new(kiro::KiroCliAdapter::new(None)),
        ];
        for backend in &backends {
            let cmd = backend.launch_command("test prompt", true, false, None);
            assert!(cmd.is_ok(), "launch_command failed for {}", backend.name());
            assert!(
                !cmd.unwrap().is_empty(),
                "launch_command empty for {}",
                backend.name()
            );
        }
    }

    #[test]
    fn supports_resume_varies_by_backend() {
        let claude = adapter_from_name("claude").unwrap();
        let codex = adapter_from_name("codex").unwrap();
        let kiro = adapter_from_name("kiro").unwrap();
        assert!(claude.supports_resume());
        assert!(codex.supports_resume());
        assert!(kiro.supports_resume()); // ACP supports session/load
    }

    #[test]
    fn new_session_id_varies_by_backend() {
        let claude = adapter_from_name("claude").unwrap();
        let codex = adapter_from_name("codex").unwrap();
        let kiro = adapter_from_name("kiro").unwrap();
        assert!(claude.new_session_id().is_some());
        assert!(codex.new_session_id().is_none());
        assert!(kiro.new_session_id().is_some());
    }

    #[test]
    fn backend_health_default_is_healthy() {
        assert_eq!(BackendHealth::default(), BackendHealth::Healthy);
    }

    #[test]
    fn backend_health_as_str() {
        assert_eq!(BackendHealth::Healthy.as_str(), "healthy");
        assert_eq!(BackendHealth::Degraded.as_str(), "degraded");
        assert_eq!(BackendHealth::Unreachable.as_str(), "unreachable");
    }

    #[test]
    fn backend_health_is_healthy() {
        assert!(BackendHealth::Healthy.is_healthy());
        assert!(!BackendHealth::Degraded.is_healthy());
        assert!(!BackendHealth::Unreachable.is_healthy());
    }

    #[test]
    fn health_check_by_name_returns_none_for_unknown() {
        assert!(health_check_by_name("unknown-agent").is_none());
    }

    #[test]
    fn health_check_for_nonexistent_binary_returns_unreachable() {
        let adapter =
            claude::ClaudeCodeAdapter::new(Some("/nonexistent/path/to/claude-9999".to_string()));
        assert_eq!(adapter.health_check(), BackendHealth::Unreachable);
    }

    #[test]
    fn check_binary_available_finds_bash() {
        // bash should always be present on the system
        assert_eq!(check_binary_available("bash"), BackendHealth::Healthy);
    }

    #[test]
    fn check_binary_available_returns_unreachable_for_missing() {
        assert_eq!(
            check_binary_available("nonexistent-binary-12345"),
            BackendHealth::Unreachable,
        );
    }
}
