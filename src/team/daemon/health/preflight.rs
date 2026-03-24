//! Startup preflight checks: session readiness, pane liveness, board init.

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use super::super::helpers::{
    board_dir, ensure_board_initialized, ensure_kanban_available, ensure_tmux_session_ready,
};
use super::super::*;
use super::STARTUP_PREFLIGHT_RESPAWN_DELAY;

impl TeamDaemon {
    pub(in super::super) fn run_startup_preflight(&mut self) -> Result<()> {
        ensure_tmux_session_ready(&self.config.session)?;
        self.ensure_member_panes_ready()?;
        ensure_kanban_available()?;
        if ensure_board_initialized(&self.config.project_root)? {
            let board_dir = board_dir(&self.config.project_root);
            info!(
                board = %board_dir.display(),
                "initialized missing board during daemon startup preflight"
            );
            self.record_orchestrator_action(format!(
                "startup: initialized board at {}",
                board_dir.display()
            ));
        }
        self.validate_member_panes_on_startup();
        Ok(())
    }

    fn ensure_member_panes_ready(&mut self) -> Result<()> {
        let members = self.config.members.clone();
        for member in &members {
            if member.role_type == RoleType::User {
                continue;
            }

            let Some(pane_id) = self.config.pane_map.get(&member.name) else {
                bail!(
                    "daemon startup pre-flight failed: no tmux pane mapped for member '{}'",
                    member.name
                );
            };
            if !tmux::pane_exists(pane_id) {
                bail!(
                    "daemon startup pre-flight failed: pane '{}' for member '{}' is missing",
                    pane_id,
                    member.name
                );
            }

            if !tmux::pane_dead(pane_id)
                .with_context(|| format!("failed to inspect pane '{pane_id}'"))?
            {
                continue;
            }

            warn!(
                member = %member.name,
                pane = %pane_id,
                "respawning dead pane during daemon startup preflight"
            );
            tmux::respawn_pane(pane_id, "bash")
                .with_context(|| format!("failed to respawn pane '{pane_id}'"))?;
            std::thread::sleep(STARTUP_PREFLIGHT_RESPAWN_DELAY);

            if tmux::pane_dead(pane_id)
                .with_context(|| format!("failed to inspect respawned pane '{pane_id}'"))?
            {
                bail!(
                    "daemon startup pre-flight failed: pane '{}' for member '{}' stayed dead after respawn",
                    pane_id,
                    member.name
                );
            }

            self.record_orchestrator_action(format!(
                "startup: respawned dead pane for {}",
                member.name
            ));
            self.emit_event(TeamEvent::pane_respawned(&member.name));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::*;
    use super::super::helpers::{board_dir, ensure_board_initialized, ensure_kanban_available};
    use super::super::test_helpers::{EnvVarGuard, PATH_LOCK, setup_fake_kanban, test_team_config};
    use crate::team::config::RoleType;
    use crate::team::hierarchy::MemberInstance;
    use crate::team::test_support::{
        TestDaemonBuilder, architect_member, engineer_member, init_git_repo, setup_fake_claude,
    };
    use crate::team::watcher::SessionWatcher;
    use serial_test::serial;
    use std::collections::HashMap;
    use std::path::Path;
    use std::process::Command;
    use std::time::Duration;

    #[test]
    fn startup_preflight_reports_missing_kanban_binary() {
        let _path_guard = PATH_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let empty_bin = tmp.path().join("empty-bin");
        std::fs::create_dir_all(&empty_bin).unwrap();
        let _path = EnvVarGuard::set("PATH", empty_bin.to_string_lossy().as_ref());

        let error = ensure_kanban_available().unwrap_err();

        assert!(format!("{error:#}").contains("kanban-md"));
    }

    #[test]
    fn startup_preflight_initializes_missing_board_directory() {
        let _path_guard = PATH_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "startup_board_init");
        let fake_bin = setup_fake_kanban(&tmp, "startup-board-init");
        let original_path = std::env::var("PATH").unwrap_or_default();
        let _path = EnvVarGuard::set(
            "PATH",
            &format!("{}:{original_path}", fake_bin.to_string_lossy()),
        );

        let board_path = board_dir(&repo);
        assert!(!board_path.exists());

        assert!(ensure_board_initialized(&repo).unwrap());
        assert!(board_path.join("tasks").is_dir());
        assert!(!ensure_board_initialized(&repo).unwrap());
    }

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn startup_preflight_respawns_dead_pane_and_bootstraps_board() {
        let _path_guard = PATH_LOCK.lock().unwrap();
        let session = format!("batty-test-startup-preflight-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "startup_preflight");
        let fake_bin = setup_fake_kanban(&tmp, "startup-preflight");
        let original_path = std::env::var("PATH").unwrap_or_default();
        let _path = EnvVarGuard::set(
            "PATH",
            &format!("{}:{original_path}", fake_bin.to_string_lossy()),
        );

        crate::tmux::create_session(&session, "bash", &[], repo.to_string_lossy().as_ref())
            .unwrap();
        crate::tmux::create_window(
            &session,
            "eng-1",
            "bash",
            &[],
            repo.to_string_lossy().as_ref(),
        )
        .unwrap();

        let architect_pane = crate::tmux::pane_id(&session).unwrap();
        let engineer_pane = crate::tmux::pane_id(&format!("{session}:eng-1")).unwrap();
        Command::new("tmux")
            .args([
                "set-option",
                "-p",
                "-t",
                &engineer_pane,
                "remain-on-exit",
                "on",
            ])
            .output()
            .unwrap();
        crate::tmux::send_keys(&engineer_pane, "exit", true).unwrap();
        for _ in 0..20 {
            if crate::tmux::pane_dead(&engineer_pane).unwrap_or(false) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(crate::tmux::pane_dead(&engineer_pane).unwrap());

        let members = vec![
            architect_member("architect"),
            engineer_member("eng-1", Some("architect"), false),
        ];
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: repo.clone(),
            team_config: test_team_config("startup-preflight"),
            session: session.clone(),
            members,
            pane_map: HashMap::from([
                ("architect".to_string(), architect_pane),
                ("eng-1".to_string(), engineer_pane.clone()),
            ]),
        })
        .unwrap();

        daemon.run_startup_preflight().unwrap();

        assert!(!crate::tmux::pane_dead(&engineer_pane).unwrap_or(true));
        assert!(board_dir(&repo).join("tasks").is_dir());

        let events = crate::team::events::read_events(
            &repo.join(".batty").join("team_config").join("events.jsonl"),
        )
        .unwrap();
        assert!(events.iter().any(|event| {
            event.event == "pane_respawned" && event.role.as_deref() == Some("eng-1")
        }));

        crate::tmux::kill_session(&session).unwrap();
    }

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn startup_cwd_validation_corrects_all_agent_panes() {
        let session = format!("batty-test-startup-cwd-val-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        let wrong_dir = tmp.path().join("wrong");
        std::fs::create_dir_all(&wrong_dir).unwrap();

        let member_name = "architect-cwd-val";
        let (_fake_bin, _fake_log) = setup_fake_claude(&tmp, member_name);

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
            team_config: test_team_config("startup-cwd-val"),
            session: session.clone(),
            members: vec![member],
            pane_map: HashMap::from([(member_name.to_string(), pane_id.clone())]),
        })
        .unwrap();

        daemon.validate_member_panes_on_startup();

        let expected = normalized_assignment_dir(tmp.path());
        let cwd_ok = (0..30).any(|_| {
            std::thread::sleep(Duration::from_millis(200));
            crate::tmux::pane_current_path(&pane_id)
                .map(|p| normalized_assignment_dir(Path::new(&p)) == expected)
                .unwrap_or(false)
        });
        assert!(cwd_ok, "startup cwd validation should correct pane cwd");

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        assert!(
            events.iter().any(|event| event.event == "cwd_corrected"
                && event.role.as_deref() == Some(member_name))
        );

        crate::tmux::kill_session(&session).unwrap();
    }

    #[test]
    fn mark_member_working_updates_state_and_watcher() {
        let tmp = tempfile::tempdir().unwrap();
        let mut watchers = HashMap::new();
        watchers.insert(
            "architect".to_string(),
            SessionWatcher::new("%0", "architect", 300, None),
        );

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .watchers(watchers)
            .build();

        daemon.mark_member_working("architect");

        assert_eq!(
            daemon.states.get("architect"),
            Some(&crate::team::standup::MemberState::Working)
        );
        assert_eq!(
            daemon
                .watchers
                .get("architect")
                .map(|watcher| watcher.state),
            Some(crate::team::watcher::WatcherState::Active)
        );
    }

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn pre_assignment_health_check_corrects_mismatched_cwd() {
        use crate::team::config::{
            AutomationConfig, BoardConfig, OrchestratorPosition, StandupConfig, TeamConfig,
            WorkflowMode, WorkflowPolicy,
        };

        let session = format!("batty-test-health-check-cwd-correct-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        let wrong_dir = tmp.path().join("wrong");
        let expected_dir = tmp.path().join("expected");
        std::fs::create_dir_all(&wrong_dir).unwrap();
        std::fs::create_dir_all(&expected_dir).unwrap();

        crate::tmux::create_session(&session, "bash", &[], wrong_dir.to_string_lossy().as_ref())
            .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        std::thread::sleep(Duration::from_millis(200));

        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: true,
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
            members: vec![engineer],
            pane_map: HashMap::from([("eng-1".to_string(), pane_id.clone())]),
        })
        .unwrap();

        daemon
            .ensure_member_pane_cwd("eng-1", &pane_id, &expected_dir)
            .unwrap();

        let current = crate::tmux::pane_current_path(&pane_id).unwrap();
        assert_eq!(
            normalized_assignment_dir(Path::new(&current)),
            normalized_assignment_dir(&expected_dir)
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
            .expect("expected cwd_corrected event");
        assert_eq!(corrected.role.as_deref(), Some("eng-1"));
        assert_eq!(
            corrected.reason.as_deref(),
            Some(expected_dir.to_string_lossy().as_ref())
        );

        crate::tmux::kill_session(&session).unwrap();
    }

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn pre_assignment_health_check_cwd_matching_path_passes_silently() {
        use crate::team::config::{
            AutomationConfig, BoardConfig, OrchestratorPosition, StandupConfig, TeamConfig,
            WorkflowMode, WorkflowPolicy,
        };

        let session = format!("batty-test-health-check-cwd-match-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        let expected_dir = tmp.path().join("expected");
        std::fs::create_dir_all(&expected_dir).unwrap();

        crate::tmux::create_session(
            &session,
            "bash",
            &[],
            expected_dir.to_string_lossy().as_ref(),
        )
        .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        std::thread::sleep(Duration::from_millis(200));

        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: true,
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
            members: vec![engineer],
            pane_map: HashMap::from([("eng-1".to_string(), pane_id.clone())]),
        })
        .unwrap();

        daemon
            .ensure_member_pane_cwd("eng-1", &pane_id, &expected_dir)
            .unwrap();

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        assert!(
            events.iter().all(|event| event.event != "cwd_corrected"),
            "did not expect cwd_corrected event when pane cwd already matched"
        );

        crate::tmux::kill_session(&session).unwrap();
    }
}
