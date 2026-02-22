//! Codex CLI adapter.
//!
//! Runs Codex in interactive mode by default, passing the composed task prompt
//! as the initial user prompt argument.

use std::path::Path;

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

    fn format_input(&self, response: &str) -> String {
        format!("{response}\n")
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
}
