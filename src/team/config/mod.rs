//! Team configuration parsed from `.batty/team_config/team.yaml`.

mod types;

pub use types::*;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::TEAM_CONFIG_DIR;
use crate::agent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanningDirectiveFile {
    ReplenishmentContext,
    ReviewPolicy,
    EscalationPolicy,
}

impl PlanningDirectiveFile {
    pub fn file_name(self) -> &'static str {
        match self {
            Self::ReplenishmentContext => "replenishment_context.md",
            Self::ReviewPolicy => "review_policy.md",
            Self::EscalationPolicy => "escalation_policy.md",
        }
    }

    pub fn path_for(self, project_root: &Path) -> PathBuf {
        project_root
            .join(".batty")
            .join(TEAM_CONFIG_DIR)
            .join(self.file_name())
    }
}

/// A single validation check result.
#[derive(Debug, Clone)]
pub struct ValidationCheck {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

impl TeamConfig {
    pub fn resolve_claude_auth(&self, role: &RoleDef) -> ClaudeAuth {
        ClaudeAuth {
            mode: role.auth_mode.unwrap_or_default(),
            env: role.auth_env.clone(),
        }
    }

    pub fn role_def(&self, role_name: &str) -> Option<&RoleDef> {
        self.roles.iter().find(|role| role.name == role_name)
    }

    pub fn role_barrier_group(&self, role_name: &str) -> Option<&str> {
        self.role_def(role_name)
            .and_then(|role| role.barrier_group.as_deref())
    }

    pub fn orchestrator_enabled(&self) -> bool {
        self.workflow_mode.enables_runtime_surface() && self.orchestrator_pane
    }

    /// Resolve the effective agent for a role.
    ///
    /// Resolution order: role-level agent > team-level agent > "claude".
    pub fn resolve_agent(&self, role: &RoleDef) -> Option<String> {
        if role.role_type == RoleType::User {
            return None;
        }
        Some(
            role.agent
                .clone()
                .or_else(|| self.agent.clone())
                .unwrap_or_else(|| "claude".to_string()),
        )
    }

    /// Check if a role is allowed to send messages to another role.
    ///
    /// Uses `talks_to` if configured. If `talks_to` is empty for a role,
    /// falls back to the default hierarchy:
    /// - User <-> Architect
    /// - Architect <-> Manager
    /// - Manager <-> Engineer
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
        // external senders (e.g. email-router, slack-bridge) can send to anyone
        if self.external_senders.iter().any(|s| s == from_role) {
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

        // Default hierarchy: user<->architect, architect<->manager, manager<->engineer
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

        let valid_agents = agent::KNOWN_AGENT_NAMES.join(", ");

        // Validate team-level agent if specified.
        if let Some(team_agent) = self.agent.as_deref() {
            if agent::adapter_from_name(team_agent).is_none() {
                bail!(
                    "unknown team-level agent '{}'; valid agents: {}",
                    team_agent,
                    valid_agents
                );
            }
        }

        let mut role_names: HashSet<&str> = HashSet::new();
        for role in &self.roles {
            if role.name.is_empty() {
                bail!("role has empty name — every role requires a non-empty 'name' field");
            }

            if !role_names.insert(&role.name) {
                bail!("duplicate role name: '{}'", role.name);
            }

            // Non-user roles need an agent — either their own or the team default.
            if role.role_type != RoleType::User && role.agent.is_none() && self.agent.is_none() {
                bail!(
                    "role '{}' has no agent configured — \
                     set a role-level 'agent' field or a team-level 'agent' default; \
                     valid agents: {}",
                    role.name,
                    valid_agents
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

            if let Some(agent_name) = role.agent.as_deref()
                && agent::adapter_from_name(agent_name).is_none()
            {
                bail!(
                    "role '{}' uses unknown agent '{}'; valid agents: {}",
                    role.name,
                    agent_name,
                    valid_agents
                );
            }

            let effective_agent = role.agent.as_deref().or(self.agent.as_deref());
            if (role.auth_mode.is_some() || !role.auth_env.is_empty())
                && !matches!(effective_agent, Some("claude" | "claude-code"))
            {
                bail!(
                    "role '{}' configures Claude auth but effective agent is not claude",
                    role.name
                );
            }
            if role.auth_mode != Some(ClaudeAuthMode::Custom) && !role.auth_env.is_empty() {
                bail!(
                    "role '{}' sets auth_env but auth_mode is not 'custom'",
                    role.name
                );
            }
            for env_name in &role.auth_env {
                if !is_valid_env_name(env_name) {
                    bail!(
                        "role '{}' has invalid auth_env entry '{}'; expected shell env name",
                        role.name,
                        env_name
                    );
                }
            }
        }

        if self.workflow_policy.clean_room_mode {
            if self.workflow_policy.handoff_directory.trim().is_empty() {
                bail!("workflow_policy.handoff_directory cannot be empty in clean_room_mode");
            }

            for group_name in self.workflow_policy.barrier_groups.keys() {
                if group_name.trim().is_empty() {
                    bail!("workflow_policy.barrier_groups cannot contain an empty group name");
                }
            }

            for (group_name, roles) in &self.workflow_policy.barrier_groups {
                for role_name in roles {
                    if !role_names.contains(role_name.as_str()) {
                        bail!(
                            "workflow_policy.barrier_groups['{}'] references unknown role '{}'",
                            group_name,
                            role_name
                        );
                    }
                }
            }

            for role in &self.roles {
                if let Some(group) = role.barrier_group.as_deref()
                    && !self.workflow_policy.barrier_groups.is_empty()
                    && !self.workflow_policy.barrier_groups.contains_key(group)
                {
                    bail!(
                        "role '{}' references unknown barrier_group '{}'",
                        role.name,
                        group
                    );
                }
            }
        }

        // Validate talks_to references exist
        let all_role_names: Vec<&str> = role_names.iter().copied().collect();
        for role in &self.roles {
            for target in &role.talks_to {
                if !role_names.contains(target.as_str()) {
                    bail!(
                        "role '{}' references unknown role '{}' in talks_to; \
                         defined roles: {}",
                        role.name,
                        target,
                        all_role_names.join(", ")
                    );
                }
            }
        }

        if let Some(sender) = &self.automation_sender
            && !role_names.contains(sender.as_str())
            && sender != "human"
        {
            bail!(
                "automation_sender references unknown role '{}'; \
                 defined roles: {}",
                sender,
                all_role_names.join(", ")
            );
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

    /// Run all validation checks, collecting results for each check.
    /// Returns a list of (check_name, passed, detail) tuples.
    pub fn validate_verbose(&self) -> Vec<ValidationCheck> {
        let mut checks = Vec::new();

        // 1. Team name
        let name_ok = !self.name.is_empty();
        checks.push(ValidationCheck {
            name: "team_name".to_string(),
            passed: name_ok,
            detail: if name_ok {
                format!("team name: '{}'", self.name)
            } else {
                "team name is empty".to_string()
            },
        });

        // 2. Roles present
        let roles_ok = !self.roles.is_empty();
        checks.push(ValidationCheck {
            name: "roles_present".to_string(),
            passed: roles_ok,
            detail: if roles_ok {
                format!("{} role(s) defined", self.roles.len())
            } else {
                "no roles defined".to_string()
            },
        });

        if !roles_ok {
            return checks;
        }

        // 3. Team-level agent
        let team_agent_ok = match self.agent.as_deref() {
            Some(name) => agent::adapter_from_name(name).is_some(),
            None => true,
        };
        checks.push(ValidationCheck {
            name: "team_agent".to_string(),
            passed: team_agent_ok,
            detail: match self.agent.as_deref() {
                Some(name) if team_agent_ok => format!("team agent: '{name}'"),
                Some(name) => format!("unknown team agent: '{name}'"),
                None => "no team-level agent (roles must set their own)".to_string(),
            },
        });

        // 4. Per-role checks
        let mut role_names: HashSet<&str> = HashSet::new();
        for role in &self.roles {
            let unique = role_names.insert(&role.name);
            checks.push(ValidationCheck {
                name: format!("role_unique:{}", role.name),
                passed: unique,
                detail: if unique {
                    format!("role '{}' is unique", role.name)
                } else {
                    format!("duplicate role name: '{}'", role.name)
                },
            });

            let has_agent =
                role.role_type == RoleType::User || role.agent.is_some() || self.agent.is_some();
            checks.push(ValidationCheck {
                name: format!("role_agent:{}", role.name),
                passed: has_agent,
                detail: if has_agent {
                    let effective = role
                        .agent
                        .as_deref()
                        .or(self.agent.as_deref())
                        .unwrap_or("(user)");
                    format!("role '{}' agent: {effective}", role.name)
                } else {
                    format!("role '{}' has no agent", role.name)
                },
            });

            if let Some(agent_name) = role.agent.as_deref() {
                let valid = agent::adapter_from_name(agent_name).is_some();
                checks.push(ValidationCheck {
                    name: format!("role_agent_valid:{}", role.name),
                    passed: valid,
                    detail: if valid {
                        format!("role '{}' agent '{}' is valid", role.name, agent_name)
                    } else {
                        format!("role '{}' uses unknown agent '{}'", role.name, agent_name)
                    },
                });
            }

            let instances_ok = role.instances > 0;
            checks.push(ValidationCheck {
                name: format!("role_instances:{}", role.name),
                passed: instances_ok,
                detail: format!("role '{}' instances: {}", role.name, role.instances),
            });
        }

        // 5. talks_to references
        for role in &self.roles {
            for target in &role.talks_to {
                let valid = role_names.contains(target.as_str());
                checks.push(ValidationCheck {
                    name: format!("talks_to:{}→{}", role.name, target),
                    passed: valid,
                    detail: if valid {
                        format!("role '{}' → '{}' is valid", role.name, target)
                    } else {
                        format!(
                            "role '{}' references unknown role '{}' in talks_to",
                            role.name, target
                        )
                    },
                });
            }
        }

        // 6. automation_sender
        if let Some(sender) = &self.automation_sender {
            let valid = role_names.contains(sender.as_str()) || sender == "human";
            checks.push(ValidationCheck {
                name: "automation_sender".to_string(),
                passed: valid,
                detail: if valid {
                    format!("automation_sender '{sender}' is valid")
                } else {
                    format!("automation_sender references unknown role '{sender}'")
                },
            });
        }

        // 7. Layout zones
        if let Some(layout) = &self.layout {
            let total_pct: u32 = layout.zones.iter().map(|z| z.width_pct).sum();
            let valid = total_pct <= 100;
            checks.push(ValidationCheck {
                name: "layout_zones".to_string(),
                passed: valid,
                detail: if valid {
                    format!("layout zones sum to {total_pct}%")
                } else {
                    format!("layout zones sum to {total_pct}%, exceeds 100%")
                },
            });
        }

        // 8. Backend health checks (warnings, not failures)
        for (agent_name, health) in self.backend_health_results() {
            let healthy = health.is_healthy();
            checks.push(ValidationCheck {
                name: format!("backend_health:{agent_name}"),
                passed: healthy,
                detail: if healthy {
                    format!("backend '{agent_name}' is available")
                } else {
                    format!(
                        "backend '{agent_name}' binary not found on PATH (status: {})",
                        health.as_str()
                    )
                },
            });
        }

        checks
    }

    /// Collect unique configured backends and their health status.
    pub fn backend_health_results(&self) -> Vec<(String, agent::BackendHealth)> {
        let mut seen = HashSet::new();
        let mut results = Vec::new();
        for role in &self.roles {
            if let Some(agent_name) = self.resolve_agent(role) {
                if seen.insert(agent_name.clone()) {
                    let health = agent::health_check_by_name(&agent_name)
                        .unwrap_or(agent::BackendHealth::Unreachable);
                    results.push((agent_name, health));
                }
            }
        }
        results
    }

    /// Return warning messages for any unhealthy backends.
    pub fn check_backend_health(&self) -> Vec<String> {
        self.backend_health_results()
            .into_iter()
            .filter(|(_, health)| !health.is_healthy())
            .map(|(name, health)| {
                format!(
                    "backend '{name}' binary not found on PATH (status: {})",
                    health.as_str()
                )
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeAuth {
    pub mode: ClaudeAuthMode,
    pub env: Vec<String>,
}

fn is_valid_env_name(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) if first == '_' || first.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

pub fn load_planning_directive(
    project_root: &Path,
    directive: PlanningDirectiveFile,
    max_chars: usize,
) -> Result<Option<String>> {
    let path = directive.path_for(project_root);
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let trimmed = content.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }

            let total_chars = trimmed.chars().count();
            let truncated = trimmed.chars().take(max_chars).collect::<String>();
            if total_chars > max_chars {
                Ok(Some(format!(
                    "{truncated}\n\n[truncated to {max_chars} chars from {}]",
                    directive.file_name()
                )))
            } else {
                Ok(Some(truncated))
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error)
            .with_context(|| format!("failed to read planning directive {}", path.display())),
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_advanced;
#[cfg(test)]
mod tests_proptest;
