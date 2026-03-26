//! Kiro CLI adapter.
//!
//! Batty creates a per-member agent config in `.kiro/agents/` and launches
//! kiro with `--agent batty-<member>`. The agent config carries the system
//! prompt, model selection, and tool permissions so kiro loads them natively.
#![cfg_attr(not(test), allow(dead_code))]

use std::path::Path;

use uuid::Uuid;

use crate::agent::{AgentAdapter, SpawnConfig};
use crate::prompt::PromptPatterns;

/// Default model for Kiro agents.
pub const KIRO_DEFAULT_MODEL: &str = "claude-opus-4.6-1m";

/// Adapter for the Kiro CLI.
pub struct KiroCliAdapter {
    /// Override the kiro binary name/path (default: "kiro-cli").
    program: String,
}

impl KiroCliAdapter {
    pub fn new(program: Option<String>) -> Self {
        Self {
            program: program.unwrap_or_else(|| "kiro-cli".to_string()),
        }
    }
}

/// Write a `.kiro/agents/batty-<member>.json` agent config so kiro loads the
/// role prompt as a proper system prompt rather than user input.
pub fn write_kiro_agent_config(
    member_name: &str,
    prompt: &str,
    work_dir: &Path,
) -> anyhow::Result<String> {
    let agent_name = format!("batty-{member_name}");
    let agents_dir = work_dir.join(".kiro").join("agents");
    std::fs::create_dir_all(&agents_dir)?;
    let config_path = agents_dir.join(format!("{agent_name}.json"));
    let config = serde_json::json!({
        "name": agent_name,
        "description": format!("Batty-managed agent for {member_name}"),
        "prompt": prompt,
        "tools": ["*"],
        "allowedTools": ["*"],
        "model": KIRO_DEFAULT_MODEL,
        "includeMcpJson": true
    });
    std::fs::write(&config_path, serde_json::to_string_pretty(&config)?)?;
    Ok(agent_name)
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
                "--trust-all-tools".to_string(),
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

    fn launch_command(
        &self,
        prompt: &str,
        _idle: bool,
        _resume: bool,
        _session_id: Option<&str>,
    ) -> anyhow::Result<String> {
        let escaped_program = self.program.replace('\'', "'\\''");
        // `prompt` is reused as the agent name when called from the launcher
        // after write_kiro_agent_config has been invoked. If it looks like an
        // agent name (no whitespace), use --agent; otherwise fall back to
        // passing the prompt as input.
        if !prompt.contains(' ') && prompt.starts_with("batty-") {
            Ok(format!(
                "exec '{escaped_program}' chat --trust-all-tools --agent {prompt}"
            ))
        } else {
            let escaped = prompt.replace('\'', "'\\''");
            Ok(format!(
                "exec '{escaped_program}' chat --trust-all-tools --model {KIRO_DEFAULT_MODEL} '{escaped}'"
            ))
        }
    }

    fn new_session_id(&self) -> Option<String> {
        Some(Uuid::new_v4().to_string())
    }

    fn health_check(&self) -> super::BackendHealth {
        super::check_binary_available(&self.program)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_program_is_kiro() {
        let adapter = KiroCliAdapter::new(None);
        let config = adapter.spawn_config("test", Path::new("/tmp"));
        assert_eq!(config.program, "kiro-cli");
    }

    #[test]
    fn spawn_uses_chat_with_trust_all_tools() {
        let adapter = KiroCliAdapter::new(None);
        let config = adapter.spawn_config("ship the patch", Path::new("/tmp/worktree"));
        assert_eq!(
            config.args,
            vec![
                "chat".to_string(),
                "--trust-all-tools".to_string(),
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

    #[test]
    fn launch_command_with_agent_name() {
        let adapter = KiroCliAdapter::new(None);
        let cmd = adapter
            .launch_command("batty-architect", false, false, None)
            .unwrap();
        assert_eq!(
            cmd,
            "exec 'kiro-cli' chat --trust-all-tools --agent batty-architect"
        );
    }

    #[test]
    fn launch_command_with_raw_prompt_fallback() {
        let adapter = KiroCliAdapter::new(None);
        let cmd = adapter
            .launch_command("do the thing", false, false, None)
            .unwrap();
        assert!(cmd.contains("--model"));
        assert!(cmd.contains("'do the thing'"));
    }

    #[test]
    fn launch_command_escapes_single_quotes() {
        let adapter = KiroCliAdapter::new(None);
        let cmd = adapter
            .launch_command("fix user's bug", false, false, None)
            .unwrap();
        assert!(cmd.contains("user'\\''s"));
    }

    #[test]
    fn launch_command_uses_configured_program() {
        let adapter = KiroCliAdapter::new(Some("/opt/kiro-cli".to_string()));
        let cmd = adapter
            .launch_command("batty-architect", false, false, None)
            .unwrap();
        assert_eq!(
            cmd,
            "exec '/opt/kiro-cli' chat --trust-all-tools --agent batty-architect"
        );
    }

    #[test]
    fn write_agent_config_creates_json() {
        let tmp = tempfile::tempdir().unwrap();
        let name =
            write_kiro_agent_config("architect", "You are an architect", tmp.path()).unwrap();
        assert_eq!(name, "batty-architect");
        let config_path = tmp.path().join(".kiro/agents/batty-architect.json");
        assert!(config_path.exists());
        let content: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(content["prompt"], "You are an architect");
        assert_eq!(content["model"], KIRO_DEFAULT_MODEL);
        assert_eq!(content["tools"], serde_json::json!(["*"]));
        assert_eq!(content["allowedTools"], serde_json::json!(["*"]));
    }

    #[test]
    fn new_session_id_returns_uuid() {
        let adapter = KiroCliAdapter::new(None);
        let sid = adapter.new_session_id();
        assert!(sid.is_some());
        assert!(!sid.unwrap().is_empty());
    }

    #[test]
    fn supports_resume_is_false() {
        let adapter = KiroCliAdapter::new(None);
        assert!(!adapter.supports_resume());
    }
}
