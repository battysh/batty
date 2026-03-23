//! Dead member restart and pane death recovery.

use std::time::Duration;

use anyhow::Result;
use tracing::warn;

use super::super::*;

impl TeamDaemon {
    pub(in super::super) fn restart_dead_members(&mut self) -> Result<()> {
        let member_names: Vec<String> = self.config.pane_map.keys().cloned().collect();
        for name in member_names {
            let Some(pane_id) = self.config.pane_map.get(&name) else {
                continue;
            };
            if !tmux::pane_exists(pane_id) {
                continue;
            }
            if !tmux::pane_dead(pane_id).unwrap_or(false) {
                continue;
            }
            self.restart_member(&name)?;
        }
        Ok(())
    }

    pub(in super::super) fn restart_member(&mut self, member_name: &str) -> Result<()> {
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

        warn!(member = %member_name, pane = %pane_id, "detected dead pane, restarting member");
        tmux::respawn_pane(&pane_id, "bash")?;
        std::thread::sleep(Duration::from_millis(200));

        let previous_launch_state = load_launch_state(&self.config.project_root);
        let duplicate_claude_session_ids = duplicate_claude_session_ids(&previous_launch_state);
        let plan = self.prepare_member_launch(
            &member,
            true,
            &previous_launch_state,
            &duplicate_claude_session_ids,
        )?;
        self.apply_member_launch(&member, &pane_id, &plan)?;
        if let Err(error) = self.persist_member_launch_identity(&member.name, plan.identity.clone())
        {
            warn!(member = %member.name, error = %error, "failed to persist restarted launch identity");
        }
        self.record_orchestrator_action(format!(
            "restart: respawned pane and relaunched {} after pane death",
            member.name
        ));
        self.emit_event(TeamEvent::pane_respawned(&member.name));
        self.record_member_crashed(&member.name, true);
        Ok(())
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
    use crate::team::hierarchy::MemberInstance;
    use crate::team::standup::MemberState;
    use crate::team::test_support::setup_fake_claude;
    use serial_test::serial;
    use std::collections::HashMap;
    use std::path::Path;
    use std::process::Command;
    use std::time::Duration;

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn poll_watchers_respawns_pane_dead_member_and_records_events() {
        let session = format!("batty-test-restart-dead-member-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "architect-restart";
        let project_slug = tmp
            .path()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "default".to_string());
        let fake_bin = std::env::temp_dir().join(format!("batty-bin-{project_slug}-{member_name}"));
        let _ = std::fs::remove_dir_all(&fake_bin);
        std::fs::create_dir_all(&fake_bin).unwrap();
        let fake_log = tmp.path().join("fake-claude.log");
        let fake_claude = fake_bin.join("claude");
        std::fs::write(
            &fake_claude,
            format!(
                "#!/bin/bash\nprintf '%s\\n' \"$*\" >> '{}'\nsleep 5\n",
                fake_log.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

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

        let member = MemberInstance {
            name: member_name.to_string(),
            role_name: "architect".to_string(),
            role_type: RoleType::Architect,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
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
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.clone(),
            members: vec![member],
            pane_map: HashMap::from([(member_name.to_string(), pane_id.clone())]),
        })
        .unwrap();
        daemon
            .states
            .insert(member_name.to_string(), MemberState::Working);

        crate::tmux::send_keys(&pane_id, "exit", true).unwrap();
        for _ in 0..5 {
            if crate::tmux::pane_dead(&pane_id).unwrap_or(false) {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        assert!(crate::tmux::pane_dead(&pane_id).unwrap());

        daemon.poll_watchers().unwrap();
        std::thread::sleep(Duration::from_millis(700));

        assert!(!crate::tmux::pane_dead(&pane_id).unwrap_or(true));
        assert_eq!(daemon.states.get(member_name), Some(&MemberState::Idle));

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

        let launch_state = load_launch_state(tmp.path());
        let identity = launch_state
            .get(member_name)
            .expect("missing restarted member launch state");
        assert_eq!(identity.agent, "claude-code");
        assert!(identity.session_id.is_some());

        let events = std::fs::read_to_string(
            tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        assert!(events.contains("\"event\":\"pane_death\""));
        assert!(events.contains("\"event\":\"pane_respawned\""));
        assert!(events.contains("\"event\":\"member_crashed\""));
        assert!(events.contains(&format!("\"role\":\"{member_name}\"")));
        assert!(events.contains("\"restart\":true"));

        crate::tmux::kill_session(&session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
    }

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn spawn_all_agents_corrects_mismatched_cwd_before_launch() {
        let session = format!("batty-test-spawn-cwd-correct-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        let wrong_dir = tmp.path().join("wrong");
        std::fs::create_dir_all(&wrong_dir).unwrap();

        let member_name = "architect-cwd";
        let (fake_bin, fake_log) = setup_fake_claude(&tmp, member_name);

        crate::tmux::create_session(&session, "bash", &[], wrong_dir.to_string_lossy().as_ref())
            .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();

        let member = MemberInstance {
            name: member_name.to_string(),
            role_name: "architect".to_string(),
            role_type: RoleType::Architect,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
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
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.clone(),
            members: vec![member],
            pane_map: HashMap::from([(member_name.to_string(), pane_id.clone())]),
        })
        .unwrap();

        daemon.spawn_all_agents(false).unwrap();

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
                    "fake claude log was not written by spawned member at {}",
                    fake_log.display()
                )
            });
        assert!(log.contains("--append-system-prompt"));

        let current = (0..50)
            .find_map(|_| match crate::tmux::pane_current_path(&pane_id) {
                Ok(current) if !current.is_empty() => Some(current),
                _ => {
                    std::thread::sleep(Duration::from_millis(100));
                    None
                }
            })
            .unwrap_or_else(|| {
                panic!("tmux pane current path never became available for target '{pane_id}'")
            });
        assert_eq!(
            normalized_assignment_dir(Path::new(&current)),
            normalized_assignment_dir(tmp.path())
        );

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        let corrected = events
            .iter()
            .find(|event| event.event == "cwd_corrected")
            .expect("expected cwd_corrected event during spawn");
        assert_eq!(corrected.role.as_deref(), Some(member_name));
        assert_eq!(
            corrected.reason.as_deref(),
            Some(tmp.path().to_string_lossy().as_ref())
        );

        crate::tmux::kill_session(&session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
    }

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn restart_member_corrects_mismatched_cwd_after_respawn() {
        let session = format!("batty-test-restart-cwd-correct-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        let wrong_dir = tmp.path().join("wrong");
        std::fs::create_dir_all(&wrong_dir).unwrap();

        let member_name = "architect-restart-cwd";
        let (fake_bin, fake_log) = setup_fake_claude(&tmp, member_name);

        crate::tmux::create_session(&session, "bash", &[], wrong_dir.to_string_lossy().as_ref())
            .unwrap();
        crate::tmux::create_window(
            &session,
            "keeper",
            "sleep",
            &["30".to_string()],
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
            role_name: "architect".to_string(),
            role_type: RoleType::Architect,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
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
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.clone(),
            members: vec![member],
            pane_map: HashMap::from([(member_name.to_string(), pane_id.clone())]),
        })
        .unwrap();

        crate::tmux::send_keys(&pane_id, "exit", true).unwrap();
        for _ in 0..10 {
            if crate::tmux::pane_dead(&pane_id).unwrap_or(false) {
                break;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        assert!(crate::tmux::pane_dead(&pane_id).unwrap());

        daemon.restart_member(member_name).unwrap();

        let log = (0..40)
            .find_map(|_| {
                std::thread::sleep(Duration::from_millis(200));
                let content = std::fs::read_to_string(&fake_log).ok()?;
                if content.contains("--append-system-prompt") {
                    Some(content)
                } else {
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

        let expected = normalized_assignment_dir(tmp.path());
        let cwd_ok = (0..20).any(|_| {
            std::thread::sleep(Duration::from_millis(200));
            crate::tmux::pane_current_path(&pane_id)
                .map(|p| normalized_assignment_dir(Path::new(&p)) == expected)
                .unwrap_or(false)
        });
        assert!(cwd_ok, "pane cwd did not converge to expected dir");

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        assert!(events.iter().any(|event| {
            event.event == "pane_respawned" && event.role.as_deref() == Some(member_name)
        }));
        assert!(events.iter().any(|event| {
            event.event == "cwd_corrected"
                && event.role.as_deref() == Some(member_name)
                && event.reason.as_deref() == Some(tmp.path().to_string_lossy().as_ref())
        }));

        crate::tmux::kill_session(&session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
    }
}
