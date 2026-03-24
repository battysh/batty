//! CLI-side scale command: mutates team.yaml to change instance counts
//! or add/remove manager roles. The daemon detects the config change via
//! hot-reload and reconciles the running topology.

use std::path::Path;

use anyhow::{Context, Result, bail};
use regex::Regex;

use super::config::{RoleType, TeamConfig};
use super::config_diff;
use super::hierarchy;
use super::team_config_path;
use crate::cli::ScaleCommand;

/// Run a `batty scale` subcommand.
pub fn run(project_root: &Path, command: ScaleCommand) -> Result<()> {
    let config_path = team_config_path(project_root);
    if !config_path.exists() {
        bail!("No team config found. Run `batty init` first.");
    }

    match command {
        ScaleCommand::Engineers { count } => scale_engineers(project_root, count),
        ScaleCommand::AddManager { name } => add_manager(project_root, &name),
        ScaleCommand::RemoveManager { name } => remove_manager(project_root, &name),
        ScaleCommand::Status => show_topology(project_root),
    }
}

/// Update the engineer role's instance count in team.yaml.
fn scale_engineers(project_root: &Path, count: u32) -> Result<()> {
    if count == 0 {
        bail!("Engineer count must be at least 1.");
    }

    let config_path = team_config_path(project_root);
    let config = TeamConfig::load(&config_path)?;

    // Find the engineer role
    let eng_role = config
        .roles
        .iter()
        .find(|r| r.role_type == RoleType::Engineer)
        .context("No engineer role found in team config")?;

    let old_count = eng_role.instances;
    if old_count == count {
        println!("Engineers already at {count} instances. No change needed.");
        return Ok(());
    }

    // Read raw YAML and update the engineer instances field
    let content = std::fs::read_to_string(&config_path).context("failed to read team.yaml")?;
    let updated = update_role_instances(&content, &eng_role.name, count)?;
    std::fs::write(&config_path, &updated).context("failed to write updated team.yaml")?;

    // Compute diff for display
    let new_config = TeamConfig::load(&config_path)?;
    let diff = config_diff::diff_configs(&config, &new_config)?;

    if count > old_count {
        println!(
            "Scaled engineers from {} to {} (+{} agents).",
            old_count,
            count,
            diff.added.len()
        );
    } else {
        println!(
            "Scaled engineers from {} to {} (-{} agents).",
            old_count,
            count,
            diff.removed.len()
        );
    }
    println!("Daemon will detect the change and reconcile topology.");
    Ok(())
}

/// Add a new manager role to team.yaml.
fn add_manager(project_root: &Path, name: &str) -> Result<()> {
    let config_path = team_config_path(project_root);
    let config = TeamConfig::load(&config_path)?;

    // Check name doesn't conflict with existing roles
    if config.roles.iter().any(|r| r.name == name) {
        bail!("Role '{name}' already exists in team config.");
    }

    // Find existing manager for defaults
    let existing_mgr = config
        .roles
        .iter()
        .find(|r| r.role_type == RoleType::Manager);
    let agent = existing_mgr
        .and_then(|m| m.agent.clone())
        .unwrap_or_else(|| "claude".to_string());
    let prompt = existing_mgr
        .and_then(|m| m.prompt.clone())
        .unwrap_or_else(|| "batty_manager.md".to_string());

    // Append new manager role to the YAML
    let content = std::fs::read_to_string(&config_path).context("failed to read team.yaml")?;

    let role_block = format!(
        "\n  - name: {name}\n    role_type: manager\n    agent: {agent}\n    instances: 1\n    prompt: {prompt}\n    talks_to: [architect, engineer]\n"
    );

    let updated = content.trim_end().to_string() + &role_block;
    std::fs::write(&config_path, &updated).context("failed to write updated team.yaml")?;

    println!("Added manager role '{name}'. Daemon will spawn the new agent.");
    Ok(())
}

/// Remove a manager role from team.yaml.
fn remove_manager(project_root: &Path, name: &str) -> Result<()> {
    let config_path = team_config_path(project_root);
    let config = TeamConfig::load(&config_path)?;

    let role = config
        .roles
        .iter()
        .find(|r| r.name == name)
        .context(format!("Role '{name}' not found in team config"))?;

    if role.role_type != RoleType::Manager {
        bail!(
            "Role '{name}' is not a manager (it's a {:?}).",
            role.role_type
        );
    }

    // Count managers — must keep at least one
    let manager_count = config
        .roles
        .iter()
        .filter(|r| r.role_type == RoleType::Manager)
        .count();
    if manager_count <= 1 {
        bail!("Cannot remove the last manager. At least one manager is required.");
    }

    // Remove the role block from YAML
    let content = std::fs::read_to_string(&config_path).context("failed to read team.yaml")?;
    let updated = remove_role_block(&content, name)?;
    std::fs::write(&config_path, &updated).context("failed to write updated team.yaml")?;

    println!("Removed manager role '{name}'. Daemon will gracefully shut down the agent.");
    Ok(())
}

/// Show current topology.
fn show_topology(project_root: &Path) -> Result<()> {
    let config_path = team_config_path(project_root);
    let config = TeamConfig::load(&config_path)?;
    let members = hierarchy::resolve_hierarchy(&config)?;

    println!("Team: {}", config.name);
    println!();

    for role in &config.roles {
        let type_str = match role.role_type {
            RoleType::User => "user",
            RoleType::Architect => "architect",
            RoleType::Manager => "manager",
            RoleType::Engineer => "engineer",
        };
        let member_names: Vec<&str> = members
            .iter()
            .filter(|m| m.role_name == role.name)
            .map(|m| m.name.as_str())
            .collect();
        println!(
            "  {:12} {:10} instances={:<3} members=[{}]",
            role.name,
            type_str,
            role.instances,
            member_names.join(", ")
        );
    }

    let agent_count = members
        .iter()
        .filter(|m| m.role_type != RoleType::User)
        .count();
    println!();
    println!("Total agent members: {agent_count}");
    Ok(())
}

// ---------------------------------------------------------------------------
// YAML manipulation helpers
// ---------------------------------------------------------------------------

/// Update the `instances:` field for a role identified by name in raw YAML.
///
/// This preserves comments and formatting by doing a targeted regex replacement.
fn update_role_instances(yaml: &str, role_name: &str, new_count: u32) -> Result<String> {
    // Strategy: find the role block starting with `- name: <role_name>`, then
    // update the `instances:` line within it (before the next `- name:` or EOF).
    let lines: Vec<&str> = yaml.lines().collect();
    let mut result = Vec::new();
    let mut in_target_role = false;
    let mut updated = false;
    let instances_re = Regex::new(r"^(\s+instances:\s*)\d+").unwrap();

    for line in &lines {
        // Detect role block boundaries
        let trimmed = line.trim();
        if trimmed.starts_with("- name:") {
            let name_val = trimmed.strip_prefix("- name:").unwrap_or("").trim();
            in_target_role = name_val == role_name;
        }

        if in_target_role && !updated {
            if let Some(caps) = instances_re.captures(line) {
                let prefix = caps.get(1).unwrap().as_str();
                result.push(format!("{prefix}{new_count}"));
                updated = true;
                continue;
            }
        }

        result.push(line.to_string());
    }

    if !updated {
        bail!("Could not find instances field for role '{role_name}' in team.yaml");
    }

    Ok(result.join("\n") + "\n")
}

/// Remove a role block (from `- name: <name>` to the next `- name:` or EOF).
fn remove_role_block(yaml: &str, role_name: &str) -> Result<String> {
    let lines: Vec<&str> = yaml.lines().collect();
    let mut result = Vec::new();
    let mut skipping = false;
    let mut found = false;

    for line in &lines {
        let trimmed = line.trim();
        if trimmed.starts_with("- name:") {
            let name_val = trimmed.strip_prefix("- name:").unwrap_or("").trim();
            if name_val == role_name {
                skipping = true;
                found = true;
                continue;
            }
            skipping = false;
        }

        if !skipping {
            result.push(*line);
        }
    }

    if !found {
        bail!("Could not find role '{role_name}' in team.yaml");
    }

    Ok(result.join("\n") + "\n")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_YAML: &str = r#"name: test-team

roles:
  - name: architect
    role_type: architect
    agent: claude
    instances: 1

  - name: manager
    role_type: manager
    agent: claude
    instances: 1
    prompt: batty_manager.md
    talks_to: [architect, engineer]

  - name: secondary-mgr
    role_type: manager
    agent: claude
    instances: 1
    prompt: batty_manager.md

  - name: engineer
    role_type: engineer
    agent: claude
    instances: 3
    prompt: batty_engineer.md
    talks_to: [manager]
    use_worktrees: true
"#;

    #[test]
    fn update_instances_for_engineer() {
        let result = update_role_instances(SAMPLE_YAML, "engineer", 6).unwrap();
        assert!(result.contains("instances: 6"));
        // Architect still has 1
        let config: serde_yaml::Value = serde_yaml::from_str(&result).unwrap();
        let roles = config["roles"].as_sequence().unwrap();
        let eng = roles
            .iter()
            .find(|r| r["name"].as_str() == Some("engineer"))
            .unwrap();
        assert_eq!(eng["instances"].as_u64(), Some(6));
        // Manager still 1
        let mgr = roles
            .iter()
            .find(|r| r["name"].as_str() == Some("manager"))
            .unwrap();
        assert_eq!(mgr["instances"].as_u64(), Some(1));
    }

    #[test]
    fn update_instances_for_manager() {
        let result = update_role_instances(SAMPLE_YAML, "manager", 3).unwrap();
        let config: serde_yaml::Value = serde_yaml::from_str(&result).unwrap();
        let roles = config["roles"].as_sequence().unwrap();
        let mgr = roles
            .iter()
            .find(|r| r["name"].as_str() == Some("manager"))
            .unwrap();
        assert_eq!(mgr["instances"].as_u64(), Some(3));
    }

    #[test]
    fn update_instances_missing_role_errors() {
        let result = update_role_instances(SAMPLE_YAML, "nonexistent", 5);
        assert!(result.is_err());
    }

    #[test]
    fn remove_role_block_removes_manager() {
        let result = remove_role_block(SAMPLE_YAML, "secondary-mgr").unwrap();
        assert!(!result.contains("secondary-mgr"));
        // Other roles still present
        assert!(result.contains("architect"));
        assert!(result.contains("manager"));
        assert!(result.contains("engineer"));
        // Parses as valid YAML
        let config: serde_yaml::Value = serde_yaml::from_str(&result).unwrap();
        let roles = config["roles"].as_sequence().unwrap();
        assert_eq!(roles.len(), 3);
    }

    #[test]
    fn remove_role_block_missing_errors() {
        let result = remove_role_block(SAMPLE_YAML, "nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn remove_last_role_block_works() {
        // Removing the last role (engineer) should work
        let result = remove_role_block(SAMPLE_YAML, "engineer").unwrap();
        assert!(!result.contains("- name: engineer"));
        // The word "engineer" may still appear in talks_to of other roles
        let config: serde_yaml::Value = serde_yaml::from_str(&result).unwrap();
        let roles = config["roles"].as_sequence().unwrap();
        assert_eq!(roles.len(), 3);
    }
}
