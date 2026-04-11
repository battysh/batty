//! Startup preflight checks: session readiness, pane liveness, board init.

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use super::super::helpers::{
    board_dir, ensure_agent_binaries_available, ensure_board_initialized, ensure_git_ready,
    ensure_kanban_available, ensure_telemetry_writable, ensure_tmux_session_ready,
    ensure_worktree_operations,
};
use super::super::*;
use super::STARTUP_PREFLIGHT_RESPAWN_DELAY;

impl TeamDaemon {
    pub(in super::super) fn run_startup_preflight(&mut self) -> Result<()> {
        ensure_tmux_session_ready(&self.config.session)?;
        ensure_git_ready(&self.config.project_root)?;
        ensure_worktree_operations(&self.config.project_root)?;
        ensure_telemetry_writable(&self.config.project_root)?;
        ensure_agent_binaries_available(&self.config.members)?;
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
        self.repair_malformed_board_frontmatter()?;
        self.validate_member_panes_on_startup();
        Ok(())
    }

    pub(in super::super) fn repair_malformed_board_frontmatter(&mut self) -> Result<()> {
        let board_dir = board_dir(&self.config.project_root);
        for repair in crate::team::task_cmd::repair_board_frontmatter_compat(&board_dir)? {
            info!(
                task_id = ?repair.task_id,
                status = repair.status.as_deref().unwrap_or("unknown"),
                path = %repair.path.display(),
                "repaired malformed task frontmatter during daemon startup preflight"
            );
            let task_label = repair
                .task_id
                .map(|task_id| format!("#{task_id}"))
                .unwrap_or_else(|| repair.path.display().to_string());
            let reason_suffix = repair
                .reason
                .as_deref()
                .map(|reason| format!(" ({reason})"))
                .unwrap_or_default();
            self.record_orchestrator_action(format!(
                "startup: repaired malformed task frontmatter for {task_label}{reason_suffix}"
            ));
            self.record_state_reconciliation(None, repair.task_id, "task_frontmatter_repair");
        }
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
    use super::super::helpers::{
        board_dir, ensure_agent_binaries_available, ensure_board_initialized, ensure_git_ready,
        ensure_kanban_available, ensure_telemetry_writable, ensure_worktree_operations,
    };
    use super::super::test_helpers::{EnvVarGuard, PATH_LOCK, setup_fake_kanban, test_team_config};
    use crate::team::config::{
        AutomationConfig, BoardConfig, OrchestratorPosition, RoleType, StandupConfig, TeamConfig,
        WorkflowMode, WorkflowPolicy,
    };
    use crate::team::hierarchy::MemberInstance;
    use crate::team::test_support::{
        TestDaemonBuilder, architect_member, engineer_member, init_git_repo, manager_member,
        setup_fake_backend, setup_fake_claude,
    };
    use crate::team::watcher::SessionWatcher;
    use serial_test::serial;
    use std::collections::HashMap;
    use std::path::Path;
    use std::process::Command;
    use std::time::Duration;

    #[test]
    fn startup_preflight_reports_missing_kanban_binary() {
        let _path_guard = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let empty_bin = tmp.path().join("empty-bin");
        std::fs::create_dir_all(&empty_bin).unwrap();
        let _path = EnvVarGuard::set("PATH", empty_bin.to_string_lossy().as_ref());

        let error = ensure_kanban_available().unwrap_err();

        assert!(format!("{error:#}").contains("kanban-md"));
    }

    #[test]
    fn startup_preflight_initializes_missing_board_directory() {
        let _path_guard = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
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
    fn startup_preflight_repairs_malformed_hidden_task_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "startup_board_repair");
        let task_path = board_dir(&repo).join("tasks").join("041-hidden-active.md");
        std::fs::create_dir_all(task_path.parent().unwrap()).unwrap();
        std::fs::write(
            &task_path,
            "---\nid: 41\ntitle: Hidden active task\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\nblocked: waiting on reviewer\nclass: standard\n---\n",
        )
        .unwrap();

        let mut daemon = TestDaemonBuilder::new(&repo).build();

        daemon.repair_malformed_board_frontmatter().unwrap();

        let content = std::fs::read_to_string(&task_path).unwrap();
        assert!(content.contains("blocked: true"));
        assert!(content.contains("block_reason: waiting on reviewer"));
        assert!(content.contains("blocked_on: waiting on reviewer"));

        let events_path = repo.join(".batty").join("team_config").join("events.jsonl");
        let events = std::fs::read_to_string(events_path).unwrap_or_default();
        assert!(events.contains("\"event\":\"state_reconciliation\""));
        assert!(events.contains("\"reason\":\"task_frontmatter_repair\""));
        assert!(events.contains("\"task\":\"41\""));
    }

    #[test]
    fn startup_preflight_repairs_legacy_timestamp_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "startup_board_timestamp_repair");
        let task_path = board_dir(&repo).join("tasks").join("623-stale-review.md");
        std::fs::create_dir_all(task_path.parent().unwrap()).unwrap();
        std::fs::write(
            &task_path,
            "---\nid: 623\ntitle: Stale review\nstatus: review\npriority: high\ncreated: 2026-04-10T16:31:02.743151-04:00\nupdated: 2026-04-10T19:26:40-0400\nreview_owner: manager\nclass: standard\n---\n\nTask body.\n",
        )
        .unwrap();

        let mut daemon = TestDaemonBuilder::new(&repo).build();

        daemon.repair_malformed_board_frontmatter().unwrap();

        let content = std::fs::read_to_string(&task_path).unwrap();
        assert!(content.contains("updated: 2026-04-10T19:26:40-04:00"));
        assert!(content.ends_with("\n\nTask body.\n"));

        let events_path = repo.join(".batty").join("team_config").join("events.jsonl");
        let events = std::fs::read_to_string(events_path).unwrap_or_default();
        assert!(events.contains("\"event\":\"state_reconciliation\""));
        assert!(events.contains("\"reason\":\"task_frontmatter_repair\""));
        assert!(events.contains("\"task\":\"623\""));
    }

    #[test]
    fn startup_preflight_reports_missing_git_identity() {
        let _path_guard = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "startup_git_identity");
        Command::new("git")
            .args(["config", "--unset", "user.email"])
            .current_dir(&repo)
            .status()
            .unwrap();
        let isolated_home = tmp.path().join("isolated-home");
        let isolated_config = tmp.path().join("isolated-xdg");
        let isolated_global = tmp.path().join("isolated-gitconfig");
        std::fs::create_dir_all(&isolated_home).unwrap();
        std::fs::create_dir_all(&isolated_config).unwrap();
        std::fs::write(&isolated_global, "").unwrap();
        let _home = EnvVarGuard::set("HOME", isolated_home.to_string_lossy().as_ref());
        let _xdg = EnvVarGuard::set(
            "XDG_CONFIG_HOME",
            isolated_config.to_string_lossy().as_ref(),
        );
        let _git_global = EnvVarGuard::set(
            "GIT_CONFIG_GLOBAL",
            isolated_global.to_string_lossy().as_ref(),
        );
        let _git_system = EnvVarGuard::set("GIT_CONFIG_NOSYSTEM", "1");

        let error = ensure_git_ready(&repo).unwrap_err();

        assert!(format!("{error:#}").contains("user.email"));
    }

    #[test]
    fn startup_preflight_verifies_worktree_operations() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "startup_worktree_probe");

        ensure_worktree_operations(&repo).unwrap();
    }

    #[test]
    fn startup_preflight_verifies_telemetry_db_writable() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "startup_telemetry_probe");

        ensure_telemetry_writable(&repo).unwrap();
        assert!(repo.join(".batty").join("telemetry.db").exists());
    }

    #[test]
    fn startup_preflight_reports_missing_agent_binary() {
        let _path_guard = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let empty_bin = tmp.path().join("empty-bin");
        std::fs::create_dir_all(&empty_bin).unwrap();
        let _path = EnvVarGuard::set("PATH", empty_bin.to_string_lossy().as_ref());

        let members = vec![engineer_member("eng-1", Some("manager"), false)];
        let error = ensure_agent_binaries_available(&members).unwrap_err();

        let rendered = format!("{error:#}");
        assert!(rendered.contains("eng-1"));
        assert!(rendered.contains("unreachable"));
    }

    #[test]
    fn startup_preflight_accepts_available_agent_binaries() {
        let _path_guard = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let (fake_claude_bin, _fake_claude_log) = setup_fake_claude(&tmp, "eng-1");
        let (fake_codex_bin, _fake_codex_log) =
            setup_fake_backend(&tmp, "codex", "eng-1-fake-codex.log");
        let original_path = std::env::var("PATH").unwrap_or_default();
        let _path = EnvVarGuard::set(
            "PATH",
            &format!(
                "{}:{}:{original_path}",
                fake_claude_bin.to_string_lossy(),
                fake_codex_bin.to_string_lossy()
            ),
        );

        let mut codex_member = engineer_member("eng-1", Some("manager"), false);
        codex_member.agent = Some("codex".to_string());
        let members = vec![architect_member("architect"), codex_member];

        ensure_agent_binaries_available(&members).unwrap();
    }

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn startup_preflight_respawns_dead_pane_and_bootstraps_board() {
        let _path_guard = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
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
    fn startup_preflight_missing_sessions_recover_failed_member_without_respawning_healthy_panes() {
        let _path_guard = PATH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let session = format!("batty-test-startup-missing-session-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "startup_missing_sessions");
        let fake_kanban = setup_fake_kanban(&tmp, "startup-missing-session");
        let (fake_claude_bin, _fake_claude_log) = setup_fake_claude(&tmp, "supervisors");
        let (fake_codex_bin, _fake_codex_log) =
            setup_fake_backend(&tmp, "codex", "eng-1-fake-codex.log");
        let original_path = std::env::var("PATH").unwrap_or_default();
        let _path = EnvVarGuard::set(
            "PATH",
            &format!(
                "{}:{}:{}:{original_path}",
                fake_kanban.to_string_lossy(),
                fake_claude_bin.to_string_lossy(),
                fake_codex_bin.to_string_lossy(),
            ),
        );

        crate::tmux::create_session(&session, "bash", &[], repo.to_string_lossy().as_ref())
            .unwrap();
        crate::tmux::create_window(
            &session,
            "manager",
            "bash",
            &[],
            repo.to_string_lossy().as_ref(),
        )
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
        let manager_pane = crate::tmux::pane_id(&format!("{session}:manager")).unwrap();
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
        assert!(!crate::tmux::pane_dead(&architect_pane).unwrap_or(true));
        assert!(!crate::tmux::pane_dead(&manager_pane).unwrap_or(true));

        std::fs::write(
            repo.join(".batty").join("launch-state.json"),
            serde_json::json!({
                "architect": {
                    "agent": "claude-code",
                    "prompt": "",
                    "session_id": "missing-architect-session"
                },
                "manager": {
                    "agent": "claude-code",
                    "prompt": "",
                    "session_id": "missing-manager-session"
                },
                "eng-1": {
                    "agent": "codex-cli",
                    "prompt": "",
                    "session_id": "missing-engineer-session"
                }
            })
            .to_string(),
        )
        .unwrap();

        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: repo.clone(),
            team_config: TeamConfig {
                name: "test".to_string(),
                agent: None,
                workflow_mode: WorkflowMode::Hybrid,
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
            members: vec![
                architect_member("architect"),
                manager_member("manager", Some("architect")),
                engineer_member("eng-1", Some("manager"), false),
            ],
            pane_map: HashMap::from([
                ("architect".to_string(), architect_pane.clone()),
                ("manager".to_string(), manager_pane.clone()),
                ("eng-1".to_string(), engineer_pane.clone()),
            ]),
        })
        .unwrap();

        daemon.run_startup_preflight().unwrap();
        assert!(!crate::tmux::pane_dead(&architect_pane).unwrap_or(true));
        assert!(!crate::tmux::pane_dead(&manager_pane).unwrap_or(true));
        assert!(!crate::tmux::pane_dead(&engineer_pane).unwrap_or(true));

        daemon.spawn_all_agents(true).unwrap();
        std::thread::sleep(Duration::from_millis(300));

        assert!(crate::tmux::session_exists(&session));
        assert_eq!(
            daemon.states.get("architect"),
            Some(&crate::team::standup::MemberState::Idle)
        );
        assert_eq!(
            daemon.states.get("manager"),
            Some(&crate::team::standup::MemberState::Idle)
        );
        assert_eq!(
            daemon.states.get("eng-1"),
            Some(&crate::team::standup::MemberState::Idle)
        );

        let events = crate::team::events::read_events(
            &repo.join(".batty").join("team_config").join("events.jsonl"),
        )
        .unwrap();
        assert!(events.iter().any(|event| {
            event.event == "pane_respawned" && event.role.as_deref() == Some("eng-1")
        }));
        assert!(!events.iter().any(|event| {
            event.event == "pane_respawned" && event.role.as_deref() == Some("architect")
        }));
        assert!(!events.iter().any(|event| {
            event.event == "pane_respawned" && event.role.as_deref() == Some("manager")
        }));

        let orchestrator_log =
            std::fs::read_to_string(repo.join(".batty").join("orchestrator.log")).unwrap();
        assert!(orchestrator_log.contains("architect=no (prompt changed)"));
        assert!(orchestrator_log.contains("manager=no (prompt changed)"));
        assert!(orchestrator_log.contains("eng-1=no (prompt changed)"));

        crate::tmux::kill_session(&session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_claude_bin);
        let _ = std::fs::remove_dir_all(&fake_codex_bin);
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
            ..Default::default()
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
            // This test only verifies pane cwd correction; enabling worktrees
            // needlessly couples it to git availability in the environment.
            use_worktrees: false,
            ..Default::default()
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
            // This test only verifies pane cwd correction; enabling worktrees
            // needlessly couples it to git availability in the environment.
            use_worktrees: false,
            ..Default::default()
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
