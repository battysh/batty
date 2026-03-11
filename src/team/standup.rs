//! Standup status gathering and injection into manager panes.

use std::collections::HashMap;

use anyhow::Result;

use super::config::RoleType;
use super::hierarchy::MemberInstance;
use super::watcher::SessionWatcher;
use crate::tmux;

/// Generate a standup report for a specific recipient, showing only their
/// direct reports.
pub fn generate_standup_for(
    recipient: &MemberInstance,
    members: &[MemberInstance],
    watchers: &HashMap<String, SessionWatcher>,
    states: &HashMap<String, MemberState>,
    output_lines: usize,
) -> String {
    let mut report = String::new();
    report.push_str(&format!("=== STANDUP for {} ===\n", recipient.name));

    // Only include members who report to this recipient
    let direct_reports: Vec<&MemberInstance> = members
        .iter()
        .filter(|m| m.reports_to.as_deref() == Some(&recipient.name))
        .collect();

    if direct_reports.is_empty() {
        report.push_str("(no direct reports)\n");
    } else {
        for member in &direct_reports {
            let state = states
                .get(&member.name)
                .copied()
                .unwrap_or(MemberState::Idle);
            let state_str = match state {
                MemberState::Idle => "idle",
                MemberState::Working => "working",
                MemberState::Completed => "completed",
                MemberState::Crashed => "CRASHED",
            };

            report.push_str(&format!("\n[{}] status: {}\n", member.name, state_str));

            if let Some(watcher) = watchers.get(&member.name) {
                let last = watcher.last_lines(output_lines);
                if !last.trim().is_empty() {
                    report.push_str("  recent output:\n");
                    for line in last.lines().take(output_lines) {
                        report.push_str(&format!("    {line}\n"));
                    }
                }
            }
        }
    }

    report.push_str("\n=== END STANDUP ===\n");
    report
}

/// Inject standup text into a pane via load-buffer + paste-buffer.
pub fn inject_standup(pane_id: &str, standup: &str) -> Result<()> {
    tmux::load_buffer(standup)?;
    tmux::paste_buffer(pane_id)?;
    // paste-buffer needs a moment to complete before we press Enter
    std::thread::sleep(std::time::Duration::from_millis(500));
    tmux::send_keys(pane_id, "", true)?;
    Ok(())
}

/// Simple member state enum used by standup reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberState {
    Idle,
    Working,
    Completed,
    Crashed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::config::RoleType;

    fn make_member(name: &str, role_type: RoleType, reports_to: Option<&str>) -> MemberInstance {
        MemberInstance {
            name: name.to_string(),
            role_name: name.to_string(),
            role_type,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: reports_to.map(|s| s.to_string()),
            use_worktrees: false,
        }
    }

    #[test]
    fn standup_shows_only_direct_reports() {
        let members = vec![
            make_member("architect", RoleType::Architect, None),
            make_member("manager", RoleType::Manager, Some("architect")),
            make_member("eng-1-1", RoleType::Engineer, Some("manager")),
            make_member("eng-1-2", RoleType::Engineer, Some("manager")),
        ];
        let watchers = HashMap::new();
        let mut states = HashMap::new();
        states.insert("eng-1-1".to_string(), MemberState::Working);
        states.insert("eng-1-2".to_string(), MemberState::Idle);
        states.insert("architect".to_string(), MemberState::Working);

        // Manager standup should only show engineers, not architect
        let manager = &members[1];
        let report = generate_standup_for(manager, &members, &watchers, &states, 5);
        assert!(report.contains("[eng-1-1] status: working"));
        assert!(report.contains("[eng-1-2] status: idle"));
        assert!(!report.contains("[architect]"));
        assert!(report.contains("STANDUP for manager"));
    }

    #[test]
    fn standup_architect_sees_manager() {
        let members = vec![
            make_member("architect", RoleType::Architect, None),
            make_member("manager", RoleType::Manager, Some("architect")),
            make_member("eng-1-1", RoleType::Engineer, Some("manager")),
        ];
        let watchers = HashMap::new();
        let states = HashMap::new();

        let architect = &members[0];
        let report = generate_standup_for(architect, &members, &watchers, &states, 5);
        assert!(report.contains("[manager]"));
        assert!(!report.contains("[eng-1-1]"));
    }

    #[test]
    fn standup_no_reports_for_engineer() {
        let members = vec![
            make_member("manager", RoleType::Manager, None),
            make_member("eng-1-1", RoleType::Engineer, Some("manager")),
        ];
        let watchers = HashMap::new();
        let states = HashMap::new();

        let eng = &members[1];
        let report = generate_standup_for(eng, &members, &watchers, &states, 5);
        assert!(report.contains("no direct reports"));
    }

    #[test]
    fn standup_excludes_user_role() {
        let members = vec![MemberInstance {
            name: "human".to_string(),
            role_name: "human".to_string(),
            role_type: RoleType::User,
            agent: None,
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        }];
        let report =
            generate_standup_for(&members[0], &members, &HashMap::new(), &HashMap::new(), 5);
        assert!(!report.contains("[human]"));
    }

    #[test]
    fn standup_shows_crashed_state() {
        let members = vec![
            make_member("manager", RoleType::Manager, None),
            make_member("eng-1", RoleType::Engineer, Some("manager")),
        ];
        let mut states = HashMap::new();
        states.insert("eng-1".to_string(), MemberState::Crashed);

        let manager = &members[0];
        let report = generate_standup_for(manager, &members, &HashMap::new(), &states, 5);
        assert!(report.contains("CRASHED"));
    }
}
