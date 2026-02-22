use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const CONFIG_FILENAME: &str = "config.toml";
const CONFIG_DIR: &str = ".batty";

#[derive(Debug, Default, Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Policy {
    #[default]
    Observe,
    Suggest,
    Act,
}

#[derive(Debug, Deserialize)]
pub struct Defaults {
    #[serde(default = "default_agent")]
    pub agent: String,
    #[serde(default)]
    pub policy: Policy,
    #[serde(default)]
    pub dod: Option<String>,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
}

/// Policy section with auto-answer patterns.
///
/// ```toml
/// [policy.auto_answer]
/// "Continue? [y/n]" = "y"
/// "Allow tool" = "y"
/// ```
#[derive(Debug, Deserialize, Default)]
#[allow(dead_code)] // Used by policy engine (task #8), wired in task #12
pub struct PolicyConfig {
    #[serde(default)]
    pub auto_answer: HashMap<String, String>,
}

fn default_agent() -> String {
    "claude".to_string()
}

fn default_max_retries() -> u32 {
    3
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            agent: default_agent(),
            policy: Policy::default(),
            dod: None,
            max_retries: default_max_retries(),
        }
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct ProjectConfig {
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    #[allow(dead_code)] // Used by policy engine, wired in task #12
    pub policy: PolicyConfig,
}

impl ProjectConfig {
    /// Search upward from `start` for a `.batty/config.toml` file and load it.
    /// Returns the default config if no file is found.
    pub fn load(start: &Path) -> Result<(Self, Option<PathBuf>)> {
        if let Some(path) = Self::find_config_file(start) {
            let contents = std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let config: ProjectConfig = toml::from_str(&contents)
                .with_context(|| format!("failed to parse {}", path.display()))?;
            Ok((config, Some(path)))
        } else {
            Ok((ProjectConfig::default(), None))
        }
    }

    fn find_config_file(start: &Path) -> Option<PathBuf> {
        let mut dir = start.to_path_buf();
        loop {
            let candidate = dir.join(CONFIG_DIR).join(CONFIG_FILENAME);
            if candidate.is_file() {
                return Some(candidate);
            }
            if !dir.pop() {
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn default_config_values() {
        let config = ProjectConfig::default();
        assert_eq!(config.defaults.agent, "claude");
        assert_eq!(config.defaults.policy, Policy::Observe);
        assert_eq!(config.defaults.max_retries, 3);
        assert!(config.defaults.dod.is_none());
    }

    #[test]
    fn parse_full_config() {
        let toml = r#"
[defaults]
agent = "codex"
policy = "act"
dod = "cargo test --workspace"
max_retries = 5
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.defaults.agent, "codex");
        assert_eq!(config.defaults.policy, Policy::Act);
        assert_eq!(
            config.defaults.dod.as_deref(),
            Some("cargo test --workspace")
        );
        assert_eq!(config.defaults.max_retries, 5);
    }

    #[test]
    fn parse_partial_config() {
        let toml = r#"
[defaults]
agent = "aider"
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.defaults.agent, "aider");
        assert_eq!(config.defaults.policy, Policy::Observe);
        assert_eq!(config.defaults.max_retries, 3);
    }

    #[test]
    fn load_from_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let batty_dir = tmp.path().join(".batty");
        fs::create_dir_all(&batty_dir).unwrap();
        fs::write(
            batty_dir.join("config.toml"),
            r#"
[defaults]
agent = "claude"
policy = "act"
dod = "cargo test"
max_retries = 2
"#,
        )
        .unwrap();

        let (config, path) = ProjectConfig::load(tmp.path()).unwrap();
        assert!(path.is_some());
        assert_eq!(config.defaults.agent, "claude");
        assert_eq!(config.defaults.policy, Policy::Act);
        assert_eq!(config.defaults.max_retries, 2);
    }

    #[test]
    fn load_returns_default_when_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (config, path) = ProjectConfig::load(tmp.path()).unwrap();
        assert!(path.is_none());
        assert_eq!(config.defaults.agent, "claude");
    }

    #[test]
    fn parse_auto_answer_config() {
        let toml = r#"
[defaults]
agent = "claude"
policy = "act"

[policy.auto_answer]
"Continue? [y/n]" = "y"
"Allow tool" = "y"
"#;
        let config: ProjectConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.policy.auto_answer.len(), 2);
        assert_eq!(
            config.policy.auto_answer.get("Continue? [y/n]").unwrap(),
            "y"
        );
        assert_eq!(config.policy.auto_answer.get("Allow tool").unwrap(), "y");
    }

    #[test]
    fn load_walks_up_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let batty_dir = tmp.path().join(".batty");
        fs::create_dir_all(&batty_dir).unwrap();
        fs::write(
            batty_dir.join("config.toml"),
            r#"
[defaults]
agent = "codex"
"#,
        )
        .unwrap();

        let nested = tmp.path().join("src").join("deep").join("nested");
        fs::create_dir_all(&nested).unwrap();

        let (config, path) = ProjectConfig::load(&nested).unwrap();
        assert!(path.is_some());
        assert_eq!(config.defaults.agent, "codex");
    }
}
