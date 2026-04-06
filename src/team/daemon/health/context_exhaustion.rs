//! Context exhaustion detection, restart, and escalation.

use std::time::Instant;

use anyhow::Result;
use tracing::{info, warn};

use super::super::*;
use super::{CONTEXT_RESTART_COOLDOWN, format_checkpoint_section};

impl TeamDaemon {
    pub(in super::super) fn handle_context_exhaustion(&mut self, member_name: &str) -> Result<()> {
        let Some(task) = self.active_task(member_name)? else {
            warn!(member = %member_name, "context exhausted but no active task is recorded");
            self.states
                .insert(member_name.to_string(), MemberState::Idle);
            return Ok(());
        };
        let Some(member) = self
            .config
            .members
            .iter()
            .find(|member| member.name == member_name)
            .cloned()
        else {
            return Ok(());
        };
        let Some(_pane_id) = self.config.pane_map.get(member_name).cloned() else {
            return Ok(());
        };
        let restart_cooldown_key = Self::context_restart_cooldown_key(member_name);
        let restart_on_cooldown = self
            .intervention_cooldowns
            .get(&restart_cooldown_key)
            .is_some_and(|last| last.elapsed() < CONTEXT_RESTART_COOLDOWN);
        let escalation_cooldown_key = Self::context_escalation_cooldown_key(member_name);
        let escalation_on_cooldown = self
            .intervention_cooldowns
            .get(&escalation_cooldown_key)
            .is_some_and(|last| last.elapsed() < CONTEXT_RESTART_COOLDOWN);

        let prior_restarts = self.context_restart_count(task.id)?;
        if prior_restarts >= 1 {
            if escalation_on_cooldown {
                info!(
                    member = %member_name,
                    task_id = task.id,
                    "context exhaustion escalation suppressed by cooldown"
                );
                return Ok(());
            }
            self.escalate_context_exhaustion(&member, &task, prior_restarts + 1)?;
            self.intervention_cooldowns
                .insert(escalation_cooldown_key, Instant::now());
            return Ok(());
        }

        if restart_on_cooldown {
            info!(
                member = %member_name,
                task_id = task.id,
                "context exhaustion restart suppressed by cooldown"
            );
            return Ok(());
        }

        warn!(
            member = %member_name,
            task_id = task.id,
            "context exhausted; restarting agent with task context"
        );
        self.restart_member_with_task_context(member_name, "context exhaustion")?;
        self.intervention_cooldowns
            .insert(restart_cooldown_key, Instant::now());
        self.record_agent_restarted(
            member_name,
            task.id.to_string(),
            "context_exhausted",
            prior_restarts + 1,
        );
        Ok(())
    }

    pub(super) fn handle_context_pressure_restart(&mut self, member_name: &str) -> Result<()> {
        self.restart_member_with_task_context(member_name, "context pressure")?;
        self.intervention_cooldowns.insert(
            Self::context_restart_cooldown_key(member_name),
            Instant::now(),
        );
        Ok(())
    }

    pub(crate) fn restart_member_with_task_context(
        &mut self,
        member_name: &str,
        reason: &str,
    ) -> Result<()> {
        let Some(task) = self.active_task(member_name)? else {
            warn!(
                member = %member_name,
                reason,
                "restart requested but no active task is recorded"
            );
            return Ok(());
        };
        let Some(member) = self
            .config
            .members
            .iter()
            .find(|member| member.name == member_name)
            .cloned()
        else {
            return Ok(());
        };
        let Some(pane_id) = self.config.pane_map.get(member_name).cloned() else {
            return Ok(());
        };

        let work_dir = self.member_work_dir(&member);
        self.preserve_restart_context(member_name, &task, Some(&pane_id), &work_dir, reason);

        tmux::respawn_pane(&pane_id, "bash")?;
        std::thread::sleep(std::time::Duration::from_millis(200));

        let assignment = self.restart_assignment_with_handoff(member_name, &task, &work_dir);
        let launch = self.launch_task_assignment(member_name, &assignment, Some(task.id), false)?;
        let mut restart_notice = format!(
            "Restarted after {reason}. Continue task #{} from the current worktree state.",
            task.id
        );
        if let Some(branch) = launch.branch.as_deref() {
            restart_notice.push_str(&format!("\nBranch: {branch}"));
        }
        restart_notice.push_str(&format!("\nWorktree: {}", launch.work_dir.display()));
        if let Some(cp_content) =
            super::super::super::checkpoint::read_checkpoint(&self.config.project_root, member_name)
        {
            restart_notice.push_str(&format_checkpoint_section(&cp_content));
        }
        if let Err(error) = self.queue_message("daemon", member_name, &restart_notice) {
            warn!(member = %member_name, error = %error, "failed to inject restart notice");
        }
        self.record_orchestrator_action(format!(
            "restart: relaunched {} on task #{} after {}",
            member_name, task.id, reason
        ));
        if let Some(branch) = launch.branch.as_deref() {
            info!(member = %member_name, task_id = task.id, branch, reason, "context restart relaunched assignment");
        }
        Ok(())
    }

    pub(super) fn capture_context_handoff_output(&self, pane_id: &str) -> Option<String> {
        let screen_history = self
            .config
            .team_config
            .workflow_policy
            .handoff_screen_history
            .max(1);
        let rows = crate::tmux::pane_dimensions(pane_id)
            .map(|(_, rows)| rows as usize)
            .unwrap_or(50);
        let line_count = rows.saturating_mul(screen_history).min(u32::MAX as usize) as u32;
        crate::tmux::capture_pane_recent(pane_id, line_count).ok()
    }

    fn escalate_context_exhaustion(
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
                "context exhaustion exceeded restart limit with no escalation target"
            );
            return Ok(());
        };

        let body = format!(
            "Task #{task_id} for {member_name} exhausted context {restart_count} times. Batty restarted it once already and will not restart it again automatically.\n\
            Task: {title}\n\
            Next step: decide whether to split the task, redirect the engineer, or intervene directly in the lane.",
            task_id = task.id,
            member_name = member.name,
            title = task.title,
        );
        self.queue_message("daemon", manager, &body)?;
        self.record_orchestrator_action(format!(
            "restart: escalated context exhaustion for {} on task #{} after {} exhaustions",
            member.name, task.id, restart_count
        ));
        self.record_task_escalated(&member.name, task.id.to_string(), Some("context_exhausted"));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::*;
    use super::super::test_helpers::test_team_config;
    use crate::team::config::RoleType;
    use crate::team::events::TeamEvent;
    use crate::team::hierarchy::MemberInstance;
    use crate::team::standup::MemberState;
    use crate::team::test_helpers::write_event_log;
    use crate::team::test_support::{
        TestDaemonBuilder, engineer_member, manager_member, setup_fake_claude,
        write_owned_task_file,
    };
    use serial_test::serial;
    use std::collections::HashMap;
    use std::path::Path;
    use std::process::Command;
    use std::time::{Duration, Instant};

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn agent_restart_relaunches_context_exhausted_member_with_task_context() {
        let session = format!("batty-test-agent-restart-context-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-ctx-restart";
        let lead_name = "manager-ctx";
        let (fake_bin, fake_log) = setup_fake_claude(&tmp, member_name);
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, lead_name).unwrap();
        inbox::init_inbox(&inbox_root, member_name).unwrap();

        write_owned_task_file(tmp.path(), 42, "test-task", "in-progress", member_name);

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

        let member = MemberInstance {
            name: member_name.to_string(),
            role_name: "engineer".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some(lead_name.to_string()),
            use_worktrees: false,
            ..Default::default()
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: test_team_config("ctx-restart"),
            session: session.clone(),
            members: vec![member],
            pane_map: HashMap::from([(member_name.to_string(), pane_id.clone())]),
        })
        .unwrap();
        daemon.active_tasks.insert(member_name.to_string(), 42);

        daemon.handle_context_exhaustion(member_name).unwrap();

        let log = (0..100)
            .find_map(|_| {
                let content = match std::fs::read_to_string(&fake_log) {
                    Ok(content) => content,
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(100));
                        return None;
                    }
                };
                if content.contains("--append-system-prompt") {
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
        assert!(log.contains("--append-system-prompt"));

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        assert!(events.iter().any(|event| event.event == "agent_restarted"
            && event.task.as_deref() == Some("42")
            && event.reason.as_deref() == Some("context_exhausted")));

        let restart_msg =
            inbox::pending_messages(&inbox::inboxes_root(tmp.path()), member_name).unwrap();
        assert!(
            restart_msg
                .iter()
                .any(|msg| msg.body.contains("context exhaustion")),
            "restart notice should be sent to the restarted member"
        );

        crate::tmux::kill_session(&session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
    }

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn context_exhaustion_relaunch_corrects_mismatched_cwd() {
        let session = format!("batty-test-ctx-cwd-correct-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        let wrong_dir = tmp.path().join("wrong");
        std::fs::create_dir_all(&wrong_dir).unwrap();

        let member_name = "eng-ctx-cwd";
        let lead_name = "manager-ctx-cwd";
        let (fake_bin, _fake_log) = setup_fake_claude(&tmp, member_name);
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, lead_name).unwrap();
        inbox::init_inbox(&inbox_root, member_name).unwrap();

        write_owned_task_file(tmp.path(), 42, "test-task", "in-progress", member_name);

        crate::tmux::create_session(&session, "bash", &[], wrong_dir.to_string_lossy().as_ref())
            .unwrap();
        crate::tmux::create_window(
            &session,
            "keeper",
            "sleep",
            &["60".to_string()],
            wrong_dir.to_string_lossy().as_ref(),
        )
        .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        Command::new("tmux")
            .args(["set-option", "-p", "-t", &pane_id, "remain-on-exit", "on"])
            .output()
            .unwrap();

        let member = MemberInstance {
            name: member_name.to_string(),
            role_name: "engineer".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some(lead_name.to_string()),
            use_worktrees: false,
            ..Default::default()
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: test_team_config("ctx-cwd"),
            session: session.clone(),
            members: vec![member],
            pane_map: HashMap::from([(member_name.to_string(), pane_id.clone())]),
        })
        .unwrap();
        daemon.active_tasks.insert(member_name.to_string(), 42);

        daemon.handle_context_exhaustion(member_name).unwrap();

        let expected = normalized_assignment_dir(tmp.path());
        let cwd_ok = (0..30).any(|_| {
            std::thread::sleep(Duration::from_millis(200));
            crate::tmux::pane_current_path(&pane_id)
                .map(|p| normalized_assignment_dir(Path::new(&p)) == expected)
                .unwrap_or(false)
        });
        assert!(
            cwd_ok,
            "context restart should correct pane cwd to project root"
        );

        crate::tmux::kill_session(&session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
    }

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn agent_restart_second_exhaustion_escalates_instead_of_restarting() {
        let session = format!("batty-test-agent-restart-escalate-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-ctx-escalate";
        let lead_name = "manager-ctx-escalate";
        let (_fake_bin, _fake_log) = setup_fake_claude(&tmp, member_name);
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, lead_name).unwrap();
        inbox::init_inbox(&inbox_root, member_name).unwrap();

        write_owned_task_file(tmp.path(), 42, "test-task", "in-progress", member_name);

        // Write a prior restart event so this counts as the second exhaustion.
        write_event_log(
            tmp.path(),
            &[TeamEvent::agent_restarted(
                member_name,
                "42",
                "context_exhausted",
                1,
            )],
        );

        crate::tmux::create_session(&session, "bash", &[], tmp.path().to_str().unwrap()).unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();

        let member = MemberInstance {
            name: member_name.to_string(),
            role_name: "engineer".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some(lead_name.to_string()),
            use_worktrees: false,
            ..Default::default()
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: test_team_config("ctx-escalate"),
            session: session.clone(),
            members: vec![
                MemberInstance {
                    name: lead_name.to_string(),
                    role_name: "manager".to_string(),
                    role_type: RoleType::Manager,
                    agent: None,
                    prompt: None,
                    reports_to: None,
                    use_worktrees: false,
                    ..Default::default()
                },
                member,
            ],
            pane_map: HashMap::from([(member_name.to_string(), pane_id)]),
        })
        .unwrap();
        daemon.active_tasks.insert(member_name.to_string(), 42);

        daemon.handle_context_exhaustion(member_name).unwrap();

        let manager_messages =
            inbox::pending_messages(&inbox::inboxes_root(tmp.path()), lead_name).unwrap();
        assert!(
            manager_messages
                .iter()
                .any(|msg| msg.body.contains("exhausted context")),
            "escalation message should be sent to manager"
        );

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        assert!(
            events
                .iter()
                .any(|event| event.event == "task_escalated"
                    && event.task.as_deref() == Some("42")),
            "task_escalated event should be emitted"
        );

        crate::tmux::kill_session(&session).unwrap();
    }

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn agent_restart_respects_cooldown_before_first_restart() {
        let session = format!("batty-test-agent-cooldown-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-ctx-cooldown";
        let lead_name = "manager-ctx-cooldown";
        let (_fake_bin, _fake_log) = setup_fake_claude(&tmp, member_name);
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, lead_name).unwrap();
        inbox::init_inbox(&inbox_root, member_name).unwrap();

        write_owned_task_file(tmp.path(), 42, "test-task", "in-progress", member_name);

        crate::tmux::create_session(&session, "bash", &[], tmp.path().to_str().unwrap()).unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();

        let member = MemberInstance {
            name: member_name.to_string(),
            role_name: "engineer".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some(lead_name.to_string()),
            use_worktrees: false,
            ..Default::default()
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: test_team_config("ctx-cooldown"),
            session: session.clone(),
            members: vec![member],
            pane_map: HashMap::from([(member_name.to_string(), pane_id)]),
        })
        .unwrap();
        daemon.active_tasks.insert(member_name.to_string(), 42);

        // Set the cooldown so restart is suppressed.
        daemon.intervention_cooldowns.insert(
            TeamDaemon::context_restart_cooldown_key(member_name),
            Instant::now(),
        );

        daemon.handle_context_exhaustion(member_name).unwrap();

        // No restart notice should be sent.
        let member_msgs =
            inbox::pending_messages(&inbox::inboxes_root(tmp.path()), member_name).unwrap();
        assert!(
            member_msgs.is_empty(),
            "cooldown should suppress the restart"
        );

        crate::tmux::kill_session(&session).unwrap();
    }

    #[test]
    fn handle_context_exhaustion_no_task_sets_idle() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![engineer_member("eng-1", Some("manager"), false)])
            .build();
        daemon
            .states
            .insert("eng-1".to_string(), MemberState::Working);
        // No active task set — active_tasks is empty.

        daemon.handle_context_exhaustion("eng-1").unwrap();

        assert_eq!(daemon.states.get("eng-1"), Some(&MemberState::Idle));
    }

    #[test]
    fn handle_context_exhaustion_unknown_member_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        // Calling with a member that doesn't exist should not panic.
        let result = daemon.handle_context_exhaustion("nonexistent");
        assert!(result.is_ok());
    }

    #[test]
    fn handle_context_exhaustion_escalation_cooldown_suppresses() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-1";
        let lead_name = "manager";
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, lead_name).unwrap();
        inbox::init_inbox(&inbox_root, member_name).unwrap();

        write_owned_task_file(tmp.path(), 42, "test-task", "in-progress", member_name);
        // Write a prior restart event so we'd normally escalate.
        write_event_log(
            tmp.path(),
            &[TeamEvent::agent_restarted(
                member_name,
                "42",
                "context_exhausted",
                1,
            )],
        );

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member(lead_name, Some("architect")),
                engineer_member(member_name, Some(lead_name), false),
            ])
            .build();
        daemon.active_tasks.insert(member_name.to_string(), 42);
        // Set the escalation cooldown so it suppresses the escalation.
        daemon.intervention_cooldowns.insert(
            TeamDaemon::context_escalation_cooldown_key(member_name),
            Instant::now(),
        );

        daemon.handle_context_exhaustion(member_name).unwrap();

        // No message should have been sent to the manager.
        let pending = inbox::pending_messages(&inbox_root, lead_name).unwrap();
        assert!(
            pending.is_empty(),
            "escalation should be suppressed by cooldown"
        );
    }
}
