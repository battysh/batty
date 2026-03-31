//! Stall detection, restart, and escalation.

use std::time::Instant;

use anyhow::Result;
use tracing::warn;

use super::super::*;
use super::{CONTEXT_RESTART_COOLDOWN, format_checkpoint_section};

impl TeamDaemon {
    /// Handle a stalled agent — no output change for longer than the configured threshold.
    #[allow(dead_code)]
    pub(in super::super) fn handle_stalled_agent(
        &mut self,
        member_name: &str,
        stall_secs: u64,
    ) -> Result<()> {
        let Some(task) = self.active_task(member_name)? else {
            return Ok(());
        };
        let member = match self.config.members.iter().find(|m| m.name == member_name) {
            Some(m) => m.clone(),
            None => return Ok(()),
        };
        let Some(pane_id) = self.config.pane_map.get(member_name).cloned() else {
            return Ok(());
        };

        let stall_cooldown_key = format!("stall-restart::{member_name}");
        let on_cooldown = self
            .intervention_cooldowns
            .get(&stall_cooldown_key)
            .is_some_and(|last| last.elapsed() < CONTEXT_RESTART_COOLDOWN);
        if on_cooldown {
            return Ok(());
        }

        let task_id_str = task.id.to_string();
        let prior_restarts = self.stall_restart_count(task.id)?;
        let max_restarts = self.config.team_config.workflow_policy.max_stall_restarts;

        warn!(
            member = %member_name,
            task_id = task.id,
            stall_secs,
            prior_restarts,
            "agent stalled — no output change"
        );

        self.emit_event(TeamEvent::stall_detected(
            member_name,
            Some(task.id),
            stall_secs,
        ));
        self.record_orchestrator_action(format!(
            "stall: detected agent stall for {} on task #{} ({}s no output, {} prior restarts)",
            member_name, task.id, stall_secs, prior_restarts,
        ));

        if prior_restarts >= max_restarts {
            // Escalate to manager instead of restarting again.
            let escalation_key = format!("stall-escalation::{member_name}");
            let escalation_on_cooldown = self
                .intervention_cooldowns
                .get(&escalation_key)
                .is_some_and(|last| last.elapsed() < CONTEXT_RESTART_COOLDOWN);
            if escalation_on_cooldown {
                return Ok(());
            }
            self.escalate_stalled_agent(&member, &task, prior_restarts + 1)?;
            self.intervention_cooldowns
                .insert(escalation_key, Instant::now());
            return Ok(());
        }

        // Write progress checkpoint before restarting.
        let checkpoint = super::super::super::checkpoint::gather_checkpoint(
            &self.config.project_root,
            member_name,
            &task,
        );
        if let Err(error) = super::super::super::checkpoint::write_checkpoint(
            &self.config.project_root,
            &checkpoint,
        ) {
            warn!(member = %member_name, error = %error, "failed to write progress checkpoint");
        }

        // Restart the stalled agent with task context.
        tmux::respawn_pane(&pane_id, "bash")?;
        std::thread::sleep(std::time::Duration::from_millis(200));

        let assignment = Self::restart_assignment_message(&task);
        let launch = self.launch_task_assignment(member_name, &assignment, Some(task.id), false)?;
        let mut restart_notice = format!(
            "Restarted after stall ({}s no output). Continue task #{} from the current worktree state.",
            stall_secs, task.id
        );
        if let Some(branch) = launch.branch.as_deref() {
            restart_notice.push_str(&format!("\nBranch: {branch}"));
        }
        restart_notice.push_str(&format!("\nWorktree: {}", launch.work_dir.display()));
        // Include checkpoint content in restart notice.
        if let Some(cp_content) =
            super::super::super::checkpoint::read_checkpoint(&self.config.project_root, member_name)
        {
            restart_notice.push_str(&format_checkpoint_section(&cp_content));
        }
        if let Err(error) = self.queue_message("daemon", member_name, &restart_notice) {
            warn!(member = %member_name, error = %error, "failed to inject stall restart notice");
        }
        self.record_orchestrator_action(format!(
            "stall: relaunched {} on task #{} after {}s stall",
            member_name, task.id, stall_secs,
        ));
        self.intervention_cooldowns
            .insert(stall_cooldown_key, Instant::now());
        self.record_agent_restarted(member_name, task_id_str, "stalled", prior_restarts + 1);
        Ok(())
    }

    /// Escalate a stalled agent to its manager after max restarts exceeded.
    #[allow(dead_code)]
    fn escalate_stalled_agent(
        &mut self,
        member: &MemberInstance,
        task: &crate::task::Task,
        restart_count: u32,
    ) -> Result<()> {
        let Some(manager) = member.reports_to.as_deref() else {
            warn!(
                member = %member.name,
                task_id = task.id,
                restart_count,
                "stall exceeded restart limit with no escalation target"
            );
            return Ok(());
        };

        let body = format!(
            "Task #{task_id} for {member_name} stalled {restart_count} times (no output). \
             Batty restarted it {max} time(s) already and will not restart again automatically.\n\
             Task: {title}\n\
             Next step: decide whether to split the task, redirect the engineer, or intervene directly.",
            task_id = task.id,
            member_name = member.name,
            title = task.title,
            max = restart_count.saturating_sub(1),
        );
        self.queue_message("daemon", manager, &body)?;
        self.record_orchestrator_action(format!(
            "stall: escalated stall for {} on task #{} after {} stalls",
            member.name, task.id, restart_count,
        ));
        self.record_task_escalated(&member.name, task.id.to_string(), Some("stalled"));
        Ok(())
    }

    /// Count prior stall restarts for a given task from the event log.
    #[allow(dead_code)]
    pub(in super::super) fn stall_restart_count(&self, task_id: u32) -> Result<u32> {
        let events_path = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        let task_id = task_id.to_string();
        let count = super::super::super::events::read_events(&events_path)?
            .into_iter()
            .filter(|event| event.event == "agent_restarted")
            .filter(|event| event.task.as_deref() == Some(task_id.as_str()))
            .filter(|event| event.reason.as_deref() == Some("stalled"))
            .count() as u32;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::*;
    use super::super::test_helpers::test_team_config;
    use crate::team::config::{
        AutomationConfig, BoardConfig, OrchestratorPosition, RoleType, StandupConfig, TeamConfig,
        WorkflowMode, WorkflowPolicy,
    };
    use crate::team::events::TeamEvent;
    use crate::team::hierarchy::MemberInstance;
    use crate::team::standup::MemberState;
    use crate::team::test_helpers::{make_test_daemon, write_event_log};
    use crate::team::test_support::{
        TestDaemonBuilder, engineer_member, setup_fake_claude, write_owned_task_file,
        write_owned_task_file_with_context,
    };
    use serial_test::serial;
    use std::collections::HashMap;
    use std::process::Command;
    use std::time::{Duration, Instant};

    #[test]
    fn stall_restart_count_returns_zero_with_no_events() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let daemon = make_test_daemon(tmp.path(), vec![]);
        let count = daemon.stall_restart_count(42).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn stall_restart_count_counts_only_stalled_reason() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        write_event_log(
            tmp.path(),
            &[
                TeamEvent::agent_restarted("eng-1-1", "42", "context_exhausted", 1),
                TeamEvent::agent_restarted("eng-1-1", "42", "stalled", 1),
                TeamEvent::agent_restarted("eng-1-1", "42", "stalled", 2),
                TeamEvent::agent_restarted("eng-1-1", "99", "stalled", 1),
            ],
        );

        let daemon = make_test_daemon(tmp.path(), vec![]);
        assert_eq!(daemon.stall_restart_count(42).unwrap(), 2);
        assert_eq!(daemon.stall_restart_count(99).unwrap(), 1);
        assert_eq!(daemon.stall_restart_count(100).unwrap(), 0);
    }

    #[test]
    fn stall_detection_config_defaults() {
        let policy = WorkflowPolicy::default();
        assert_eq!(policy.stall_threshold_secs, 300);
        assert_eq!(policy.max_stall_restarts, 2);
    }

    #[test]
    fn stall_restart_cooldown_prevents_repeat_restart() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-stall-cd";
        let lead_name = "lead-stall-cd";

        let lead = MemberInstance {
            name: lead_name.to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: member_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some(lead_name.to_string()),
            use_worktrees: false,
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![lead, engineer]);
        daemon.active_tasks.insert(member_name.to_string(), 77);
        daemon
            .config
            .pane_map
            .insert(member_name.to_string(), "%999".to_string());
        write_owned_task_file(tmp.path(), 77, "cooldown-task", "in-progress", member_name);

        daemon
            .intervention_cooldowns
            .insert(format!("stall-restart::{member_name}"), Instant::now());

        daemon.handle_stalled_agent(member_name, 300).unwrap();

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap_or_default();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.event == "stall_detected" || e.event == "agent_restarted")
                .count(),
            0,
            "cooldown should suppress all stall handling"
        );
    }

    #[test]
    fn handle_stalled_agent_no_task_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![engineer_member("eng-1", Some("manager"), false)])
            .build();
        let result = daemon.handle_stalled_agent("eng-1", 600);
        assert!(result.is_ok());
        let events_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        let events = crate::team::events::read_events(&events_path).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn handle_stalled_agent_cooldown_prevents_action() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-1";
        write_owned_task_file(tmp.path(), 42, "test-task", "in-progress", member_name);
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, member_name).unwrap();

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![engineer_member(member_name, Some("manager"), false)])
            .build();
        daemon.active_tasks.insert(member_name.to_string(), 42);
        daemon
            .intervention_cooldowns
            .insert(format!("stall-restart::{member_name}"), Instant::now());

        let result = daemon.handle_stalled_agent(member_name, 600);
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn stall_restart_relaunches_stalled_agent_with_task_context() {
        let session = format!("batty-test-stall-restart-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-stall";
        let lead_name = "lead-stall";
        let (fake_bin, fake_log) = setup_fake_claude(&tmp, member_name);
        let worktree_path = tmp.path().join("worktree");
        std::fs::create_dir_all(&worktree_path).unwrap();

        crate::tmux::create_session(&session, "bash", &[], tmp.path().to_str().unwrap()).unwrap();
        crate::tmux::create_window(
            &session,
            "keeper",
            "sleep",
            &["30".to_string()],
            tmp.path().to_str().unwrap(),
        )
        .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        Command::new("tmux")
            .args(["set-option", "-p", "-t", &pane_id, "remain-on-exit", "on"])
            .output()
            .unwrap();

        let lead = MemberInstance {
            name: lead_name.to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: member_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some(lead_name.to_string()),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                agent: None,
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                external_senders: Vec::new(),
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                grafana: Default::default(),
                use_shim: false,
                use_sdk_mode: false,
                auto_respawn_on_crash: false,
                shim_health_check_interval_secs: 60,
                shim_health_timeout_secs: 120,
                shim_shutdown_timeout_secs: 30,
                shim_working_state_timeout_secs: 1800,
                pending_queue_max_age_secs: 600,
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.clone(),
            members: vec![lead, engineer],
            pane_map: HashMap::from([(member_name.to_string(), pane_id.clone())]),
        })
        .unwrap();
        daemon
            .states
            .insert(member_name.to_string(), MemberState::Working);
        daemon.active_tasks.insert(member_name.to_string(), 42);

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, member_name).unwrap();
        inbox::init_inbox(&root, lead_name).unwrap();
        write_owned_task_file_with_context(
            tmp.path(),
            42,
            "stall-test-task",
            "in-progress",
            member_name,
            "eng-stall/42",
            &worktree_path.display().to_string(),
        );

        daemon.handle_stalled_agent(member_name, 300).unwrap();

        let log = (0..100)
            .find_map(|_| {
                let content = match std::fs::read_to_string(&fake_log) {
                    Ok(content) => content,
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(100));
                        return None;
                    }
                };
                if content.contains("Continuing Task #42") {
                    Some(content)
                } else {
                    std::thread::sleep(Duration::from_millis(100));
                    None
                }
            })
            .unwrap_or_else(|| {
                panic!(
                    "fake claude log was not written by restarted member at {}",
                    fake_log.display()
                )
            });
        assert!(log.contains("stall-test-task"));
        assert!(log.contains("Branch: eng-stall/42"));
        assert!(log.contains(&format!("Worktree: {}", worktree_path.display())));

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();

        let stall_events: Vec<_> = events
            .iter()
            .filter(|e| e.event == "stall_detected")
            .collect();
        assert_eq!(stall_events.len(), 1);
        assert_eq!(stall_events[0].role.as_deref(), Some(member_name));
        assert_eq!(stall_events[0].task.as_deref(), Some("42"));
        assert_eq!(stall_events[0].uptime_secs, Some(300));

        let restart_events: Vec<_> = events
            .iter()
            .filter(|e| e.event == "agent_restarted")
            .collect();
        assert_eq!(restart_events.len(), 1);
        assert_eq!(restart_events[0].role.as_deref(), Some(member_name));
        assert_eq!(restart_events[0].task.as_deref(), Some("42"));
        assert_eq!(restart_events[0].reason.as_deref(), Some("stalled"));
        assert_eq!(restart_events[0].restart_count, Some(1));

        assert!(events.iter().any(|e| {
            e.event == "message_routed"
                && e.from.as_deref() == Some("daemon")
                && e.to.as_deref() == Some(member_name)
        }));

        crate::tmux::kill_session(&session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
    }

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn stall_escalates_after_max_restarts() {
        let session = format!("batty-test-stall-escalate-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-stall-esc";
        let lead_name = "lead-stall-esc";
        let (fake_bin, fake_log) = setup_fake_claude(&tmp, member_name);

        crate::tmux::create_session(&session, "bash", &[], tmp.path().to_str().unwrap()).unwrap();
        crate::tmux::create_window(
            &session,
            "keeper",
            "sleep",
            &["30".to_string()],
            tmp.path().to_str().unwrap(),
        )
        .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();

        let lead = MemberInstance {
            name: lead_name.to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: member_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some(lead_name.to_string()),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                agent: None,
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy {
                    max_stall_restarts: 2,
                    ..WorkflowPolicy::default()
                },
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                external_senders: Vec::new(),
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                grafana: Default::default(),
                use_shim: false,
                use_sdk_mode: false,
                auto_respawn_on_crash: false,
                shim_health_check_interval_secs: 60,
                shim_health_timeout_secs: 120,
                shim_shutdown_timeout_secs: 30,
                shim_working_state_timeout_secs: 1800,
                pending_queue_max_age_secs: 600,
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.clone(),
            members: vec![lead, engineer],
            pane_map: HashMap::from([(member_name.to_string(), pane_id)]),
        })
        .unwrap();
        daemon
            .states
            .insert(member_name.to_string(), MemberState::Working);
        daemon.active_tasks.insert(member_name.to_string(), 50);

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, lead_name).unwrap();
        write_owned_task_file(
            tmp.path(),
            50,
            "stall-escalate-task",
            "in-progress",
            member_name,
        );

        write_event_log(
            tmp.path(),
            &[
                TeamEvent::agent_restarted(member_name, "50", "stalled", 1),
                TeamEvent::agent_restarted(member_name, "50", "stalled", 2),
            ],
        );

        daemon.handle_stalled_agent(member_name, 600).unwrap();

        let pending = inbox::pending_messages(&root, lead_name).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].body.contains("Task #50"));
        assert!(pending[0].body.contains("stalled"));
        assert!(pending[0].body.contains("will not restart again"));

        let log = std::fs::read_to_string(&fake_log).unwrap_or_default();
        assert!(!log.contains("Continuing Task #50"));

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.event == "agent_restarted")
                .count(),
            2
        );
        assert!(events.iter().any(|e| {
            e.event == "task_escalated"
                && e.role.as_deref() == Some(member_name)
                && e.reason.as_deref() == Some("stalled")
        }));
        assert!(events.iter().any(|e| e.event == "stall_detected"));

        crate::tmux::kill_session(&session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
    }

    // Checkpoint-related stall tests are in separate files; they cover gather/write/read_checkpoint
    // which are tested via the context_exhaustion and stall flows.

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn stall_checkpoint_restart_resume_full_flow() {
        use crate::team::test_support::init_git_repo;

        let session = format!("batty-test-stall-cp-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-stall-cp");

        let member_name = "eng-stall-cp";
        let lead_name = "lead-stall-cp";
        let (fake_bin, _fake_log) = setup_fake_claude(&tmp, member_name);

        let worktree_dir = repo.join(".batty").join("worktrees").join(member_name);
        let team_config_dir = repo.join(".batty").join("team_config");
        crate::team::task_loop::setup_engineer_worktree(
            &repo,
            &worktree_dir,
            member_name,
            &team_config_dir,
        )
        .unwrap();

        // Create a task branch with some work
        let task_branch = format!("{member_name}/42");
        crate::team::test_support::git_ok(&worktree_dir, &["checkout", "-b", &task_branch]);
        std::fs::write(worktree_dir.join("work.rs"), "fn main() {}\n").unwrap();
        crate::team::test_support::git_ok(&worktree_dir, &["add", "work.rs"]);
        crate::team::test_support::git_ok(&worktree_dir, &["commit", "-m", "partial impl"]);

        let inbox_root = inbox::inboxes_root(&repo);
        inbox::init_inbox(&inbox_root, member_name).unwrap();
        inbox::init_inbox(&inbox_root, lead_name).unwrap();

        write_owned_task_file_with_context(
            &repo,
            42,
            "stall-cp-task",
            "in-progress",
            member_name,
            &task_branch,
            &worktree_dir.display().to_string(),
        );

        crate::tmux::create_session(&session, "bash", &[], repo.to_string_lossy().as_ref())
            .unwrap();
        crate::tmux::create_window(
            &session,
            "keeper",
            "sleep",
            &["60".to_string()],
            repo.to_string_lossy().as_ref(),
        )
        .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        Command::new("tmux")
            .args(["set-option", "-p", "-t", &pane_id, "remain-on-exit", "on"])
            .output()
            .unwrap();

        let lead = MemberInstance {
            name: lead_name.to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: member_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some(lead_name.to_string()),
            use_worktrees: true,
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: repo.clone(),
            team_config: test_team_config("stall-cp"),
            session: session.clone(),
            members: vec![lead, engineer],
            pane_map: HashMap::from([(member_name.to_string(), pane_id)]),
        })
        .unwrap();
        daemon
            .states
            .insert(member_name.to_string(), MemberState::Working);
        daemon.active_tasks.insert(member_name.to_string(), 42);

        daemon.handle_stalled_agent(member_name, 300).unwrap();

        // Checkpoint should have been written
        let cp_content = crate::team::checkpoint::read_checkpoint(&repo, member_name);
        assert!(
            cp_content.is_some(),
            "checkpoint should be written before stall restart"
        );
        let cp_text = cp_content.unwrap();
        assert!(cp_text.contains("**Task:** #42"));
        assert!(cp_text.contains(&task_branch));

        // Restart notice should contain checkpoint content
        let msgs = inbox::pending_messages(&inbox_root, member_name).unwrap();
        let restart_msg = msgs
            .iter()
            .find(|m| m.body.contains("Restarted after stall"));
        assert!(restart_msg.is_some(), "restart notice should be queued");
        let body = &restart_msg.unwrap().body;
        assert!(
            body.contains("[RESUMING FROM CHECKPOINT]"),
            "restart notice should include checkpoint"
        );
        assert!(body.contains("partial impl"));

        crate::tmux::kill_session(&session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
        let _ = Command::new("git")
            .current_dir(&repo)
            .args([
                "worktree",
                "remove",
                "--force",
                worktree_dir.to_str().unwrap(),
            ])
            .output();
    }

    #[test]
    fn stall_with_no_active_task_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-stall-no-task";
        let engineer = MemberInstance {
            name: member_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);
        daemon
            .config
            .pane_map
            .insert(member_name.to_string(), "%999".to_string());
        // active_tasks does NOT contain member — so active_task returns None.

        daemon.handle_stalled_agent(member_name, 300).unwrap();

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap_or_default();
        assert!(
            events.is_empty(),
            "stall with no active task should be a noop"
        );
    }

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn stall_overwrites_existing_checkpoint() {
        use crate::team::test_support::init_git_repo;

        let session = format!("batty-test-stall-overwrite-cp-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-stall-overwrite");

        let member_name = "eng-stall-ow";
        let lead_name = "lead-stall-ow";
        let (fake_bin, _fake_log) = setup_fake_claude(&tmp, member_name);

        let worktree_dir = repo.join(".batty").join("worktrees").join(member_name);
        let team_config_dir = repo.join(".batty").join("team_config");
        crate::team::task_loop::setup_engineer_worktree(
            &repo,
            &worktree_dir,
            member_name,
            &team_config_dir,
        )
        .unwrap();

        let task_branch = format!("{member_name}/55");
        crate::team::test_support::git_ok(&worktree_dir, &["checkout", "-b", &task_branch]);
        std::fs::write(worktree_dir.join("v1.rs"), "fn v1() {}\n").unwrap();
        crate::team::test_support::git_ok(&worktree_dir, &["add", "v1.rs"]);
        crate::team::test_support::git_ok(&worktree_dir, &["commit", "-m", "first version"]);

        let inbox_root = inbox::inboxes_root(&repo);
        inbox::init_inbox(&inbox_root, member_name).unwrap();
        inbox::init_inbox(&inbox_root, lead_name).unwrap();

        write_owned_task_file_with_context(
            &repo,
            55,
            "overwrite-cp-task",
            "in-progress",
            member_name,
            &task_branch,
            &worktree_dir.display().to_string(),
        );

        // Write an initial checkpoint that should get overwritten.
        let old_cp = crate::team::checkpoint::Checkpoint {
            role: member_name.to_string(),
            task_id: 55,
            task_title: "OLD TITLE".to_string(),
            task_description: "OLD DESC".to_string(),
            branch: Some("old-branch".to_string()),
            last_commit: Some("old-commit".to_string()),
            test_summary: None,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
        };
        crate::team::checkpoint::write_checkpoint(&repo, &old_cp).unwrap();

        crate::tmux::create_session(&session, "bash", &[], repo.to_string_lossy().as_ref())
            .unwrap();
        crate::tmux::create_window(
            &session,
            "keeper",
            "sleep",
            &["60".to_string()],
            repo.to_string_lossy().as_ref(),
        )
        .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        Command::new("tmux")
            .args(["set-option", "-p", "-t", &pane_id, "remain-on-exit", "on"])
            .output()
            .unwrap();

        let lead = MemberInstance {
            name: lead_name.to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: member_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some(lead_name.to_string()),
            use_worktrees: true,
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: repo.clone(),
            team_config: test_team_config("stall-ow"),
            session: session.clone(),
            members: vec![lead, engineer],
            pane_map: HashMap::from([(member_name.to_string(), pane_id)]),
        })
        .unwrap();
        daemon
            .states
            .insert(member_name.to_string(), MemberState::Working);
        daemon.active_tasks.insert(member_name.to_string(), 55);

        daemon.handle_stalled_agent(member_name, 300).unwrap();

        let new_cp = crate::team::checkpoint::read_checkpoint(&repo, member_name).unwrap();
        assert!(
            !new_cp.contains("OLD TITLE"),
            "old checkpoint should have been overwritten"
        );
        assert!(
            new_cp.contains("overwrite-cp-task"),
            "new checkpoint should contain current task title"
        );
        assert!(
            new_cp.contains(&task_branch),
            "new checkpoint should contain current branch"
        );

        crate::tmux::kill_session(&session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
        let _ = Command::new("git")
            .current_dir(&repo)
            .args([
                "worktree",
                "remove",
                "--force",
                worktree_dir.to_str().unwrap(),
            ])
            .output();
    }

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn stall_checkpoint_with_missing_worktree() {
        let session = format!("batty-test-stall-no-wt-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-stall-no-wt";
        let lead_name = "lead-stall-no-wt";
        let (fake_bin, _fake_log) = setup_fake_claude(&tmp, member_name);

        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, member_name).unwrap();
        inbox::init_inbox(&inbox_root, lead_name).unwrap();

        write_owned_task_file(tmp.path(), 66, "no-wt-task", "in-progress", member_name);

        crate::tmux::create_session(&session, "bash", &[], tmp.path().to_str().unwrap()).unwrap();
        crate::tmux::create_window(
            &session,
            "keeper",
            "sleep",
            &["60".to_string()],
            tmp.path().to_str().unwrap(),
        )
        .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        Command::new("tmux")
            .args(["set-option", "-p", "-t", &pane_id, "remain-on-exit", "on"])
            .output()
            .unwrap();

        let lead = MemberInstance {
            name: lead_name.to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: member_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some(lead_name.to_string()),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: test_team_config("stall-no-wt"),
            session: session.clone(),
            members: vec![lead, engineer],
            pane_map: HashMap::from([(member_name.to_string(), pane_id)]),
        })
        .unwrap();
        daemon
            .states
            .insert(member_name.to_string(), MemberState::Working);
        daemon.active_tasks.insert(member_name.to_string(), 66);

        // Should not panic even without a valid worktree.
        daemon.handle_stalled_agent(member_name, 300).unwrap();

        // A checkpoint should still have been written (with None for branch/last_commit).
        let cp = crate::team::checkpoint::read_checkpoint(tmp.path(), member_name);
        assert!(
            cp.is_some(),
            "checkpoint should be written even without worktree"
        );

        crate::tmux::kill_session(&session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
    }

    #[test]
    fn stall_checkpoint_cleared_on_task_clear() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-stall-clear";
        let engineer = MemberInstance {
            name: member_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);
        daemon.active_tasks.insert(member_name.to_string(), 88);

        // Write a checkpoint
        let cp = crate::team::checkpoint::Checkpoint {
            role: member_name.to_string(),
            task_id: 88,
            task_title: "clear-test".to_string(),
            task_description: "desc".to_string(),
            branch: None,
            last_commit: None,
            test_summary: None,
            timestamp: "2026-03-22T00:00:00Z".to_string(),
        };
        crate::team::checkpoint::write_checkpoint(tmp.path(), &cp).unwrap();
        assert!(crate::team::checkpoint::read_checkpoint(tmp.path(), member_name).is_some());

        daemon.clear_active_task(member_name);

        // Checkpoint should be cleared along with the task.
        let cp_after = crate::team::checkpoint::read_checkpoint(tmp.path(), member_name);
        assert!(
            cp_after.is_none(),
            "checkpoint should be cleared when task is cleared"
        );
        assert!(!daemon.active_tasks.contains_key(member_name));
    }
}
