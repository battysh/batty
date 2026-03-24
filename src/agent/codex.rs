//! Codex CLI adapter.
//!
//! Runs Codex in interactive mode by default, passing the composed task prompt
//! as the initial user prompt argument.
#![cfg_attr(not(test), allow(dead_code))]

use std::path::Path;

use anyhow::Context;

use crate::agent::{AgentAdapter, SpawnConfig};
use crate::prompt::PromptPatterns;

/// Adapter for Codex CLI.
pub struct CodexCliAdapter {
    /// Override the codex binary name/path (default: "codex").
    program: String,
}

impl CodexCliAdapter {
    pub fn new(program: Option<String>) -> Self {
        Self {
            program: program.unwrap_or_else(|| "codex".to_string()),
        }
    }
}

impl AgentAdapter for CodexCliAdapter {
    fn name(&self) -> &str {
        "codex-cli"
    }

    fn spawn_config(&self, task_description: &str, work_dir: &Path) -> SpawnConfig {
        SpawnConfig {
            program: self.program.clone(),
            args: vec![task_description.to_string()],
            work_dir: work_dir.to_string_lossy().to_string(),
            env: vec![],
        }
    }

    fn prompt_patterns(&self) -> PromptPatterns {
        PromptPatterns::codex_cli()
    }

    fn instruction_candidates(&self) -> &'static [&'static str] {
        &["AGENTS.md", "CLAUDE.md"]
    }

    fn wrap_launch_prompt(&self, prompt: &str) -> String {
        format!(
            "You are running as Codex under Batty supervision.\n\
             Treat the launch context below as authoritative session context.\n\n\
             {prompt}"
        )
    }

    fn format_input(&self, response: &str) -> String {
        format!("{response}\n")
    }

    fn launch_command(
        &self,
        prompt: &str,
        idle: bool,
        resume: bool,
        session_id: Option<&str>,
    ) -> anyhow::Result<String> {
        let escaped = prompt.replace('\'', "'\\''");
        if resume {
            let sid = session_id.context("missing Codex session ID for resume")?;
            Ok(format!(
                "exec codex resume '{sid}' --dangerously-bypass-approvals-and-sandbox"
            ))
        } else {
            let prefix = "exec codex --dangerously-bypass-approvals-and-sandbox";
            if idle {
                Ok(prefix.to_string())
            } else {
                Ok(format!("{prefix} '{escaped}'"))
            }
        }
    }

    fn supports_resume(&self) -> bool {
        true
    }

    fn health_check(&self) -> super::BackendHealth {
        super::check_binary_available(&self.program)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_program_is_codex() {
        let adapter = CodexCliAdapter::new(None);
        let config = adapter.spawn_config("test", Path::new("/tmp"));
        assert_eq!(config.program, "codex");
    }

    #[test]
    fn custom_program_path() {
        let adapter = CodexCliAdapter::new(Some("/usr/local/bin/codex".to_string()));
        let config = adapter.spawn_config("test", Path::new("/tmp"));
        assert_eq!(config.program, "/usr/local/bin/codex");
    }

    #[test]
    fn spawn_sets_work_dir() {
        let adapter = CodexCliAdapter::new(None);
        let config = adapter.spawn_config("task", Path::new("/my/worktree"));
        assert_eq!(config.work_dir, "/my/worktree");
    }

    #[test]
    fn prompt_patterns_detect_permission() {
        let adapter = CodexCliAdapter::new(None);
        let patterns = adapter.prompt_patterns();
        let d = patterns.detect("Would you like to run the following command?");
        assert!(d.is_some());
        assert!(matches!(
            d.unwrap().kind,
            crate::prompt::PromptKind::Permission { .. }
        ));
    }

    #[test]
    fn format_input_appends_newline() {
        let adapter = CodexCliAdapter::new(None);
        assert_eq!(adapter.format_input("y"), "y\n");
        assert_eq!(adapter.format_input("yes"), "yes\n");
    }

    #[test]
    fn name_is_codex_cli() {
        let adapter = CodexCliAdapter::new(None);
        assert_eq!(adapter.name(), "codex-cli");
    }

    #[test]
    fn codex_prefers_agents_md_instruction_order() {
        let adapter = CodexCliAdapter::new(None);
        assert_eq!(
            adapter.instruction_candidates(),
            &["AGENTS.md", "CLAUDE.md"]
        );
    }

    #[test]
    fn codex_wraps_launch_prompt() {
        let adapter = CodexCliAdapter::new(None);
        let wrapped = adapter.wrap_launch_prompt("Launch body");
        assert!(wrapped.contains("Codex under Batty supervision"));
        assert!(wrapped.contains("Launch body"));
    }

    // --- Backend trait method tests ---

    #[test]
    fn launch_command_active_includes_prompt() {
        let adapter = CodexCliAdapter::new(None);
        let cmd = adapter
            .launch_command("do the thing", false, false, None)
            .unwrap();
        assert!(cmd.contains("exec codex --dangerously-bypass-approvals-and-sandbox"));
        assert!(cmd.contains("'do the thing'"));
    }

    #[test]
    fn launch_command_idle_omits_prompt() {
        let adapter = CodexCliAdapter::new(None);
        let cmd = adapter
            .launch_command("ignored", true, false, None)
            .unwrap();
        assert_eq!(cmd, "exec codex --dangerously-bypass-approvals-and-sandbox");
    }

    #[test]
    fn launch_command_resume_uses_session_id() {
        let adapter = CodexCliAdapter::new(None);
        let cmd = adapter
            .launch_command("ignored", false, true, Some("codex-sess-1"))
            .unwrap();
        assert!(cmd.contains("exec codex resume 'codex-sess-1'"));
        assert!(cmd.contains("--dangerously-bypass-approvals-and-sandbox"));
    }

    #[test]
    fn launch_command_resume_without_session_id_errors() {
        let adapter = CodexCliAdapter::new(None);
        let result = adapter.launch_command("ignored", false, true, None);
        assert!(result.is_err());
    }

    #[test]
    fn new_session_id_returns_none() {
        let adapter = CodexCliAdapter::new(None);
        assert!(adapter.new_session_id().is_none());
    }

    #[test]
    fn supports_resume_is_true() {
        let adapter = CodexCliAdapter::new(None);
        assert!(adapter.supports_resume());
    }
}
