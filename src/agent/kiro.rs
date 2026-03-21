//! Kiro CLI adapter.
//!
//! The locally installed Kiro CLI exposes `kiro chat [prompt]` with window and
//! mode flags, but not stable session-management flags. Batty therefore treats
//! Kiro as a launchable interactive backend without explicit resume support.
#![cfg_attr(not(test), allow(dead_code))]

use std::path::Path;

use crate::agent::{AgentAdapter, SpawnConfig};
use crate::prompt::PromptPatterns;

/// Adapter for the Kiro CLI.
pub struct KiroCliAdapter {
    /// Override the kiro binary name/path (default: "kiro").
    program: String,
}

impl KiroCliAdapter {
    pub fn new(program: Option<String>) -> Self {
        Self {
            program: program.unwrap_or_else(|| "kiro".to_string()),
        }
    }
}

impl AgentAdapter for KiroCliAdapter {
    fn name(&self) -> &str {
        "kiro-cli"
    }

    fn spawn_config(&self, task_description: &str, work_dir: &Path) -> SpawnConfig {
        SpawnConfig {
            program: self.program.clone(),
            args: vec![
                "chat".to_string(),
                "--mode".to_string(),
                "agent".to_string(),
                task_description.to_string(),
            ],
            work_dir: work_dir.to_string_lossy().to_string(),
            env: vec![],
        }
    }

    fn prompt_patterns(&self) -> PromptPatterns {
        PromptPatterns::kiro_cli()
    }

    fn instruction_candidates(&self) -> &'static [&'static str] {
        &["AGENTS.md", "CLAUDE.md"]
    }

    fn wrap_launch_prompt(&self, prompt: &str) -> String {
        format!(
            "You are running as Kiro under Batty supervision.\n\
             Treat the launch context below as authoritative session context.\n\n\
             {prompt}"
        )
    }

    fn format_input(&self, response: &str) -> String {
        format!("{response}\n")
    }

    fn reset_context_keys(&self) -> Vec<(String, bool)> {
        vec![("C-c".to_string(), false)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_program_is_kiro() {
        let adapter = KiroCliAdapter::new(None);
        let config = adapter.spawn_config("test", Path::new("/tmp"));
        assert_eq!(config.program, "kiro");
    }

    #[test]
    fn spawn_uses_chat_agent_mode() {
        let adapter = KiroCliAdapter::new(None);
        let config = adapter.spawn_config("ship the patch", Path::new("/tmp/worktree"));
        assert_eq!(
            config.args,
            vec![
                "chat".to_string(),
                "--mode".to_string(),
                "agent".to_string(),
                "ship the patch".to_string()
            ]
        );
        assert_eq!(config.work_dir, "/tmp/worktree");
    }

    #[test]
    fn kiro_prefers_agents_md_instruction_order() {
        let adapter = KiroCliAdapter::new(None);
        assert_eq!(
            adapter.instruction_candidates(),
            &["AGENTS.md", "CLAUDE.md"]
        );
    }

    #[test]
    fn kiro_wraps_launch_prompt() {
        let adapter = KiroCliAdapter::new(None);
        let wrapped = adapter.wrap_launch_prompt("Launch body");
        assert!(wrapped.contains("Kiro under Batty supervision"));
        assert!(wrapped.contains("Launch body"));
    }
}
