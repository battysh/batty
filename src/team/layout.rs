//! tmux layout builder — creates zones and panes from team hierarchy.
//!
//! Zones are vertical columns in the tmux window. Within each zone, members
//! are stacked horizontally (top-to-bottom splits).

use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Result};
use tracing::{debug, info};

use super::config::{LayoutConfig, RoleType};
use super::hierarchy::MemberInstance;
use crate::tmux;

/// Build the tmux layout for a team session.
///
/// Creates the session with the first member's pane, then splits to create
/// all remaining panes. Returns a mapping of member name → tmux pane target.
pub fn build_layout(
    session: &str,
    members: &[MemberInstance],
    layout: &Option<LayoutConfig>,
    project_root: &Path,
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

    if pane_members.len() == 1 {
        // Single pane — just use the initial pane
        let pane_id = tmux::pane_id(session)?;
        set_pane_title(session, &pane_id, &pane_members[0].name)?;
        pane_map.insert(pane_members[0].name.clone(), pane_id);
        return Ok(pane_map);
    }

    // Group members by zone for layout
    let zones = if let Some(layout_config) = layout {
        build_zones_from_config(layout_config, &pane_members)
    } else {
        build_zones_auto(&pane_members)
    };

    // Keep the initial pane unlabeled until the per-zone vertical layout is
    // built, so multi-member zones can use it as the remaining container.
    let initial_pane = tmux::pane_id(session)?;

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
    let mut zone_panes: Vec<String> = vec![initial_pane.clone()];
    let mut remaining_pct: u32 = zones.iter().map(|(p, _)| *p).sum();
    for (i, (_width_pct, _zone_members)) in zones.iter().enumerate().skip(1) {
        let right_side: u32 = zones[i..].iter().map(|(p, _)| *p).sum();
        let split_pct = ((right_side as f64 / remaining_pct as f64) * 100.0).round() as u32;
        let split_pct = split_pct.max(10).min(90);
        let split_from = zone_panes.last().unwrap();
        let pane_id = tmux::split_window_horizontal(split_from, split_pct)?;
        zone_panes.push(pane_id);
        remaining_pct = right_side;
        debug!(zone = i, split_pct, "created zone column");
    }

    // Within each zone, split vertically for members
    for (zone_idx, (_, zone_members)) in zones.iter().enumerate() {
        let zone_pane = &zone_panes[zone_idx];

        if zone_members.is_empty() {
            continue;
        }

        let remaining_pane = zone_pane.clone();

        for member_idx in (1..zone_members.len()).rev() {
            let member = zone_members[member_idx];
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

        set_pane_title(session, &remaining_pane, &zone_members[0].name)?;
        pane_map.insert(zone_members[0].name.clone(), remaining_pane.clone());
    }

    info!(session, panes = pane_map.len(), "team layout created");

    Ok(pane_map)
}

fn split_off_current_member_pct(total_slots: usize) -> u32 {
    (((1.0 / total_slots as f64) * 100.0).round() as u32)
        .max(10)
        .min(90)
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
) -> Vec<(u32, Vec<&'a MemberInstance>)> {
    let mut zones: Vec<(u32, Vec<&MemberInstance>)> = config
        .zones
        .iter()
        .map(|z| (z.width_pct, Vec::new()))
        .collect();

    // Map members to zones by role type
    let mut member_queue: Vec<&MemberInstance> = members.to_vec();

    for (zone_idx, zone_def) in config.zones.iter().enumerate() {
        // Try to match zone name to role types
        let target_types = match zone_def.name.as_str() {
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
                zones[zone_idx].1.push(m);
                taken += 1;
                false
            } else {
                true
            }
        });
    }

    // Put any unplaced members in the last zone
    if let Some(last) = zones.last_mut() {
        last.1.extend(member_queue);
    }

    // Remove empty zones
    zones.retain(|(_pct, members)| !members.is_empty());
    zones
}

/// Auto-generate zones from member role types.
fn build_zones_auto<'a>(members: &'a [&MemberInstance]) -> Vec<(u32, Vec<&'a MemberInstance>)> {
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
        zones.push((pct, architects));
    }
    if !managers.is_empty() {
        let pct = ((managers.len() as u32 * 100) / total).max(15);
        zones.push((pct, managers));
    }
    if !engineers.is_empty() {
        let pct = ((engineers.len() as u32 * 100) / total).max(20);
        zones.push((pct, engineers));
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
        assert_eq!(zones[0].1.len(), 1); // architect
        assert_eq!(zones[1].1.len(), 2); // managers
        assert_eq!(zones[2].1.len(), 6); // engineers (2 managers × 3 each)
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
        assert_eq!(zones[0].1[0].role_type, RoleType::Architect);
        assert_eq!(zones[1].1[0].role_type, RoleType::Manager);
        assert_eq!(zones[2].1.len(), 3);
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

        let pane_map = build_layout(session, &members, &layout, Path::new("/tmp")).unwrap();
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

        let engineer_heights_output = Command::new("tmux")
            .args([
                "list-panes",
                "-t",
                session,
                "-F",
                "#{pane_title} #{pane_height}",
            ])
            .output()
            .unwrap();
        assert!(engineer_heights_output.status.success());
        let engineer_heights: Vec<u32> = String::from_utf8_lossy(&engineer_heights_output.stdout)
            .lines()
            .filter_map(|line| {
                let (title, height) = line.rsplit_once(' ')?;
                if !title.starts_with("eng-") {
                    return None;
                }
                height.parse().ok()
            })
            .collect();
        assert_eq!(engineer_heights.len(), 6);
        assert!(engineer_heights.iter().all(|height| *height >= 4));

        crate::tmux::kill_session(session).unwrap();
    }
}
