//! Team configuration parsed from `.batty/team_config/team.yaml`.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct TeamConfig {
    pub name: String,
    #[serde(default)]
    pub board: BoardConfig,
    #[serde(default)]
    pub standup: StandupConfig,
    #[serde(default)]
    pub layout: Option<LayoutConfig>,
    pub roles: Vec<RoleDef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BoardConfig {
    #[serde(default = "default_rotation_threshold")]
    pub rotation_threshold: u32,
}

impl Default for BoardConfig {
    fn default() -> Self {
        Self {
            rotation_threshold: default_rotation_threshold(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct StandupConfig {
    #[serde(default = "default_standup_interval")]
    pub interval_secs: u64,
    #[serde(default = "default_output_lines")]
    pub output_lines: u32,
}

impl Default for StandupConfig {
    fn default() -> Self {
        Self {
            interval_secs: default_standup_interval(),
            output_lines: default_output_lines(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct LayoutConfig {
    pub zones: Vec<ZoneDef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ZoneDef {
    pub name: String,
    pub width_pct: u32,
    #[serde(default)]
    pub split: Option<SplitDef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SplitDef {
    pub horizontal: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RoleDef {
    pub name: String,
    pub role_type: RoleType,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default = "default_instances")]
    pub instances: u32,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub talks_to: Vec<String>,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub channel_config: Option<ChannelConfig>,
    #[serde(default)]
    pub nudge_interval_secs: Option<u64>,
    #[serde(default)]
    pub receives_standup: Option<bool>,
    #[serde(default)]
    pub standup_interval_secs: Option<u64>,
    #[serde(default)]
    pub owns: Vec<String>,
    #[serde(default)]
    pub use_worktrees: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChannelConfig {
    pub target: String,
    pub provider: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoleType {
    User,
    Architect,
    Manager,
    Engineer,
}

fn default_rotation_threshold() -> u32 {
    20
}

fn default_standup_interval() -> u64 {
    1200
}

fn default_output_lines() -> u32 {
    30
}

fn default_instances() -> u32 {
    1
}

impl TeamConfig {
    /// Check if a role is allowed to send messages to another role.
    ///
    /// Uses `talks_to` if configured. If `talks_to` is empty for a role,
    /// falls back to the default hierarchy:
    /// - User ↔ Architect
    /// - Architect ↔ Manager
    /// - Manager ↔ Engineer
    ///
    /// The `from` and `to` are role definition names (not member instance names).
    /// "human" is always allowed to talk to any role.
    pub fn can_talk(&self, from_role: &str, to_role: &str) -> bool {
        // human (CLI user) can always send to anyone
        if from_role == "human" {
            return true;
        }
        // daemon-generated messages (standups, nudges) always allowed
        if from_role == "daemon" {
            return true;
        }

        let from_def = self.roles.iter().find(|r| r.name == from_role);
        let Some(from_def) = from_def else {
            return false;
        };

        // If talks_to is explicitly configured, use it
        if !from_def.talks_to.is_empty() {
            return from_def.talks_to.iter().any(|t| t == to_role);
        }

        // Default hierarchy: user↔architect, architect↔manager, manager↔engineer
        let to_def = self.roles.iter().find(|r| r.name == to_role);
        let Some(to_def) = to_def else {
            return false;
        };

        matches!(
            (from_def.role_type, to_def.role_type),
            (RoleType::User, RoleType::Architect)
                | (RoleType::Architect, RoleType::User)
                | (RoleType::Architect, RoleType::Manager)
                | (RoleType::Manager, RoleType::Architect)
                | (RoleType::Manager, RoleType::Engineer)
                | (RoleType::Engineer, RoleType::Manager)
        )
    }

    /// Load team config from a YAML file.
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let config: TeamConfig = serde_yaml::from_str(&content)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        Ok(config)
    }

    /// Validate the team config. Returns an error if invalid.
    pub fn validate(&self) -> Result<()> {
        if self.name.is_empty() {
            bail!("team name cannot be empty");
        }

        if self.roles.is_empty() {
            bail!("team must have at least one role");
        }

        let mut role_names: HashSet<&str> = HashSet::new();
        for role in &self.roles {
            if !role_names.insert(&role.name) {
                bail!("duplicate role name: '{}'", role.name);
            }

            if role.role_type != RoleType::User && role.agent.is_none() {
                bail!(
                    "role '{}' is not a user but has no agent configured",
                    role.name
                );
            }

            if role.role_type == RoleType::User && role.agent.is_some() {
                bail!(
                    "role '{}' is a user but has an agent configured; users use channels instead",
                    role.name
                );
            }

            if role.instances == 0 {
                bail!("role '{}' has zero instances", role.name);
            }
        }

        // Validate talks_to references exist
        for role in &self.roles {
            for target in &role.talks_to {
                if !role_names.contains(target.as_str()) {
                    bail!("role '{}' talks_to unknown role '{}'", role.name, target);
                }
            }
        }

        // Validate layout zones if present
        if let Some(layout) = &self.layout {
            let total_pct: u32 = layout.zones.iter().map(|z| z.width_pct).sum();
            if total_pct > 100 {
                bail!("layout zone widths sum to {}%, exceeds 100%", total_pct);
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_yaml() -> &'static str {
        r#"
name: test-team
roles:
  - name: architect
    role_type: architect
    agent: claude
    instances: 1
    talks_to: [manager]
  - name: manager
    role_type: manager
    agent: claude
    instances: 1
    talks_to: [architect, engineer]
  - name: engineer
    role_type: engineer
    agent: codex
    instances: 3
    talks_to: [manager]
"#
    }

    #[test]
    fn parse_minimal_config() {
        let config: TeamConfig = serde_yaml::from_str(minimal_yaml()).unwrap();
        assert_eq!(config.name, "test-team");
        assert_eq!(config.roles.len(), 3);
        assert_eq!(config.roles[0].role_type, RoleType::Architect);
        assert_eq!(config.roles[2].instances, 3);
    }

    #[test]
    fn parse_config_with_user_role() {
        let yaml = r#"
name: test-team
roles:
  - name: human
    role_type: user
    channel: telegram
    channel_config:
      target: "12345"
      provider: openclaw
    talks_to: [architect]
  - name: architect
    role_type: architect
    agent: claude
    instances: 1
    talks_to: [human]
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.roles[0].role_type, RoleType::User);
        assert_eq!(config.roles[0].channel.as_deref(), Some("telegram"));
        assert_eq!(
            config.roles[0].channel_config.as_ref().unwrap().provider,
            "openclaw"
        );
    }

    #[test]
    fn parse_full_config_with_layout() {
        let yaml = r#"
name: mafia-solver
board:
  rotation_threshold: 20
standup:
  interval_secs: 1200
  output_lines: 30
layout:
  zones:
    - name: architect
      width_pct: 15
    - name: managers
      width_pct: 25
      split: { horizontal: 3 }
    - name: engineers
      width_pct: 60
      split: { horizontal: 15 }
roles:
  - name: architect
    role_type: architect
    agent: claude
    instances: 1
    prompt: architect.md
    talks_to: [manager]
    nudge_interval_secs: 1800
    owns: ["planning/**", "docs/**"]
  - name: manager
    role_type: manager
    agent: claude
    instances: 3
    prompt: manager.md
    talks_to: [architect, engineer]
  - name: engineer
    role_type: engineer
    agent: codex
    instances: 5
    prompt: engineer.md
    talks_to: [manager]
    use_worktrees: true
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.name, "mafia-solver");
        assert_eq!(config.board.rotation_threshold, 20);
        assert_eq!(config.standup.interval_secs, 1200);
        let layout = config.layout.as_ref().unwrap();
        assert_eq!(layout.zones.len(), 3);
        assert_eq!(layout.zones[0].width_pct, 15);
        assert_eq!(layout.zones[2].split.as_ref().unwrap().horizontal, 15);
    }

    #[test]
    fn defaults_applied() {
        let yaml = r#"
name: minimal
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.board.rotation_threshold, 20);
        assert_eq!(config.standup.interval_secs, 1200);
        assert_eq!(config.standup.output_lines, 30);
        assert_eq!(config.roles[0].instances, 1);
    }

    #[test]
    fn validate_rejects_empty_name() {
        let yaml = r#"
name: ""
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("empty"));
    }

    #[test]
    fn validate_rejects_duplicate_role_names() {
        let yaml = r#"
name: test
roles:
  - name: worker
    role_type: engineer
    agent: codex
  - name: worker
    role_type: engineer
    agent: codex
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("duplicate"));
    }

    #[test]
    fn validate_rejects_non_user_without_agent() {
        let yaml = r#"
name: test
roles:
  - name: worker
    role_type: engineer
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("no agent"));
    }

    #[test]
    fn validate_rejects_user_with_agent() {
        let yaml = r#"
name: test
roles:
  - name: human
    role_type: user
    agent: claude
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("user") && err.contains("agent"));
    }

    #[test]
    fn validate_rejects_unknown_talks_to() {
        let yaml = r#"
name: test
roles:
  - name: worker
    role_type: engineer
    agent: codex
    talks_to: [nonexistent]
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("unknown role"));
    }

    #[test]
    fn validate_rejects_layout_over_100_pct() {
        let yaml = r#"
name: test
layout:
  zones:
    - name: a
      width_pct: 60
    - name: b
      width_pct: 50
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("100%"));
    }

    #[test]
    fn validate_accepts_minimal_config() {
        let config: TeamConfig = serde_yaml::from_str(minimal_yaml()).unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn load_from_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("team.yaml");
        std::fs::write(&path, minimal_yaml()).unwrap();
        let config = TeamConfig::load(&path).unwrap();
        assert_eq!(config.name, "test-team");
    }

    #[test]
    fn can_talk_default_hierarchy() {
        let config: TeamConfig = serde_yaml::from_str(
            r#"
name: test
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: manager
    role_type: manager
    agent: claude
  - name: engineer
    role_type: engineer
    agent: codex
"#,
        )
        .unwrap();

        // Default: architect↔manager, manager↔engineer
        assert!(config.can_talk("architect", "manager"));
        assert!(config.can_talk("manager", "architect"));
        assert!(config.can_talk("manager", "engineer"));
        assert!(config.can_talk("engineer", "manager"));

        // architect↔engineer blocked by default
        assert!(!config.can_talk("architect", "engineer"));
        assert!(!config.can_talk("engineer", "architect"));

        // human can talk to anyone
        assert!(config.can_talk("human", "architect"));
        assert!(config.can_talk("human", "engineer"));

        // daemon can talk to anyone
        assert!(config.can_talk("daemon", "engineer"));
    }

    #[test]
    fn can_talk_explicit_talks_to() {
        let config: TeamConfig = serde_yaml::from_str(
            r#"
name: test
roles:
  - name: architect
    role_type: architect
    agent: claude
    talks_to: [manager, engineer]
  - name: manager
    role_type: manager
    agent: claude
    talks_to: [architect, engineer]
  - name: engineer
    role_type: engineer
    agent: codex
    talks_to: [manager]
"#,
        )
        .unwrap();

        // Explicit: architect→engineer allowed
        assert!(config.can_talk("architect", "engineer"));
        // But engineer→architect still blocked (not in engineer's talks_to)
        assert!(!config.can_talk("engineer", "architect"));
    }

    #[test]
    fn validate_rejects_zero_instances() {
        let yaml = r#"
name: test
roles:
  - name: worker
    role_type: engineer
    agent: codex
    instances: 0
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("zero instances"));
    }
}
