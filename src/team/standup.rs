//! Standup status gathering and injection into manager panes.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::hierarchy::MemberInstance;
use super::metrics;
use super::watcher::SessionWatcher;
use crate::task;
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
    generate_board_aware_standup_for(recipient, members, watchers, states, output_lines, None)
}

/// Generate a standup report for a specific recipient, optionally enriching the
/// report with board-derived task ownership and workflow signals.
pub fn generate_board_aware_standup_for(
    recipient: &MemberInstance,
    members: &[MemberInstance],
    watchers: &HashMap<String, SessionWatcher>,
    states: &HashMap<String, MemberState>,
    output_lines: usize,
    board_dir: Option<&Path>,
) -> String {
    let board_context = load_board_context(board_dir, members);
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
            };

            report.push_str(&format!("\n[{}] status: {}\n", member.name, state_str));

            if let Some(board_context) = &board_context {
                let assigned_ids = board_context.assigned_task_ids.get(&member.name);
                report.push_str(&format!(
                    "  assigned tasks: {}\n",
                    format_assigned_task_ids(assigned_ids)
                ));

                if board_context
                    .idle_with_runnable
                    .contains(member.name.as_str())
                {
                    report.push_str("  warning: idle while runnable work exists on the board\n");
                }
            }

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

    if let Some(board_context) = &board_context {
        let idle_reports = direct_reports
            .iter()
            .filter(|member| {
                board_context
                    .idle_with_runnable
                    .contains(member.name.as_str())
            })
            .map(|member| member.name.as_str())
            .collect::<Vec<_>>();

        report.push_str("\nWorkflow signals:\n");
        report.push_str(&format!(
            "  blocked tasks: {}\n",
            board_context.metrics.blocked_count
        ));
        report.push_str(&format!(
            "  oldest review age: {}\n",
            format_age(board_context.metrics.oldest_review_age_secs)
        ));
        if !idle_reports.is_empty() {
            report.push_str(&format!(
                "  idle with runnable: {}\n",
                idle_reports.join(", ")
            ));
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberState {
    Idle,
    Working,
}

#[derive(Debug, Clone)]
struct BoardContext {
    metrics: metrics::WorkflowMetrics,
    assigned_task_ids: HashMap<String, Vec<u32>>,
    idle_with_runnable: HashSet<String>,
}

fn load_board_context(
    board_dir: Option<&Path>,
    members: &[MemberInstance],
) -> Option<BoardContext> {
    let board_dir = board_dir?;
    let tasks_dir = board_dir.join("tasks");
    if !tasks_dir.is_dir() {
        return None;
    }

    let metrics = metrics::compute_metrics(board_dir, members).ok()?;
    let tasks = task::load_tasks_from_dir(&tasks_dir).ok()?;
    let mut assigned_task_ids = HashMap::<String, Vec<u32>>::new();

    for task in tasks
        .into_iter()
        .filter(|task| !matches!(task.status.as_str(), "done" | "archived"))
    {
        let Some(claimed_by) = task.claimed_by else {
            continue;
        };
        assigned_task_ids
            .entry(claimed_by)
            .or_default()
            .push(task.id);
    }

    for task_ids in assigned_task_ids.values_mut() {
        task_ids.sort_unstable();
    }

    Some(BoardContext {
        idle_with_runnable: metrics.idle_with_runnable.iter().cloned().collect(),
        metrics,
        assigned_task_ids,
    })
}

fn format_assigned_task_ids(task_ids: Option<&Vec<u32>>) -> String {
    let Some(task_ids) = task_ids else {
        return "none".to_string();
    };

    if task_ids.is_empty() {
        "none".to_string()
    } else {
        task_ids
            .iter()
            .map(|task_id| format!("#{task_id}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn format_age(age_secs: Option<u64>) -> String {
    age_secs
        .map(|secs| format!("{secs}s"))
        .unwrap_or_else(|| "n/a".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::config::RoleType;
    use std::path::Path;

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

    fn write_task(
        board_dir: &Path,
        id: u32,
        title: &str,
        status: &str,
        claimed_by: Option<&str>,
        blocked: Option<&str>,
    ) {
        let tasks_dir = board_dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let mut content =
            format!("---\nid: {id}\ntitle: {title}\nstatus: {status}\npriority: medium\n");
        if let Some(claimed_by) = claimed_by {
            content.push_str(&format!("claimed_by: {claimed_by}\n"));
        }
        if let Some(blocked) = blocked {
            content.push_str(&format!("blocked: {blocked}\n"));
        }
        content.push_str("class: standard\n---\n\nTask body.\n");
        std::fs::write(tasks_dir.join(format!("{id:03}-{title}.md")), content).unwrap();
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
    fn test_generate_standup_for_formats_various_member_states() {
        let members = vec![
            make_member("manager", RoleType::Manager, None),
            make_member("eng-idle", RoleType::Engineer, Some("manager")),
            make_member("eng-working", RoleType::Engineer, Some("manager")),
        ];
        let mut states = HashMap::new();
        states.insert("eng-working".to_string(), MemberState::Working);

        let report = generate_standup_for(&members[0], &members, &HashMap::new(), &states, 5);

        assert!(report.contains("=== STANDUP for manager ==="));
        assert!(report.contains("[eng-idle] status: idle"));
        assert!(report.contains("[eng-working] status: working"));
        assert!(report.contains("=== END STANDUP ==="));
    }

    #[test]
    fn test_generate_standup_for_empty_members_returns_no_direct_reports() {
        let recipient = make_member("manager", RoleType::Manager, None);
        let report = generate_standup_for(&recipient, &[], &HashMap::new(), &HashMap::new(), 5);

        assert!(report.contains("=== STANDUP for manager ==="));
        assert!(report.contains("(no direct reports)"));
        assert!(report.contains("=== END STANDUP ==="));
    }

    #[test]
    fn test_generate_standup_for_all_same_status_lists_each_direct_report() {
        let members = vec![
            make_member("manager", RoleType::Manager, None),
            make_member("eng-1", RoleType::Engineer, Some("manager")),
            make_member("eng-2", RoleType::Engineer, Some("manager")),
            make_member("eng-3", RoleType::Engineer, Some("manager")),
        ];
        let states = HashMap::from([
            ("eng-1".to_string(), MemberState::Working),
            ("eng-2".to_string(), MemberState::Working),
            ("eng-3".to_string(), MemberState::Working),
        ]);

        let report = generate_standup_for(&members[0], &members, &HashMap::new(), &states, 5);

        assert_eq!(report.matches("status: working").count(), 3);
        assert!(report.contains("[eng-1] status: working"));
        assert!(report.contains("[eng-2] status: working"));
        assert!(report.contains("[eng-3] status: working"));
    }

    #[test]
    fn board_aware_standup_appends_task_ids_and_workflow_signals() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");
        write_task(&board_dir, 1, "active", "in-progress", Some("eng-1"), None);
        write_task(
            &board_dir,
            2,
            "blocked",
            "blocked",
            Some("eng-2"),
            Some("waiting"),
        );
        write_task(&board_dir, 3, "review", "review", Some("eng-2"), None);
        write_task(&board_dir, 4, "runnable", "todo", None, None);

        let members = vec![
            make_member("manager", RoleType::Manager, None),
            make_member("eng-1", RoleType::Engineer, Some("manager")),
            make_member("eng-2", RoleType::Engineer, Some("manager")),
            make_member("eng-3", RoleType::Engineer, Some("manager")),
        ];
        let states = HashMap::from([
            ("eng-1".to_string(), MemberState::Working),
            ("eng-2".to_string(), MemberState::Working),
            ("eng-3".to_string(), MemberState::Idle),
        ]);

        let report = generate_board_aware_standup_for(
            &members[0],
            &members,
            &HashMap::new(),
            &states,
            5,
            Some(&board_dir),
        );

        assert!(report.contains("assigned tasks: #1"));
        assert!(report.contains("assigned tasks: #2, #3"));
        assert!(report.contains("[eng-3] status: idle"));
        assert!(report.contains("assigned tasks: none"));
        assert!(report.contains("warning: idle while runnable work exists on the board"));
        assert!(report.contains("Workflow signals:"));
        assert!(report.contains("blocked tasks: 1"));
        assert!(report.contains("idle with runnable: eng-3"));
        assert!(report.contains("oldest review age: "));
        assert!(!report.contains("oldest review age: n/a"));
    }

    #[test]
    fn board_aware_standup_falls_back_when_board_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let missing_board_dir = tmp.path().join("missing-board");
        let members = vec![
            make_member("manager", RoleType::Manager, None),
            make_member("eng-1", RoleType::Engineer, Some("manager")),
        ];
        let states = HashMap::from([("eng-1".to_string(), MemberState::Idle)]);

        let report = generate_board_aware_standup_for(
            &members[0],
            &members,
            &HashMap::new(),
            &states,
            5,
            Some(&missing_board_dir),
        );

        assert!(report.contains("[eng-1] status: idle"));
        assert!(!report.contains("assigned tasks:"));
        assert!(!report.contains("Workflow signals:"));
        assert!(!report.contains("warning: idle while runnable work exists on the board"));
    }
}
