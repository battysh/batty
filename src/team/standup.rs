//! Standup status gathering and delivery helpers.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::config::{PlanningDirectiveFile, RoleType, TeamConfig, load_planning_directive};
use super::hierarchy::MemberInstance;
use super::metrics;
use super::telegram::TelegramBot;
use super::watcher::SessionWatcher;
use super::{pause_marker_path, team_config_dir};
use crate::task;
use crate::tmux;

const REVIEW_POLICY_MAX_CHARS: usize = 2_000;

/// Generate a standup report for a specific recipient, showing only their
/// direct reports.
#[cfg_attr(not(test), allow(dead_code))]
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
    prepend_review_policy_context(board_dir, report)
}

fn prepend_review_policy_context(board_dir: Option<&Path>, report: String) -> String {
    let Some(project_root) = project_root_from_board_dir(board_dir) else {
        return report;
    };
    match load_planning_directive(
        project_root,
        PlanningDirectiveFile::ReviewPolicy,
        REVIEW_POLICY_MAX_CHARS,
    ) {
        Ok(Some(policy)) => format!("Review policy context:\n{policy}\n\n{report}"),
        Ok(None) => report,
        Err(error) => {
            warn!(error = %error, "failed to load review policy for standup");
            report
        }
    }
}

fn project_root_from_board_dir(board_dir: Option<&Path>) -> Option<&Path> {
    let board_dir = board_dir?;
    let team_config = board_dir.parent()?;
    if team_config.file_name()? != "team_config" {
        return None;
    }
    let batty_dir = team_config.parent()?;
    if batty_dir.file_name()? != ".batty" {
        return None;
    }
    batty_dir.parent()
}

pub(crate) fn maybe_generate_standup(
    project_root: &Path,
    team_config: &TeamConfig,
    members: &[MemberInstance],
    watchers: &HashMap<String, SessionWatcher>,
    states: &HashMap<String, MemberState>,
    pane_map: &HashMap<String, String>,
    telegram_bot: Option<&TelegramBot>,
    paused_standups: &HashSet<String>,
    last_standup: &mut HashMap<String, Instant>,
) -> Result<Vec<String>> {
    if !team_config.automation.standups {
        return Ok(Vec::new());
    }
    if pause_marker_path(project_root).exists() {
        return Ok(Vec::new());
    }
    let global_interval = team_config.standup.interval_secs;
    if global_interval == 0 {
        return Ok(Vec::new());
    }

    let mut recipients = Vec::new();
    for role in &team_config.roles {
        let receives = role.receives_standup.unwrap_or(matches!(
            role.role_type,
            RoleType::Manager | RoleType::Architect
        ));
        if !receives {
            continue;
        }
        let interval = Duration::from_secs(role.standup_interval_secs.unwrap_or(global_interval));
        for member in members {
            if member.role_name == role.name {
                recipients.push((member.clone(), interval));
            }
        }
    }

    let mut generated_recipients = Vec::new();

    for (recipient, interval) in &recipients {
        if paused_standups.contains(&recipient.name) {
            continue;
        }

        let last = last_standup.get(&recipient.name).copied();
        let should_fire = match last {
            Some(instant) => instant.elapsed() >= *interval,
            None => true,
        };

        if last.is_none() {
            last_standup.insert(recipient.name.clone(), Instant::now());
            continue;
        }
        if !should_fire {
            continue;
        }

        let board_dir = team_config_dir(project_root).join("board");
        let report = generate_board_aware_standup_for(
            recipient,
            members,
            watchers,
            states,
            team_config.standup.output_lines as usize,
            Some(&board_dir),
        );

        match recipient.role_type {
            RoleType::User => {
                if let Some(bot) = telegram_bot {
                    let chat_id = team_config
                        .roles
                        .iter()
                        .find(|role| {
                            role.role_type == RoleType::User && role.name == recipient.role_name
                        })
                        .and_then(|role| role.channel_config.as_ref())
                        .map(|config| config.target.clone());

                    match chat_id {
                        Some(chat_id) => {
                            if let Err(error) = bot.send_message(&chat_id, &report) {
                                warn!(
                                    member = %recipient.name,
                                    target = %chat_id,
                                    error = %error,
                                    "failed to send standup via telegram"
                                );
                            } else {
                                generated_recipients.push(recipient.name.clone());
                            }
                        }
                        None => warn!(
                            member = %recipient.name,
                            "telegram standup delivery skipped: missing target"
                        ),
                    }
                } else {
                    match write_standup_file(project_root, &report) {
                        Ok(path) => {
                            tracing::info!(member = %recipient.name, path = %path.display(), "standup written to file");
                            generated_recipients.push(recipient.name.clone());
                        }
                        Err(error) => warn!(
                            member = %recipient.name,
                            error = %error,
                            "failed to write standup file"
                        ),
                    }
                }
            }
            _ => {
                if let Some(pane_id) = pane_map.get(&recipient.name) {
                    if let Err(error) = inject_standup(pane_id, &report) {
                        warn!(member = %recipient.name, error = %error, "failed to inject standup");
                    } else {
                        generated_recipients.push(recipient.name.clone());
                    }
                }
            }
        }

        last_standup.insert(recipient.name.clone(), Instant::now());
    }

    if !generated_recipients.is_empty() {
        tracing::info!("standups generated and delivered");
    }

    Ok(generated_recipients)
}

pub(crate) fn update_timer_for_state(
    team_config: &TeamConfig,
    members: &[MemberInstance],
    paused_standups: &mut HashSet<String>,
    last_standup: &mut HashMap<String, Instant>,
    member_name: &str,
    new_state: MemberState,
) {
    if standup_interval_for_member_name(team_config, members, member_name).is_none() {
        paused_standups.remove(member_name);
        last_standup.remove(member_name);
        return;
    }

    match new_state {
        MemberState::Working => {
            paused_standups.insert(member_name.to_string());
            last_standup.remove(member_name);
        }
        MemberState::Idle => {
            let was_paused = paused_standups.remove(member_name);
            if was_paused || !last_standup.contains_key(member_name) {
                last_standup.insert(member_name.to_string(), Instant::now());
            }
        }
    }
}

pub(crate) fn standup_interval_for_member_name(
    team_config: &TeamConfig,
    members: &[MemberInstance],
    member_name: &str,
) -> Option<Duration> {
    let member = members.iter().find(|member| member.name == member_name)?;
    let role_def = team_config
        .roles
        .iter()
        .find(|role| role.name == member.role_name);

    let receives = role_def
        .and_then(|role| role.receives_standup)
        .unwrap_or(matches!(
            member.role_type,
            RoleType::Manager | RoleType::Architect
        ));
    if !receives {
        return None;
    }

    let interval_secs = role_def
        .and_then(|role| role.standup_interval_secs)
        .unwrap_or(team_config.standup.interval_secs);
    Some(Duration::from_secs(interval_secs))
}

pub(crate) fn restore_timer_state(
    last_standup_elapsed_secs: HashMap<String, u64>,
) -> HashMap<String, Instant> {
    last_standup_elapsed_secs
        .into_iter()
        .map(|(member, elapsed_secs)| {
            (
                member,
                Instant::now()
                    .checked_sub(Duration::from_secs(elapsed_secs))
                    .unwrap_or_else(Instant::now),
            )
        })
        .collect()
}

pub(crate) fn snapshot_timer_state(
    last_standup: &HashMap<String, Instant>,
) -> HashMap<String, u64> {
    last_standup
        .iter()
        .map(|(member, instant)| (member.clone(), instant.elapsed().as_secs()))
        .collect()
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

/// Write standup text to a timestamped Markdown file under `.batty/standups/`.
pub fn write_standup_file(project_root: &Path, standup: &str) -> Result<PathBuf> {
    let standups_dir = project_root.join(".batty").join("standups");
    std::fs::create_dir_all(&standups_dir)
        .with_context(|| format!("failed to create {}", standups_dir.display()))?;

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before UNIX_EPOCH")?
        .as_millis();
    let path = standups_dir.join(format!("{timestamp}.md"));

    std::fs::write(&path, standup)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
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
    use crate::team::config::{
        AutomationConfig, BoardConfig, OrchestratorPosition, RoleDef, RoleType, StandupConfig,
        TeamConfig, WorkflowMode, WorkflowPolicy,
    };
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

    #[test]
    fn board_aware_standup_prepends_review_policy_context() {
        let tmp = tempfile::tempdir().unwrap();
        let team_config_dir = tmp.path().join(".batty").join("team_config");
        let board_dir = team_config_dir.join("board");
        std::fs::create_dir_all(&board_dir).unwrap();
        std::fs::write(
            team_config_dir.join("review_policy.md"),
            "Approve only after tests pass.",
        )
        .unwrap();

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
            Some(&board_dir),
        );

        assert!(report.starts_with("Review policy context:\nApprove only after tests pass."));
        assert!(report.contains("=== STANDUP for manager ==="));
    }

    #[test]
    fn board_aware_standup_reloads_updated_review_policy_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let team_config_dir = tmp.path().join(".batty").join("team_config");
        let board_dir = team_config_dir.join("board");
        std::fs::create_dir_all(&board_dir).unwrap();
        let policy_path = team_config_dir.join("review_policy.md");
        std::fs::write(&policy_path, "Initial policy").unwrap();

        let members = vec![
            make_member("manager", RoleType::Manager, None),
            make_member("eng-1", RoleType::Engineer, Some("manager")),
        ];
        let states = HashMap::from([("eng-1".to_string(), MemberState::Idle)]);

        let first = generate_board_aware_standup_for(
            &members[0],
            &members,
            &HashMap::new(),
            &states,
            5,
            Some(&board_dir),
        );
        std::fs::write(&policy_path, "Updated policy").unwrap();
        let second = generate_board_aware_standup_for(
            &members[0],
            &members,
            &HashMap::new(),
            &states,
            5,
            Some(&board_dir),
        );

        assert!(first.contains("Initial policy"));
        assert!(second.contains("Updated policy"));
        assert!(!second.contains("Initial policy"));
    }

    #[test]
    fn write_standup_file_creates_timestamped_markdown_in_batty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let report = "=== STANDUP for user ===\n[architect] status: working\n";
        let expected_dir = tmp.path().join(".batty").join("standups");

        let path = write_standup_file(tmp.path(), report).unwrap();

        assert_eq!(path.parent(), Some(expected_dir.as_path()));
        assert_eq!(path.extension().and_then(|ext| ext.to_str()), Some("md"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), report);
    }

    #[test]
    fn update_timer_for_state_pauses_while_working_and_restarts_on_idle() {
        let member = make_member("manager", RoleType::Manager, None);
        let role = RoleDef {
            name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            instances: 1,
            prompt: None,
            talks_to: vec![],
            channel: None,
            channel_config: None,
            nudge_interval_secs: None,
            receives_standup: Some(true),
            standup_interval_secs: Some(600),
            owns: Vec::new(),
            use_worktrees: false,
        };
        let team_config = TeamConfig {
            name: "test".to_string(),
            workflow_mode: WorkflowMode::Legacy,
            workflow_policy: WorkflowPolicy::default(),
            board: BoardConfig::default(),
            standup: StandupConfig::default(),
            automation: AutomationConfig::default(),
            automation_sender: None,
            orchestrator_pane: true,
            orchestrator_position: OrchestratorPosition::Bottom,
            layout: None,
            cost: Default::default(),
            event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
            roles: vec![role],
        };
        let members = vec![member];
        let mut paused_standups = HashSet::new();
        let mut last_standup = HashMap::from([(
            "manager".to_string(),
            Instant::now() - Duration::from_secs(120),
        )]);

        update_timer_for_state(
            &team_config,
            &members,
            &mut paused_standups,
            &mut last_standup,
            "manager",
            MemberState::Working,
        );

        assert!(paused_standups.contains("manager"));
        assert!(!last_standup.contains_key("manager"));

        update_timer_for_state(
            &team_config,
            &members,
            &mut paused_standups,
            &mut last_standup,
            "manager",
            MemberState::Idle,
        );

        assert!(!paused_standups.contains("manager"));
        assert!(last_standup["manager"].elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn maybe_generate_standup_skips_when_global_interval_is_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let member = make_member("manager", RoleType::Manager, None);
        let role = RoleDef {
            name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            instances: 1,
            prompt: None,
            talks_to: vec![],
            channel: None,
            channel_config: None,
            nudge_interval_secs: None,
            receives_standup: Some(true),
            standup_interval_secs: Some(600),
            owns: Vec::new(),
            use_worktrees: false,
        };
        let team_config = TeamConfig {
            name: "test".to_string(),
            workflow_mode: WorkflowMode::Legacy,
            workflow_policy: WorkflowPolicy::default(),
            board: BoardConfig::default(),
            standup: StandupConfig {
                interval_secs: 0,
                output_lines: 30,
            },
            automation: AutomationConfig::default(),
            automation_sender: None,
            orchestrator_pane: false,
            orchestrator_position: OrchestratorPosition::Bottom,
            layout: None,
            cost: Default::default(),
            event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
            roles: vec![role],
        };
        let members = vec![member];
        let mut last_standup = HashMap::new();

        let generated = maybe_generate_standup(
            tmp.path(),
            &team_config,
            &members,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            None,
            &HashSet::new(),
            &mut last_standup,
        )
        .unwrap();

        assert!(generated.is_empty());
        assert!(last_standup.is_empty());
    }

    #[test]
    fn maybe_generate_standup_writes_user_report_to_file_without_telegram_bot() {
        let tmp = tempfile::tempdir().unwrap();
        let user = MemberInstance {
            name: "user".to_string(),
            role_name: "user".to_string(),
            role_type: RoleType::User,
            agent: None,
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let architect = MemberInstance {
            name: "architect".to_string(),
            role_name: "architect".to_string(),
            role_type: RoleType::Architect,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("user".to_string()),
            use_worktrees: false,
        };
        let user_role = RoleDef {
            name: "user".to_string(),
            role_type: RoleType::User,
            agent: None,
            instances: 1,
            prompt: None,
            talks_to: vec!["architect".to_string()],
            channel: None,
            channel_config: None,
            nudge_interval_secs: None,
            receives_standup: Some(true),
            standup_interval_secs: Some(1),
            owns: Vec::new(),
            use_worktrees: false,
        };
        let architect_role = RoleDef {
            name: "architect".to_string(),
            role_type: RoleType::Architect,
            agent: Some("claude".to_string()),
            instances: 1,
            prompt: None,
            talks_to: vec![],
            channel: None,
            channel_config: None,
            nudge_interval_secs: None,
            receives_standup: Some(false),
            standup_interval_secs: None,
            owns: Vec::new(),
            use_worktrees: false,
        };
        let team_config = TeamConfig {
            name: "test".to_string(),
            workflow_mode: WorkflowMode::Legacy,
            workflow_policy: WorkflowPolicy::default(),
            board: BoardConfig::default(),
            standup: StandupConfig {
                interval_secs: 1,
                output_lines: 30,
            },
            automation: AutomationConfig::default(),
            automation_sender: None,
            orchestrator_pane: false,
            orchestrator_position: OrchestratorPosition::Bottom,
            layout: None,
            cost: Default::default(),
            event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
            roles: vec![user_role, architect_role],
        };
        let members = vec![user.clone(), architect];
        let states = HashMap::from([("architect".to_string(), MemberState::Working)]);
        let mut last_standup =
            HashMap::from([(user.name.clone(), Instant::now() - Duration::from_secs(5))]);

        let generated = maybe_generate_standup(
            tmp.path(),
            &team_config,
            &members,
            &HashMap::new(),
            &states,
            &HashMap::new(),
            None,
            &HashSet::new(),
            &mut last_standup,
        )
        .unwrap();

        assert_eq!(generated, vec!["user".to_string()]);

        let standups_dir = tmp.path().join(".batty").join("standups");
        let entries = std::fs::read_dir(&standups_dir)
            .unwrap()
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(entries.len(), 1);

        let report = std::fs::read_to_string(entries[0].path()).unwrap();
        assert!(report.contains("=== STANDUP for user ==="));
        assert!(report.contains("[architect] status: working"));
    }
}
