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
        let prefix = format!(
            "{} --dangerously-bypass-approvals-and-sandbox",
            self.program
        );
        if resume {
            let sid = session_id.context("missing Codex session ID for resume")?;
            let fallback = if idle {
                format!("exec {prefix}")
            } else {
                format!("exec {prefix} '{escaped}'")
            };
            Ok(format!(
                "{program} resume '{sid}' --dangerously-bypass-approvals-and-sandbox || {fallback}",
                program = self.program,
            ))
        } else if idle {
            Ok(format!("exec {prefix}"))
        } else {
            Ok(format!("{prefix} '{escaped}'"))
        }
    }

    fn supports_resume(&self) -> bool {
        true
    }

    fn health_check(&self) -> super::BackendHealth {
        super::check_binary_available(&self.program)
    }
}

impl CodexCliAdapter {
    /// Build the launch command for SDK (JSONL) mode.
    ///
    /// In Codex SDK mode, each message spawns a new `codex exec --json`
    /// subprocess. The initial prompt is the system/role context; actual
    /// task messages are sent per-turn by the runtime.
    ///
    /// `system_prompt`: role context passed as the initial exec prompt.
    pub fn sdk_launch_command(&self, _system_prompt: Option<&str>) -> String {
        // In Codex SDK mode, the shim runtime handles spawning per-message.
        // The launch script just needs to set up the environment (PATH, CWD).
        // We use a simple sleep loop as a placeholder process — the actual
        // codex exec calls are made by the runtime_codex module.
        //
        // Codex SDK uses spawn-per-message — the runtime handles subprocess
        // spawning. The launch script just needs a sentinel process that stays
        // alive so the shim doesn't exit.
        // Use `sleep 2147483647` (max 32-bit seconds ≈ 68 years) instead of
        // `sleep infinity` — macOS sleep(1) doesn't support "infinity".
        "exec sleep 2147483647".to_string()
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
    fn sdk_launch_command_uses_portable_sleep() {
        // Regression: macOS sleep(1) doesn't support "infinity".
        // The sentinel must use a numeric argument.
        let adapter = CodexCliAdapter::new(None);
        let cmd = adapter.sdk_launch_command(None);
        assert!(
            !cmd.contains("infinity"),
            "sleep infinity is not portable to macOS — use a numeric value"
        );
        assert!(cmd.contains("sleep"), "sentinel must use sleep");
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
        assert!(cmd.contains("codex --dangerously-bypass-approvals-and-sandbox"));
        assert!(cmd.contains("'do the thing'"));
        // Active (non-resume) should NOT use exec so the shim can detect exit
        assert!(!cmd.starts_with("exec "));
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
        assert!(cmd.contains("codex resume 'codex-sess-1'"));
        assert!(cmd.contains("--dangerously-bypass-approvals-and-sandbox"));
        assert!(cmd.contains("|| exec codex --dangerously-bypass-approvals-and-sandbox 'ignored'"));
    }

    #[test]
    fn launch_command_resume_idle_falls_back_to_fresh_idle_start() {
        let adapter = CodexCliAdapter::new(None);
        let cmd = adapter
            .launch_command("ignored", true, true, Some("codex-sess-1"))
            .unwrap();
        assert!(cmd.contains("codex resume 'codex-sess-1'"));
        assert!(cmd.contains("|| exec codex --dangerously-bypass-approvals-and-sandbox"));
        assert!(!cmd.contains("'ignored'"));
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
