//! tmux layout builder — creates zones and panes from team hierarchy.
//!
//! Zones are vertical columns in the tmux window. Within each zone, members
//! are stacked vertically; engineer-heavy zones may first be partitioned into
//! manager-aligned subcolumns to preserve the reporting hierarchy.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::path::Path;

use anyhow::{Result, bail};
use tracing::{debug, info};

use super::config::{LayoutConfig, OrchestratorPosition, RoleType, WorkflowMode};
use super::hierarchy::MemberInstance;
use crate::tmux;

const ORCHESTRATOR_PANE_WIDTH_PCT: u32 = 20;
const ORCHESTRATOR_ROLE: &str = "orchestrator";

#[derive(Debug, Clone)]
struct ZonePlan<'a> {
    width_pct: u32,
    members: Vec<&'a MemberInstance>,
    horizontal_columns: usize,
}

/// Build the tmux layout for a team session.
///
/// Creates the session with the first member's pane, then splits to create
/// all remaining panes. Returns a mapping of member name → tmux pane target.
pub fn build_layout(
    session: &str,
    members: &[MemberInstance],
    layout: &Option<LayoutConfig>,
    project_root: &Path,
    workflow_mode: WorkflowMode,
    orchestrator_pane: bool,
    orchestrator_position: OrchestratorPosition,
) -> Result<HashMap<String, String>> {
    let pane_members: Vec<_> = members
        .iter()
        .filter(|m| m.role_type != RoleType::User)
        .collect();

    if pane_members.is_empty() {
        bail!("no pane members to create layout for");
    }

    let work_dir = project_root.to_string_lossy().to_string();

    // Create session with the first member
    tmux::create_session(session, "bash", &[], &work_dir)?;
    tmux::rename_window(&format!("{session}:0"), "team")?;

    // Enable pane borders with role labels using @batty_role (agent-proof)
    let _ = std::process::Command::new("tmux")
        .args(["set-option", "-t", session, "pane-border-status", "top"])
        .output();
    let _ = std::process::Command::new("tmux")
        .args([
            "set-option",
            "-t",
            session,
            "pane-border-format",
            " #[fg=green,bold]#{@batty_role}#[default] #{@batty_status} ",
        ])
        .output();

    let mut pane_map: HashMap<String, String> = HashMap::new();
    let orchestrator_enabled = should_launch_orchestrator_pane(workflow_mode, orchestrator_pane);
    let initial_pane = tmux::pane_id(session)?;
    let agent_root_pane = if orchestrator_enabled {
        launch_orchestrator_pane(session, &initial_pane, project_root, orchestrator_position)?
    } else {
        initial_pane
    };

    if pane_members.len() == 1 {
        // Single pane — just use the initial pane
        set_pane_title(session, &agent_root_pane, &pane_members[0].name)?;
        pane_map.insert(pane_members[0].name.clone(), agent_root_pane);
        return Ok(pane_map);
    }

    // Group members by zone for layout
    let zones = if let Some(layout_config) = layout {
        build_zones_from_config(layout_config, &pane_members)
    } else {
        build_zones_auto(&pane_members)
    };

    // Create remaining zone columns by splitting the previous zone's pane.
    // Left-to-right: each split carves the next zone off the right side.
    //
    // tmux `split -h -p N` gives the NEW pane N% of the source pane.
    // Before each split, the source pane represents zones [i-1..N]. We want
    // zone i-1 to keep its share and the new pane to get the rest.
    //
    // Example: zones [20%, 20%, 60%]:
    //   Split 1: source = zones [0,1,2] = 100%. New pane gets (20+60)/100 = 80%.
    //   Split 2: source = zones [1,2] = 80%.  New pane gets 60/80 = 75%.
    let mut zone_panes: Vec<String> = vec![agent_root_pane.clone()];
    let mut remaining_pct: u32 = zones.iter().map(|zone| zone.width_pct).sum();
    for (i, _zone) in zones.iter().enumerate().skip(1) {
        let right_side: u32 = zones[i..].iter().map(|zone| zone.width_pct).sum();
        let split_pct = ((right_side as f64 / remaining_pct as f64) * 100.0).round() as u32;
        let split_pct = split_pct.clamp(10, 90);
        let split_from = zone_panes.last().unwrap();
        let pane_id = tmux::split_window_horizontal(split_from, split_pct)?;
        zone_panes.push(pane_id);
        remaining_pct = right_side;
        debug!(zone = i, split_pct, "created zone column");
    }

    // Within each zone, split vertically for members. Engineer zones with
    // multiple managers are partitioned into per-manager subcolumns first.
    for (zone_idx, zone) in zones.iter().enumerate() {
        let zone_pane = &zone_panes[zone_idx];
        let zone_members = &zone.members;

        if zone_members.is_empty() {
            continue;
        }

        let subgroups = split_zone_subgroups(zone_members);
        if subgroups.len() == 1 {
            let columns = split_members_into_columns(zone_members, zone.horizontal_columns);
            if columns.len() == 1 {
                stack_members_in_pane(session, zone_pane, &columns[0], &mut pane_map)?;
            } else {
                let column_panes = split_subgroup_columns(zone_pane, &columns)?;
                for (column_pane, column_members) in column_panes.iter().zip(columns.iter()) {
                    stack_members_in_pane(session, column_pane, column_members, &mut pane_map)?;
                }
            }
            continue;
        }

        let subgroup_panes = split_subgroup_columns(zone_pane, &subgroups)?;
        for (subgroup_pane, subgroup_members) in subgroup_panes.iter().zip(subgroups.iter()) {
            stack_members_in_pane(session, subgroup_pane, subgroup_members, &mut pane_map)?;
        }
    }

    info!(session, panes = pane_map.len(), "team layout created");

    Ok(pane_map)
}

fn should_launch_orchestrator_pane(workflow_mode: WorkflowMode, orchestrator_pane: bool) -> bool {
    workflow_mode.enables_runtime_surface() && orchestrator_pane
}

fn launch_orchestrator_pane(
    session: &str,
    initial_pane: &str,
    project_root: &Path,
    position: OrchestratorPosition,
) -> Result<String> {
    let log_path = super::orchestrator_log_path(project_root);
    ensure_orchestrator_log(&log_path)?;

    let (orchestrator_target, agent_root_pane) = match position {
        OrchestratorPosition::Left => {
            // Split horizontally: new pane (right) gets the remaining width for agents.
            // The original pane (left) becomes the orchestrator column.
            let agent_pane = tmux::split_window_horizontal(
                initial_pane,
                100 - ORCHESTRATOR_PANE_WIDTH_PCT,
            )?;
            (initial_pane.to_string(), agent_pane)
        }
        OrchestratorPosition::Bottom => {
            // Split vertically: new pane (bottom) becomes the orchestrator.
            // The original pane (top) remains the agent root.
            let orch_pane = tmux::split_window_vertical_in_pane(
                session,
                initial_pane,
                ORCHESTRATOR_PANE_WIDTH_PCT,
            )?;
            (orch_pane, initial_pane.to_string())
        }
    };

    let tail_command = format!(
        "bash -lc 'touch {path}; exec tail -n 200 -F {path}'",
        path = shell_single_quote(log_path.to_string_lossy().as_ref())
    );
    tmux::respawn_pane(&orchestrator_target, &tail_command)?;
    set_pane_title(session, &orchestrator_target, ORCHESTRATOR_ROLE)?;
    let _ = std::process::Command::new("tmux")
        .args([
            "set-option",
            "-p",
            "-t",
            orchestrator_target.as_str(),
            "@batty_status",
            "workflow stream",
        ])
        .output();
    let _ = std::process::Command::new("tmux")
        .args(["select-pane", "-t", agent_root_pane.as_str()])
        .output();
    Ok(agent_root_pane)
}

fn ensure_orchestrator_log(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    OpenOptions::new().create(true).append(true).open(path)?;
    Ok(())
}

fn shell_single_quote(value: &str) -> String {
    value.replace('\'', "'\"'\"'")
}

fn split_off_current_member_pct(total_slots: usize) -> u32 {
    (((1.0 / total_slots as f64) * 100.0).round() as u32).clamp(10, 90)
}

fn split_zone_subgroups<'a>(zone_members: &'a [&MemberInstance]) -> Vec<Vec<&'a MemberInstance>> {
    let engineer_hierarchy = zone_members
        .iter()
        .all(|member| member.role_type == RoleType::Engineer && member.reports_to.is_some());
    if !engineer_hierarchy {
        return vec![zone_members.to_vec()];
    }

    let mut groups: Vec<(String, Vec<&MemberInstance>)> = Vec::new();
    for member in zone_members {
        let parent = member.reports_to.clone().unwrap_or_default();
        if let Some((_, grouped)) = groups
            .iter_mut()
            .find(|(reports_to, _)| *reports_to == parent)
        {
            grouped.push(*member);
        } else {
            groups.push((parent, vec![*member]));
        }
    }

    groups.into_iter().map(|(_, grouped)| grouped).collect()
}

fn split_members_into_columns<'a>(
    members: &[&'a MemberInstance],
    desired_columns: usize,
) -> Vec<Vec<&'a MemberInstance>> {
    let columns = desired_columns.clamp(1, members.len().max(1));
    if columns == 1 {
        return vec![members.to_vec()];
    }

    let mut groups = Vec::with_capacity(columns);
    let mut start = 0;
    for column_idx in 0..columns {
        let remaining_members = members.len() - start;
        let remaining_columns = columns - column_idx;
        let take = remaining_members.div_ceil(remaining_columns);
        groups.push(members[start..start + take].to_vec());
        start += take;
    }

    groups
}

fn split_subgroup_columns(
    zone_pane: &str,
    subgroups: &[Vec<&MemberInstance>],
) -> Result<Vec<String>> {
    let mut panes = vec![zone_pane.to_string()];
    let mut remaining_weight: usize = subgroups.iter().map(Vec::len).sum();

    for subgroup_idx in 1..subgroups.len() {
        let right_weight: usize = subgroups[subgroup_idx..].iter().map(Vec::len).sum();
        let split_pct = ((right_weight as f64 / remaining_weight as f64) * 100.0).round() as u32;
        let split_pct = split_pct.clamp(10, 90);
        let split_from = panes.last().unwrap();
        let pane_id = tmux::split_window_horizontal(split_from, split_pct)?;
        panes.push(pane_id);
        remaining_weight = right_weight;
    }

    Ok(panes)
}

fn stack_members_in_pane(
    session: &str,
    pane_id: &str,
    members: &[&MemberInstance],
    pane_map: &mut HashMap<String, String>,
) -> Result<()> {
    let remaining_pane = pane_id.to_string();

    for member_idx in (1..members.len()).rev() {
        let member = members[member_idx];
        let pct = split_off_current_member_pct(member_idx + 1);
        let member_pane = tmux::split_window_vertical_in_pane(session, &remaining_pane, pct)?;
        set_pane_title(session, &member_pane, &member.name)?;
        debug!(
            member = %member.name,
            pane = %member_pane,
            split_pct = pct,
            "created member pane"
        );
        pane_map.insert(member.name.clone(), member_pane);
    }

    tmux::select_layout_even(&remaining_pane)?;
    set_pane_title(session, &remaining_pane, &members[0].name)?;
    pane_map.insert(members[0].name.clone(), remaining_pane);
    Ok(())
}

/// Set a pane's title and store the role name in a custom tmux option.
///
/// We set both `select-pane -T` (standard title) and a custom pane option
/// `@batty_role` that agents like Claude Code cannot overwrite. The
/// `pane-border-format` reads `@batty_role` for a stable label.
fn set_pane_title(_session: &str, pane_id: &str, title: &str) -> Result<()> {
    // Use select-pane -T to set pane title. Pane IDs (%N) are global in tmux.
    let output = std::process::Command::new("tmux")
        .args(["select-pane", "-t", pane_id, "-T", title])
        .output()?;
    if !output.status.success() {
        debug!(
            pane = pane_id,
            title, "failed to set pane title (non-critical)"
        );
    }

    // Store role name in a custom pane option that agents can't overwrite
    let _ = std::process::Command::new("tmux")
        .args(["set-option", "-p", "-t", pane_id, "@batty_role", title])
        .output();

    Ok(())
}

/// Group members into zones based on explicit layout config.
fn build_zones_from_config<'a>(
    config: &LayoutConfig,
    members: &'a [&MemberInstance],
) -> Vec<ZonePlan<'a>> {
    let mut zones: Vec<ZonePlan<'a>> = config
        .zones
        .iter()
        .map(|z| ZonePlan {
            width_pct: z.width_pct,
            members: Vec::new(),
            horizontal_columns: z
                .split
                .as_ref()
                .map(|split| split.horizontal as usize)
                .unwrap_or(1)
                .max(1),
        })
        .collect();

    // Map members to zones by role type
    let mut member_queue: Vec<&MemberInstance> = members.to_vec();

    for (zone_idx, zone_def) in config.zones.iter().enumerate() {
        let zone_name = zone_def.name.as_str();

        let exact_matches: Vec<&MemberInstance> = member_queue
            .iter()
            .copied()
            .filter(|member| member.name == zone_name || member.role_name == zone_name)
            .collect();
        if !exact_matches.is_empty() {
            let mut selected_names: Vec<&str> = Vec::new();
            for member in exact_matches {
                if !selected_names.contains(&member.name.as_str()) {
                    zones[zone_idx].members.push(member);
                    selected_names.push(member.name.as_str());
                }
                if member.role_type == RoleType::Manager {
                    for report in member_queue.iter().copied().filter(|candidate| {
                        candidate.reports_to.as_deref() == Some(member.name.as_str())
                    }) {
                        if !selected_names.contains(&report.name.as_str()) {
                            zones[zone_idx].members.push(report);
                            selected_names.push(report.name.as_str());
                        }
                    }
                }
            }
            member_queue.retain(|member| !selected_names.contains(&member.name.as_str()));
            continue;
        }

        // Try to match zone name to role types
        let target_types = match zone_name {
            n if n.contains("architect") => vec![RoleType::Architect],
            n if n.contains("manager") => vec![RoleType::Manager],
            n if n.contains("engineer") => vec![RoleType::Engineer],
            _ => continue,
        };

        let max_members = zone_def
            .split
            .as_ref()
            .map(|s| s.horizontal as usize)
            .unwrap_or(usize::MAX);

        let mut taken = 0;
        member_queue.retain(|m| {
            if taken >= max_members {
                return true;
            }
            if target_types.contains(&m.role_type) {
                zones[zone_idx].members.push(m);
                taken += 1;
                false
            } else {
                true
            }
        });
    }

    // Put any unplaced members in the last zone
    if let Some(last) = zones.last_mut() {
        last.members.extend(member_queue);
    }

    // Remove empty zones
    zones.retain(|zone| !zone.members.is_empty());
    zones
}

/// Auto-generate zones from member role types.
fn build_zones_auto<'a>(members: &'a [&MemberInstance]) -> Vec<ZonePlan<'a>> {
    let architects: Vec<_> = members
        .iter()
        .filter(|m| m.role_type == RoleType::Architect)
        .copied()
        .collect();
    let managers: Vec<_> = members
        .iter()
        .filter(|m| m.role_type == RoleType::Manager)
        .copied()
        .collect();
    let engineers: Vec<_> = members
        .iter()
        .filter(|m| m.role_type == RoleType::Engineer)
        .copied()
        .collect();

    let mut zones = Vec::new();
    let total = members.len() as u32;

    if !architects.is_empty() {
        let pct = ((architects.len() as u32 * 100) / total).max(10);
        zones.push(ZonePlan {
            width_pct: pct,
            members: architects,
            horizontal_columns: 1,
        });
    }
    if !managers.is_empty() {
        let pct = ((managers.len() as u32 * 100) / total).max(15);
        zones.push(ZonePlan {
            width_pct: pct,
            members: managers,
            horizontal_columns: 1,
        });
    }
    if !engineers.is_empty() {
        let pct = ((engineers.len() as u32 * 100) / total).max(20);
        zones.push(ZonePlan {
            width_pct: pct,
            members: engineers,
            horizontal_columns: 1,
        });
    }

    zones
}

#[cfg(test)]
mod tests {
    use super::super::config::TeamConfig;
    use super::super::hierarchy;
    use super::*;
    use serial_test::serial;
    use std::process::Command;

    fn make_members(yaml: &str) -> Vec<MemberInstance> {
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        hierarchy::resolve_hierarchy(&config).unwrap()
    }

    #[test]
    fn auto_zones_group_by_role() {
        let members = make_members(
            r#"
name: test
roles:
  - name: architect
    role_type: architect
    agent: claude
    instances: 1
  - name: manager
    role_type: manager
    agent: claude
    instances: 2
  - name: engineer
    role_type: engineer
    agent: codex
    instances: 3
"#,
        );
        let pane_members: Vec<_> = members
            .iter()
            .filter(|m| m.role_type != RoleType::User)
            .collect();
        let zones = build_zones_auto(&pane_members);
        assert_eq!(zones.len(), 3);
        assert_eq!(zones[0].members.len(), 1); // architect
        assert_eq!(zones[1].members.len(), 2); // managers
        assert_eq!(zones[2].members.len(), 6); // engineers (2 managers × 3 each)
    }

    #[test]
    fn config_zones_assign_members() {
        let members = make_members(
            r#"
name: test
roles:
  - name: architect
    role_type: architect
    agent: claude
    instances: 1
  - name: manager
    role_type: manager
    agent: claude
    instances: 1
  - name: engineer
    role_type: engineer
    agent: codex
    instances: 3
"#,
        );
        let layout = LayoutConfig {
            zones: vec![
                super::super::config::ZoneDef {
                    name: "architect".to_string(),
                    width_pct: 20,
                    split: None,
                },
                super::super::config::ZoneDef {
                    name: "managers".to_string(),
                    width_pct: 30,
                    split: None,
                },
                super::super::config::ZoneDef {
                    name: "engineers".to_string(),
                    width_pct: 50,
                    split: None,
                },
            ],
        };
        let pane_members: Vec<_> = members
            .iter()
            .filter(|m| m.role_type != RoleType::User)
            .collect();
        let zones = build_zones_from_config(&layout, &pane_members);
        assert_eq!(zones.len(), 3);
        assert_eq!(zones[0].members[0].role_type, RoleType::Architect);
        assert_eq!(zones[1].members[0].role_type, RoleType::Manager);
        assert_eq!(zones[2].members.len(), 3);
    }

    #[test]
    fn split_percentages_preserve_equal_zone_stack() {
        let splits: Vec<_> = (2..=6).map(split_off_current_member_pct).collect();
        assert_eq!(splits, vec![50, 33, 25, 20, 17]);
    }

    #[test]
    #[serial]
    fn build_layout_supports_architect_two_managers_and_six_engineers() {
        let session = "batty-test-team-layout-nine";
        let _ = crate::tmux::kill_session(session);

        let members = make_members(
            r#"
name: test
roles:
  - name: architect
    role_type: architect
    agent: codex
  - name: manager
    role_type: manager
    agent: codex
    instances: 2
  - name: engineer
    role_type: engineer
    agent: codex
    instances: 3
    talks_to: [manager]
"#,
        );

        let layout = Some(LayoutConfig {
            zones: vec![
                super::super::config::ZoneDef {
                    name: "architect".to_string(),
                    width_pct: 15,
                    split: None,
                },
                super::super::config::ZoneDef {
                    name: "managers".to_string(),
                    width_pct: 25,
                    split: Some(super::super::config::SplitDef { horizontal: 2 }),
                },
                super::super::config::ZoneDef {
                    name: "engineers".to_string(),
                    width_pct: 60,
                    split: Some(super::super::config::SplitDef { horizontal: 6 }),
                },
            ],
        });

        let pane_map = build_layout(
            session,
            &members,
            &layout,
            Path::new("/tmp"),
            WorkflowMode::Legacy,
            true,
            OrchestratorPosition::Bottom,
        )
        .unwrap();
        assert_eq!(pane_map.len(), 9);

        let pane_count_output = Command::new("tmux")
            .args(["list-panes", "-t", session, "-F", "#{pane_id}"])
            .output()
            .unwrap();
        assert!(pane_count_output.status.success());
        let pane_count = String::from_utf8_lossy(&pane_count_output.stdout)
            .lines()
            .count();
        assert_eq!(pane_count, 9);

        let engineer_geometry_output = Command::new("tmux")
            .args([
                "list-panes",
                "-t",
                session,
                "-F",
                "#{pane_title} #{pane_left} #{pane_height}",
            ])
            .output()
            .unwrap();
        assert!(engineer_geometry_output.status.success());
        let mut engineer_columns: HashMap<u32, Vec<u32>> = HashMap::new();
        for line in String::from_utf8_lossy(&engineer_geometry_output.stdout).lines() {
            let parts: Vec<_> = line.split_whitespace().collect();
            if parts.len() != 3 || !parts[0].starts_with("eng-") {
                continue;
            }
            let left: u32 = parts[1].parse().unwrap();
            let height: u32 = parts[2].parse().unwrap();
            engineer_columns.entry(left).or_default().push(height);
        }
        assert_eq!(engineer_columns.len(), 2);
        assert!(engineer_columns.values().all(|heights| heights.len() == 3));
        for heights in engineer_columns.values() {
            assert!(heights.iter().all(|height| *height >= 4));
            let min_height = heights.iter().min().copied().unwrap();
            let max_height = heights.iter().max().copied().unwrap();
            // tmux rounds pane sizes slightly differently across platforms
            // once pane borders/status lines are enabled. We only need to
            // ensure the engineer stacks stay materially balanced.
            assert!(max_height - min_height <= 2);
        }

        crate::tmux::kill_session(session).unwrap();
    }

    #[test]
    fn auto_zones_single_role_type_produces_one_zone() {
        let members = make_members(
            r#"
name: test
roles:
  - name: engineer
    role_type: engineer
    agent: codex
    instances: 4
"#,
        );
        let pane_members: Vec<_> = members
            .iter()
            .filter(|m| m.role_type != RoleType::User)
            .collect();
        let zones = build_zones_auto(&pane_members);
        assert_eq!(zones.len(), 1);
        assert_eq!(zones[0].members.len(), 4);
    }

    #[test]
    fn split_zone_subgroups_groups_engineers_by_manager() {
        let members = make_members(
            r#"
name: test
roles:
  - name: manager
    role_type: manager
    agent: claude
    instances: 2
  - name: engineer
    role_type: engineer
    agent: codex
    instances: 2
    talks_to: [manager]
"#,
        );
        let engineers: Vec<_> = members
            .iter()
            .filter(|m| m.role_type == RoleType::Engineer)
            .collect();
        let subgroups = split_zone_subgroups(&engineers);
        assert_eq!(subgroups.len(), 2);
        assert_eq!(subgroups[0].len(), 2);
        assert_eq!(subgroups[1].len(), 2);
        // Each subgroup should share the same reports_to
        for group in &subgroups {
            let parent = group[0].reports_to.as_ref().unwrap();
            assert!(
                group
                    .iter()
                    .all(|m| m.reports_to.as_ref().unwrap() == parent)
            );
        }
    }

    #[test]
    fn config_zones_unplaced_members_go_to_last_zone() {
        let members = make_members(
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
    instances: 2
"#,
        );
        // Only define one zone — everything else should end up there
        let layout = LayoutConfig {
            zones: vec![super::super::config::ZoneDef {
                name: "architect".to_string(),
                width_pct: 100,
                split: None,
            }],
        };
        let pane_members: Vec<_> = members
            .iter()
            .filter(|m| m.role_type != RoleType::User)
            .collect();
        let zones = build_zones_from_config(&layout, &pane_members);
        // Architect zone gets architect + leftover manager/engineers
        assert_eq!(zones.len(), 1);
        assert_eq!(zones[0].members.len(), 4); // 1 arch + 1 mgr + 2 eng
    }

    #[test]
    fn config_zones_exact_manager_role_collects_direct_reports() {
        let members = make_members(
            r#"
name: test
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: scientist
    role_type: architect
    agent: claude
  - name: black-lead
    role_type: manager
    agent: claude
    talks_to: [architect, black-eng]
  - name: red-lead
    role_type: manager
    agent: claude
    talks_to: [architect, red-eng]
  - name: black-eng
    role_type: engineer
    agent: codex
    instances: 2
    talks_to: [black-lead]
  - name: red-eng
    role_type: engineer
    agent: codex
    instances: 2
    talks_to: [red-lead]
"#,
        );
        let layout = LayoutConfig {
            zones: vec![
                super::super::config::ZoneDef {
                    name: "scientist".to_string(),
                    width_pct: 10,
                    split: None,
                },
                super::super::config::ZoneDef {
                    name: "architect".to_string(),
                    width_pct: 10,
                    split: None,
                },
                super::super::config::ZoneDef {
                    name: "black-lead".to_string(),
                    width_pct: 40,
                    split: None,
                },
                super::super::config::ZoneDef {
                    name: "red-lead".to_string(),
                    width_pct: 40,
                    split: None,
                },
            ],
        };
        let pane_members: Vec<_> = members
            .iter()
            .filter(|m| m.role_type != RoleType::User)
            .collect();
        let zones = build_zones_from_config(&layout, &pane_members);
        assert_eq!(zones.len(), 4);
        assert_eq!(
            zones[0]
                .members
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
            vec!["scientist"]
        );
        assert_eq!(
            zones[1]
                .members
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
            vec!["architect"]
        );
        assert_eq!(
            zones[2]
                .members
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
            vec!["black-lead", "black-eng-1-1", "black-eng-1-2"]
        );
        assert_eq!(
            zones[3]
                .members
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
            vec!["red-lead", "red-eng-1-1", "red-eng-1-2"]
        );
    }

    #[test]
    fn workflow_mode_controls_orchestrator_pane_launch() {
        assert!(!should_launch_orchestrator_pane(WorkflowMode::Legacy, true));
        assert!(should_launch_orchestrator_pane(WorkflowMode::Hybrid, true));
        assert!(should_launch_orchestrator_pane(
            WorkflowMode::WorkflowFirst,
            true,
        ));
        assert!(!should_launch_orchestrator_pane(
            WorkflowMode::Hybrid,
            false
        ));
    }

    #[test]
    #[serial]
    fn build_layout_adds_orchestrator_pane_when_enabled() {
        let session = "batty-test-team-layout-orchestrator";
        let _ = crate::tmux::kill_session(session);
        let tmp = tempfile::tempdir().unwrap();

        let members = make_members(
            r#"
name: test
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: engineer
    role_type: engineer
    agent: codex
"#,
        );

        let pane_map = build_layout(
            session,
            &members,
            &None,
            tmp.path(),
            WorkflowMode::Hybrid,
            true,
            OrchestratorPosition::Bottom,
        )
        .unwrap();
        assert_eq!(pane_map.len(), 2);

        let panes_output = Command::new("tmux")
            .args([
                "list-panes",
                "-t",
                session,
                "-F",
                "#{pane_id} #{@batty_role}",
            ])
            .output()
            .unwrap();
        assert!(panes_output.status.success());
        let pane_roles = String::from_utf8_lossy(&panes_output.stdout);
        assert!(
            pane_roles
                .lines()
                .any(|line| line.ends_with(" orchestrator"))
        );
        assert_eq!(pane_roles.lines().count(), 3);
        assert!(tmp.path().join(".batty").join("orchestrator.log").exists());

        crate::tmux::kill_session(session).unwrap();
    }

    #[test]
    #[serial]
    fn build_layout_skips_orchestrator_pane_when_disabled() {
        let session = "batty-test-team-layout-no-orchestrator";
        let _ = crate::tmux::kill_session(session);
        let tmp = tempfile::tempdir().unwrap();

        let members = make_members(
            r#"
name: test
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: engineer
    role_type: engineer
    agent: codex
"#,
        );

        let pane_map = build_layout(
            session,
            &members,
            &None,
            tmp.path(),
            WorkflowMode::Hybrid,
            false,
            OrchestratorPosition::Bottom,
        )
        .unwrap();
        assert_eq!(pane_map.len(), 2);

        let pane_count_output = Command::new("tmux")
            .args(["list-panes", "-t", session, "-F", "#{pane_id}"])
            .output()
            .unwrap();
        assert!(pane_count_output.status.success());
        let pane_count = String::from_utf8_lossy(&pane_count_output.stdout)
            .lines()
            .count();
        assert_eq!(pane_count, 2);

        crate::tmux::kill_session(session).unwrap();
    }

    #[test]
    fn split_members_into_columns_balances_contiguous_groups() {
        let members = make_members(
            r#"
name: test
roles:
  - name: architect
    role_type: architect
    agent: claude
    instances: 5
"#,
        );
        let pane_members: Vec<_> = members.iter().collect();
        let columns = split_members_into_columns(&pane_members, 2);
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[0].len(), 3);
        assert_eq!(columns[1].len(), 2);
        assert_eq!(columns[0][0].name, pane_members[0].name);
        assert_eq!(columns[1][0].name, pane_members[3].name);
    }

    #[test]
    #[serial]
    fn build_layout_honors_horizontal_split_for_architect_zone() {
        let session = "batty-test-team-layout-architect-pair";
        let _ = crate::tmux::kill_session(session);

        let members = make_members(
            r#"
name: test
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: scientist
    role_type: architect
    agent: claude
"#,
        );

        let layout = Some(LayoutConfig {
            zones: vec![super::super::config::ZoneDef {
                name: "architect".to_string(),
                width_pct: 100,
                split: Some(super::super::config::SplitDef { horizontal: 2 }),
            }],
        });

        let pane_map = build_layout(
            session,
            &members,
            &layout,
            Path::new("/tmp"),
            WorkflowMode::Legacy,
            true,
            OrchestratorPosition::Bottom,
        )
        .unwrap();
        assert_eq!(pane_map.len(), 2);

        let geometry_output = Command::new("tmux")
            .args([
                "list-panes",
                "-t",
                session,
                "-F",
                "#{pane_title}\t#{pane_left}\t#{pane_top}\t#{pane_width}\t#{pane_height}",
            ])
            .output()
            .unwrap();
        assert!(geometry_output.status.success());

        let mut panes = HashMap::new();
        for line in String::from_utf8_lossy(&geometry_output.stdout).lines() {
            let parts: Vec<_> = line.split('\t').collect();
            if parts.len() != 5 {
                continue;
            }
            panes.insert(
                parts[0].to_string(),
                (
                    parts[1].parse::<u32>().unwrap(),
                    parts[2].parse::<u32>().unwrap(),
                    parts[3].parse::<u32>().unwrap(),
                    parts[4].parse::<u32>().unwrap(),
                ),
            );
        }

        let architect = panes.get("architect").unwrap();
        let scientist = panes.get("scientist").unwrap();
        assert_ne!(architect.0, scientist.0, "expected side-by-side panes");
        assert_eq!(architect.1, scientist.1, "expected aligned top edges");
        assert!(architect.3 > 0 && scientist.3 > 0);

        crate::tmux::kill_session(session).unwrap();
    }
}
