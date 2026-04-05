// Daemon tests — core lifecycle, dispatch, nudges, interventions, state persistence.

use super::*;
use crate::team::config::AutomationConfig;
use crate::team::config::{BoardConfig, RoleDef, StandupConfig, WorkflowMode, WorkflowPolicy};
use crate::team::events::EventSink;
use crate::team::test_helpers::make_test_daemon;
use crate::team::test_support::{
    EnvVarGuard, PATH_LOCK, TestDaemonBuilder, architect_member, backdate_idle_grace,
    engineer_member, init_git_repo, manager_member, write_board_task_file, write_open_task_file,
    write_owned_task_file,
};
use std::time::UNIX_EPOCH;

use serial_test::serial;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
fn production_unwrap_expect_count(path: &Path) -> usize {
    let content = std::fs::read_to_string(path).unwrap();
    let test_split = content.split("\n#[cfg(test)]").next().unwrap_or(&content);
    test_split
        .lines()
        .filter(|line| line.contains(".unwrap(") || line.contains(".expect("))
        .count()
}
fn setup_fake_codex(project_root: &Path, log_root: &Path, member_name: &str) -> (PathBuf, PathBuf) {
    let project_slug = project_root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "default".to_string());
    let fake_bin = std::env::temp_dir().join(format!("batty-bin-{project_slug}-{member_name}"));
    let _ = std::fs::remove_dir_all(&fake_bin);
    std::fs::create_dir_all(&fake_bin).unwrap();

    let fake_log = log_root.join(format!("{member_name}-fake-codex.log"));
    let fake_codex = fake_bin.join("codex");
    std::fs::write(
        &fake_codex,
        format!(
            "#!/bin/bash\nprintf 'PWD:%s\\nARGS:%s\\n' \"$PWD\" \"$*\" >> '{}'\nsleep 1\n",
            fake_log.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_codex, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    (fake_bin, fake_log)
}

fn write_codex_session_meta(cwd: &Path) -> PathBuf {
    let home = PathBuf::from(std::env::var("HOME").expect("HOME must be set for tests"));
    let session_dir = home
        .join(".codex")
        .join("sessions")
        .join("2099")
        .join("12")
        .join("31");
    std::fs::create_dir_all(&session_dir).unwrap();

    let unique = format!(
        "batty-daemon-lifecycle-{}-{}.jsonl",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let session_file = session_dir.join(unique);
    std::fs::write(
        &session_file,
        format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"cwd\":\"{}\"}}}}\n",
            cwd.display()
        ),
    )
    .unwrap();
    session_file
}

fn append_codex_task_complete(session_file: &Path) {
    let mut handle = OpenOptions::new().append(true).open(session_file).unwrap();
    writeln!(
        handle,
        "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\"}}}}"
    )
    .unwrap();
    handle.flush().unwrap();
}

fn wait_for_log_contains(log_path: &Path, needle: &str) -> String {
    (0..300)
        .find_map(|_| {
            let content = match std::fs::read_to_string(log_path) {
                Ok(content) => content,
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(100));
                    return None;
                }
            };
            if content.contains(needle) {
                Some(content)
            } else {
                std::thread::sleep(Duration::from_millis(100));
                None
            }
        })
        .unwrap_or_else(|| panic!("log {} never contained `{needle}`", log_path.display()))
}

fn starvation_test_daemon(tmp: &tempfile::TempDir, threshold: Option<usize>) -> TeamDaemon {
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![
            architect_member("architect"),
            engineer_member("eng-1", Some("architect"), false),
            engineer_member("eng-2", Some("architect"), false),
        ])
        .workflow_policy(WorkflowPolicy {
            pipeline_starvation_threshold: threshold,
            ..WorkflowPolicy::default()
        })
        .build();
    daemon.states = HashMap::from([
        ("eng-1".to_string(), MemberState::Idle),
        ("eng-2".to_string(), MemberState::Idle),
    ]);
    daemon
}

#[test]
fn extract_nudge_from_file() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        tmp.path(),
        "# Architect\n\n## Nudge\n\nCheck work.\nUpdate roadmap.\n\n## Other\n\nstuff\n",
    )
    .unwrap();
    let nudge = extract_nudge_section(tmp.path()).unwrap();
    assert!(nudge.contains("Check work."));
    assert!(nudge.contains("Update roadmap."));
    assert!(!nudge.contains("## Other"));
}

#[test]
fn extract_nudge_returns_none_when_absent() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), "# Engineer\n\n## Workflow\n\n- code\n").unwrap();
    assert!(extract_nudge_section(tmp.path()).is_none());
}

#[test]
fn extract_nudge_returns_none_when_malformed() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        tmp.path(),
        "# Engineer\n\n## Nudge\n\n## Workflow\n\n- code\n",
    )
    .unwrap();
    assert!(extract_nudge_section(tmp.path()).is_none());
}

#[test]
fn daemon_registers_per_role_nudge_intervals_from_prompt_sections() {
    let tmp = tempfile::tempdir().unwrap();
    let team_config_dir = tmp.path().join(".batty").join("team_config");
    std::fs::create_dir_all(&team_config_dir).unwrap();
    std::fs::write(
        team_config_dir.join("manager.md"),
        "# Manager\n\n## Nudge\n\nManager follow-up.\n",
    )
    .unwrap();
    std::fs::write(
        team_config_dir.join("engineer.md"),
        "# Engineer\n\n## Nudge\n\nEngineer follow-up.\n",
    )
    .unwrap();

    let daemon = TeamDaemon::new(DaemonConfig {
        project_root: tmp.path().to_path_buf(),
        team_config: TeamConfig {
            name: "test".to_string(),
            agent: None,
            workflow_mode: WorkflowMode::Hybrid,
            board: BoardConfig::default(),
            standup: StandupConfig::default(),
            automation: AutomationConfig::default(),
            automation_sender: None,
            external_senders: Vec::new(),
            orchestrator_pane: true,
            orchestrator_position: OrchestratorPosition::Bottom,
            layout: None,
            workflow_policy: WorkflowPolicy::default(),
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
            roles: vec![
                RoleDef {
                    name: "manager".to_string(),
                    role_type: RoleType::Manager,
                    agent: Some("claude".to_string()),
                    instances: 1,
                    prompt: None,
                    talks_to: vec![],
                    channel: None,
                    channel_config: None,
                    nudge_interval_secs: Some(120),
                    receives_standup: None,
                    standup_interval_secs: None,
                    owns: Vec::new(),
                    barrier_group: None,
                    use_worktrees: false,
                },
                RoleDef {
                    name: "engineer".to_string(),
                    role_type: RoleType::Engineer,
                    agent: Some("codex".to_string()),
                    instances: 1,
                    prompt: None,
                    talks_to: vec![],
                    channel: None,
                    channel_config: None,
                    nudge_interval_secs: Some(300),
                    receives_standup: None,
                    standup_interval_secs: None,
                    owns: Vec::new(),
                    barrier_group: None,
                    use_worktrees: false,
                },
            ],
        },
        session: "test".to_string(),
        members: vec![
            MemberInstance {
                name: "lead".to_string(),
                role_name: "manager".to_string(),
                role_type: RoleType::Manager,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: None,
                use_worktrees: false,
            },
            MemberInstance {
                name: "eng-1".to_string(),
                role_name: "engineer".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("codex".to_string()),
                prompt: None,
                reports_to: Some("lead".to_string()),
                use_worktrees: true,
            },
        ],
        pane_map: HashMap::new(),
    })
    .unwrap();

    assert_eq!(
        daemon
            .nudges
            .get("lead")
            .map(|schedule| schedule.text.as_str()),
        Some("Manager follow-up.")
    );
    assert_eq!(
        daemon.nudges.get("lead").map(|schedule| schedule.interval),
        Some(Duration::from_secs(120))
    );
    assert_eq!(
        daemon
            .nudges
            .get("eng-1")
            .map(|schedule| schedule.text.as_str()),
        Some("Engineer follow-up.")
    );
    assert_eq!(
        daemon.nudges.get("eng-1").map(|schedule| schedule.interval),
        Some(Duration::from_secs(300))
    );
}

#[test]
fn format_nudge_status_marks_sent_after_fire() {
    let schedule = NudgeSchedule {
        text: "check in".to_string(),
        interval: Duration::from_secs(600),
        idle_since: Some(Instant::now() - Duration::from_secs(601)),
        fired_this_idle: true,
        paused: false,
    };

    assert_eq!(
        status::format_nudge_status(Some(&schedule)),
        " #[fg=magenta]nudge sent#[default]"
    );
}

#[test]
fn daemon_state_round_trip_preserves_runtime_fields() {
    let tmp = tempfile::tempdir().unwrap();
    let state = PersistedDaemonState {
        clean_shutdown: false,
        saved_at: 123,
        states: HashMap::from([("eng-1".to_string(), MemberState::Working)]),
        active_tasks: HashMap::from([("eng-1".to_string(), 42)]),
        retry_counts: HashMap::from([("eng-1".to_string(), 2)]),
        dispatch_queue: vec![DispatchQueueEntry {
            engineer: "eng-1".to_string(),
            task_id: 77,
            task_title: "queued".to_string(),
            queued_at: 999,
            validation_failures: 1,
            last_failure: Some("waiting for stabilization".to_string()),
        }],
        paused_standups: HashSet::from(["manager".to_string()]),
        last_standup_elapsed_secs: HashMap::from([("architect".to_string(), 55)]),
        nudge_state: HashMap::from([(
            "eng-1".to_string(),
            PersistedNudgeState {
                idle_elapsed_secs: Some(88),
                fired_this_idle: true,
                paused: false,
            },
        )]),
        pipeline_starvation_fired: true,
    };

    save_daemon_state(tmp.path(), &state).unwrap();

    let loaded = load_daemon_state(tmp.path()).unwrap();
    assert_eq!(loaded, state);
}

#[test]
fn watcher_mut_returns_context_for_unknown_member() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
    let mut daemon = make_test_daemon(tmp.path(), vec![manager_member("manager", None)]);

    let error = match daemon.watcher_mut("missing") {
        Ok(_) => panic!("expected missing watcher to return an error"),
        Err(error) => error,
    };

    assert!(
        error
            .to_string()
            .contains("watcher registry missing member 'missing'")
    );
}

#[test]
fn test_auto_dispatch_filters_idle_engineers_only() {
    let tmp = tempfile::tempdir().unwrap();
    let roles = vec![
        RoleDef {
            name: "architect".to_string(),
            role_type: RoleType::Architect,
            agent: Some("claude".to_string()),
            instances: 1,
            prompt: None,
            talks_to: vec![],
            channel: None,
            channel_config: None,
            nudge_interval_secs: None,
            receives_standup: None,
            standup_interval_secs: None,
            owns: Vec::new(),
            barrier_group: None,
            use_worktrees: false,
        },
        RoleDef {
            name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            instances: 1,
            prompt: None,
            talks_to: vec![],
            channel: None,
            channel_config: None,
            nudge_interval_secs: None,
            receives_standup: None,
            standup_interval_secs: None,
            owns: Vec::new(),
            barrier_group: None,
            use_worktrees: false,
        },
        RoleDef {
            name: "eng-1".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            instances: 1,
            prompt: None,
            talks_to: vec![],
            channel: None,
            channel_config: None,
            nudge_interval_secs: None,
            receives_standup: None,
            standup_interval_secs: None,
            owns: Vec::new(),
            barrier_group: None,
            use_worktrees: false,
        },
        RoleDef {
            name: "eng-2".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            instances: 1,
            prompt: None,
            talks_to: vec![],
            channel: None,
            channel_config: None,
            nudge_interval_secs: None,
            receives_standup: None,
            standup_interval_secs: None,
            owns: Vec::new(),
            barrier_group: None,
            use_worktrees: false,
        },
    ];
    let members = vec![
        MemberInstance {
            name: "architect".to_string(),
            role_name: "architect".to_string(),
            role_type: RoleType::Architect,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        },
        MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        },
        MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng-1".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
        },
        MemberInstance {
            name: "eng-2".to_string(),
            role_name: "eng-2".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
        },
    ];

    let mut daemon = TeamDaemon::new(DaemonConfig {
        project_root: tmp.path().to_path_buf(),
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
            roles,
        },
        session: "test".to_string(),
        members,
        pane_map: HashMap::new(),
    })
    .unwrap();

    daemon
        .states
        .insert("architect".to_string(), MemberState::Idle);
    daemon
        .states
        .insert("manager".to_string(), MemberState::Idle);
    daemon.states.insert("eng-1".to_string(), MemberState::Idle);
    daemon
        .states
        .insert("eng-2".to_string(), MemberState::Working);

    let board_dir = tmp.path().join(".batty").join("team_config").join("board");
    let tasks_dir = board_dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(
            tasks_dir.join("001-auto-task.md"),
            "---\nid: 1\ntitle: auto-task\nstatus: todo\npriority: high\nclass: standard\n---\n\nTask description.\n",
        )
        .unwrap();

    assert_eq!(daemon.idle_engineer_names(), vec!["eng-1".to_string()]);
    let task = next_unclaimed_task(&board_dir).unwrap().unwrap();
    assert_eq!(task.id, 1);
}

#[test]
fn test_maybe_auto_dispatch_respects_rate_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = TestDaemonBuilder::new(tmp.path()).build();

    let before = daemon.last_auto_dispatch;
    daemon.maybe_auto_dispatch().unwrap();
    assert_eq!(daemon.last_auto_dispatch, before);
}

#[test]
fn test_maybe_auto_dispatch_skips_when_disabled() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .board(BoardConfig {
            auto_dispatch: false,
            ..BoardConfig::default()
        })
        .build();
    daemon.last_auto_dispatch = Instant::now() - Duration::from_secs(30);

    let before = daemon.last_auto_dispatch;
    daemon.maybe_auto_dispatch().unwrap();
    assert_eq!(daemon.last_auto_dispatch, before);
}

#[test]
#[serial]
#[cfg_attr(not(feature = "integration"), ignore)]
fn daemon_lifecycle_happy_path_exercises_decomposed_modules() {
    let session = format!("batty-test-daemon-lifecycle-{}", std::process::id());
    let _ = crate::tmux::kill_session(&session);

    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-daemon-lifecycle");
    write_open_task_file(&repo, 42, "lifecycle-task", "todo");

    let member_name = "eng-lifecycle";
    let (fake_bin, fake_log) = setup_fake_codex(&repo, tmp.path(), member_name);

    crate::tmux::create_session(&session, "bash", &[], repo.to_string_lossy().as_ref()).unwrap();
    crate::tmux::create_window(
        &session,
        "keeper",
        "sleep",
        &["30".to_string()],
        repo.to_string_lossy().as_ref(),
    )
    .unwrap();
    let pane_id = crate::tmux::pane_id(&session).unwrap();

    let engineer = MemberInstance {
        name: member_name.to_string(),
        role_name: "engineer".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: true,
    };
    let mut daemon = TeamDaemon::new(DaemonConfig {
        project_root: repo.clone(),
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
        pane_map: HashMap::from([(member_name.to_string(), pane_id)]),
    })
    .unwrap();
    daemon.spawn_all_agents(false).unwrap();
    let spawn_log = wait_for_log_contains(&fake_log, "PWD:");
    assert!(spawn_log.contains("PWD:"));
    std::thread::sleep(Duration::from_millis(1200));

    let assignment = "Task #42: lifecycle-task\n\nTask description.";
    daemon
        .assign_task_with_task_id(member_name, assignment, Some(42))
        .unwrap();
    daemon.active_tasks.insert(member_name.to_string(), 42);
    assert_eq!(daemon.active_task_id(member_name), Some(42));
    assert_eq!(daemon.states.get(member_name), Some(&MemberState::Working));

    let worktree_dir = repo.join(".batty").join("worktrees").join(member_name);
    assert!(worktree_dir.exists());
    assert_eq!(
        crate::team::test_support::git_stdout(&worktree_dir, &["branch", "--show-current"]),
        format!("{member_name}/42")
    );

    let codex_cwd = worktree_dir
        .join(".batty")
        .join("codex-context")
        .join(member_name);
    let session_file = write_codex_session_meta(&codex_cwd);

    daemon.run_loop_step("poll_watchers", |daemon| daemon.poll_watchers());
    daemon.run_loop_step("sync_launch_state_session_ids", |daemon| {
        daemon.sync_launch_state_session_ids()
    });

    std::fs::write(worktree_dir.join("note.txt"), "done\n").unwrap();
    crate::team::test_support::git_ok(&worktree_dir, &["add", "note.txt"]);
    crate::team::test_support::git_ok(&worktree_dir, &["commit", "-m", "finish task"]);
    append_codex_task_complete(&session_file);

    daemon.run_loop_step("poll_watchers", |daemon| daemon.poll_watchers());

    assert_eq!(daemon.active_task_id(member_name), None);
    assert_eq!(daemon.states.get(member_name), Some(&MemberState::Idle));
    assert_eq!(
        std::fs::read_to_string(repo.join("note.txt")).unwrap(),
        "done\n"
    );

    let events = crate::team::events::read_events(
        &repo.join(".batty").join("team_config").join("events.jsonl"),
    )
    .unwrap();
    assert!(events.iter().any(|event| {
        event.event == "task_assigned"
            && event.role.as_deref() == Some(member_name)
            && event
                .task
                .as_deref()
                .is_some_and(|task| task.contains("Task #42: lifecycle-task"))
    }));
    assert!(events.iter().any(|event| {
        event.event == "task_completed" && event.role.as_deref() == Some(member_name)
    }));

    let launch_state = load_launch_state(&repo);
    let identity = launch_state.get(member_name).expect("missing launch state");
    assert_eq!(identity.agent, "codex-cli");
    assert_eq!(
        identity.session_id.as_deref(),
        session_file.file_stem().and_then(|stem| stem.to_str())
    );

    crate::tmux::kill_session(&session).unwrap();
    let _ = std::fs::remove_file(&session_file);
    let _ = std::fs::remove_dir_all(&fake_bin);
}
#[test]
#[serial]
#[cfg_attr(not(feature = "integration"), ignore)]
fn maybe_fire_nudges_marks_member_working_after_live_delivery() {
    let session = "batty-test-nudge-live-delivery";
    let mut delivered_live = false;

    // A freshly created tmux pane can occasionally reject the first live
    // injection under heavy suite load. Retry the full setup a few times so
    // this test only fails on a real regression in the live-delivery path.
    for _attempt in 0..5 {
        let _ = crate::tmux::kill_session(session);

        crate::tmux::create_session(session, "cat", &[], "/tmp").unwrap();
        let pane_id = crate::tmux::pane_id(session).unwrap();
        std::thread::sleep(Duration::from_millis(300));

        let tmp = tempfile::tempdir().unwrap();
        let mut watchers = HashMap::new();
        let mut scientist_watcher = SessionWatcher::new(&pane_id, "scientist", 300, None);
        scientist_watcher.confirm_ready();
        watchers.insert("scientist".to_string(), scientist_watcher);
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .session(session)
            .members(vec![architect_member("scientist")])
            .pane_map(HashMap::from([("scientist".to_string(), pane_id.clone())]))
            .watchers(watchers)
            .states(HashMap::from([(
                "scientist".to_string(),
                MemberState::Idle,
            )]))
            .nudges(HashMap::from([(
                "scientist".to_string(),
                NudgeSchedule {
                    text: "Please make progress.".to_string(),
                    interval: Duration::from_secs(1),
                    idle_since: Some(Instant::now() - Duration::from_secs(5)),
                    fired_this_idle: false,
                    paused: false,
                },
            )]))
            .build();

        backdate_idle_grace(&mut daemon, "scientist");
        daemon.maybe_fire_nudges().unwrap();

        if daemon.states.get("scientist") == Some(&MemberState::Working) {
            let schedule = daemon.nudges.get("scientist").unwrap();
            assert!(schedule.paused);
            assert!(schedule.idle_since.is_none());
            assert!(!schedule.fired_this_idle);
            delivered_live = true;
            crate::tmux::kill_session(session).unwrap();
            break;
        }

        crate::tmux::kill_session(session).unwrap();
        std::thread::sleep(Duration::from_millis(100));
    }

    assert!(
        delivered_live,
        "expected at least one successful live nudge delivery"
    );
}

#[test]
#[serial]
#[cfg_attr(not(feature = "integration"), ignore)]
fn maybe_intervene_triage_backlog_marks_member_working_after_live_delivery() {
    let session = format!("batty-test-triage-live-delivery-{}", std::process::id());
    let _ = crate::tmux::kill_session(&session);

    crate::tmux::create_session(&session, "cat", &[], "/tmp").unwrap();
    let pane_id = crate::tmux::pane_id(&session).unwrap();
    std::thread::sleep(Duration::from_millis(300));

    let tmp = tempfile::tempdir().unwrap();
    let mut watchers = HashMap::new();
    let mut lead_watcher = SessionWatcher::new(&pane_id, "lead", 300, None);
    lead_watcher.confirm_ready();
    watchers.insert("lead".to_string(), lead_watcher);
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .session(&session)
        .members(vec![
            manager_member("lead", Some("architect")),
            engineer_member("eng-1", Some("lead"), false),
        ])
        .pane_map(HashMap::from([("lead".to_string(), pane_id.clone())]))
        .watchers(watchers)
        .states(HashMap::from([("lead".to_string(), MemberState::Idle)]))
        .build();

    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "lead").unwrap();
    inbox::init_inbox(&root, "eng-1").unwrap();
    let mut result = inbox::InboxMessage::new_send("eng-1", "lead", "Task complete.");
    result.timestamp = super::now_unix();
    let id = inbox::deliver_to_inbox(&root, &result).unwrap();
    inbox::mark_delivered(&root, "lead", &id).unwrap();

    daemon.update_automation_timers_for_state("lead", MemberState::Working);
    daemon.update_automation_timers_for_state("lead", MemberState::Idle);
    backdate_idle_grace(&mut daemon, "lead");
    daemon.maybe_intervene_triage_backlog().unwrap();

    assert_eq!(daemon.triage_interventions.get("lead"), Some(&1));
    if daemon.states.get("lead") == Some(&MemberState::Working) {
        let pane = (0..50)
            .find_map(|_| {
                let pane = tmux::capture_pane(&pane_id).unwrap_or_default();
                if pane.contains("batty inbox lead") {
                    Some(pane)
                } else {
                    std::thread::sleep(Duration::from_millis(200));
                    None
                }
            })
            .unwrap_or_else(|| tmux::capture_pane(&pane_id).unwrap_or_default());
        assert!(pane.contains("batty inbox lead"));
        assert!(pane.contains("batty read lead <ref>"));
        assert!(pane.contains("batty send eng-1"));
        assert!(pane.contains("batty assign eng-1"));
        assert!(pane.contains("batty send architect"));
        assert!(pane.contains("next time you become idle"));
    } else {
        let pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].body.contains("batty inbox lead"));
    }

    crate::tmux::kill_session(&session).unwrap();
}

#[test]
fn maybe_intervene_triage_backlog_queues_when_live_delivery_falls_back_to_inbox() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![
            manager_member("lead", Some("architect")),
            engineer_member("eng-1", Some("lead"), false),
        ])
        .pane_map(HashMap::from([(
            "lead".to_string(),
            "%9999999".to_string(),
        )]))
        .states(HashMap::from([("lead".to_string(), MemberState::Idle)]))
        .build();

    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "lead").unwrap();
    inbox::init_inbox(&root, "eng-1").unwrap();
    let mut result = inbox::InboxMessage::new_send("eng-1", "lead", "Task complete.");
    result.timestamp = super::now_unix();
    let id = inbox::deliver_to_inbox(&root, &result).unwrap();
    inbox::mark_delivered(&root, "lead", &id).unwrap();

    daemon.update_automation_timers_for_state("lead", MemberState::Working);
    daemon.update_automation_timers_for_state("lead", MemberState::Idle);
    backdate_idle_grace(&mut daemon, "lead");
    daemon.maybe_intervene_triage_backlog().unwrap();

    assert_eq!(daemon.states.get("lead"), Some(&MemberState::Idle));
    assert_eq!(daemon.triage_interventions.get("lead"), Some(&1));
    let pending = inbox::pending_messages(&root, "lead").unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].from, "architect");
    assert!(pending[0].body.contains("Triage backlog detected"));
    assert!(pending[0].body.contains("batty inbox lead"));
    assert!(pending[0].body.contains("batty read lead <ref>"));
    assert!(pending[0].body.contains("batty send eng-1"));
    assert!(pending[0].body.contains("batty assign eng-1"));
    assert!(pending[0].body.contains("batty send architect"));
    assert!(pending[0].body.contains("next time you become idle"));
}

#[test]
fn maybe_intervene_triage_backlog_does_not_fire_on_startup_idle() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![
            manager_member("lead", Some("architect")),
            engineer_member("eng-1", Some("lead"), false),
        ])
        .pane_map(HashMap::from([("lead".to_string(), "%999".to_string())]))
        .states(HashMap::from([("lead".to_string(), MemberState::Idle)]))
        .build();

    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "lead").unwrap();
    inbox::init_inbox(&root, "eng-1").unwrap();
    let mut result = inbox::InboxMessage::new_send("eng-1", "lead", "Task complete.");
    result.timestamp = super::now_unix();
    let id = inbox::deliver_to_inbox(&root, &result).unwrap();
    inbox::mark_delivered(&root, "lead", &id).unwrap();

    daemon.maybe_intervene_triage_backlog().unwrap();

    assert!(!daemon.triage_interventions.contains_key("lead"));
    assert!(inbox::pending_messages(&root, "lead").unwrap().is_empty());
    assert_eq!(daemon.states.get("lead"), Some(&MemberState::Idle));
}

#[test]
fn maybe_intervene_owned_tasks_queues_when_idle_member_owns_unfinished_task() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![
            manager_member("lead", Some("architect")),
            engineer_member("eng-1", Some("lead"), false),
        ])
        .pane_map(HashMap::from([("lead".to_string(), "%999".to_string())]))
        .states(HashMap::from([("lead".to_string(), MemberState::Idle)]))
        .build();

    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "lead").unwrap();
    write_owned_task_file(tmp.path(), 191, "owned-task", "in-progress", "lead");

    daemon.update_automation_timers_for_state("lead", MemberState::Working);
    daemon.update_automation_timers_for_state("lead", MemberState::Idle);
    backdate_idle_grace(&mut daemon, "lead");
    daemon.maybe_intervene_owned_tasks().unwrap();

    assert_eq!(daemon.states.get("lead"), Some(&MemberState::Idle));
    assert_eq!(
        daemon
            .owned_task_interventions
            .get("lead")
            .map(|state| state.idle_epoch),
        Some(1)
    );
    let pending = inbox::pending_messages(&root, "lead").unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].from, "architect");
    assert!(pending[0].body.contains("Task #191"));
    assert!(
        pending[0]
            .body
            .contains("Owned active task backlog detected")
    );
    assert!(pending[0].body.contains("kanban-md list --dir"));
    assert!(pending[0].body.contains("kanban-md show --dir"));
    assert!(pending[0].body.contains("191"));
    assert!(pending[0].body.contains("sed -n '1,220p'"));
    assert!(pending[0].body.contains("batty assign eng-1"));
    assert!(pending[0].body.contains("batty send architect"));
    assert!(pending[0].body.contains("kanban-md move --dir"));
    assert!(pending[0].body.contains("next time you become idle"));
}

#[test]
fn maybe_intervene_owned_tasks_engineer_message_captures_initial_state() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![
            manager_member("lead", Some("architect")),
            engineer_member("eng-1", Some("lead"), false),
        ])
        .pane_map(HashMap::from([("eng-1".to_string(), "%999".to_string())]))
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();

    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "eng-1").unwrap();
    write_owned_task_file(tmp.path(), 191, "owned-task", "in-progress", "eng-1");

    daemon.update_automation_timers_for_state("eng-1", MemberState::Working);
    daemon.update_automation_timers_for_state("eng-1", MemberState::Idle);
    backdate_idle_grace(&mut daemon, "eng-1");
    daemon.maybe_intervene_owned_tasks().unwrap();

    let pending = inbox::pending_messages(&root, "eng-1").unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].from, "lead");
    assert!(
        pending[0]
            .body
            .contains("Owned active task backlog detected")
    );
    assert!(pending[0].body.contains("Task #191"));
    assert!(pending[0].body.contains("batty send lead"));

    let state = daemon.owned_task_interventions.get("eng-1").unwrap();
    assert_eq!(state.idle_epoch, 1);
    assert_eq!(state.signature, "191:in-progress");
    assert!(!state.escalation_sent);
}

#[test]
fn maybe_intervene_owned_tasks_fires_for_persistent_startup_idle_state() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![
            manager_member("lead", Some("architect")),
            engineer_member("eng-1", Some("lead"), false),
        ])
        .pane_map(HashMap::from([("lead".to_string(), "%999".to_string())]))
        .states(HashMap::from([("lead".to_string(), MemberState::Idle)]))
        .build();

    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "lead").unwrap();
    write_owned_task_file(tmp.path(), 191, "owned-task", "in-progress", "lead");

    backdate_idle_grace(&mut daemon, "lead");
    daemon.maybe_intervene_owned_tasks().unwrap();

    let pending = inbox::pending_messages(&root, "lead").unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].from, "architect");
    assert!(pending[0].body.contains("Task #191"));
    assert_eq!(
        daemon
            .owned_task_interventions
            .get("lead")
            .map(|state| state.idle_epoch),
        Some(0)
    );
    assert_eq!(daemon.states.get("lead"), Some(&MemberState::Idle));
}

#[test]
fn maybe_intervene_owned_tasks_waits_for_idle_grace() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![manager_member("lead", Some("architect"))])
        .pane_map(HashMap::from([("lead".to_string(), "%999".to_string())]))
        .states(HashMap::from([("lead".to_string(), MemberState::Idle)]))
        .build();

    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "lead").unwrap();
    write_owned_task_file(tmp.path(), 191, "owned-task", "in-progress", "lead");

    daemon.update_automation_timers_for_state("lead", MemberState::Idle);
    daemon.maybe_intervene_owned_tasks().unwrap();
    assert!(inbox::pending_messages(&root, "lead").unwrap().is_empty());

    backdate_idle_grace(&mut daemon, "lead");
    daemon.maybe_intervene_owned_tasks().unwrap();
    assert_eq!(inbox::pending_messages(&root, "lead").unwrap().len(), 1);
}

#[test]
fn maybe_intervene_owned_tasks_skips_when_pending_inbox_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![manager_member("lead", Some("architect"))])
        .pane_map(HashMap::from([("lead".to_string(), "%999".to_string())]))
        .states(HashMap::from([("lead".to_string(), MemberState::Idle)]))
        .build();

    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "lead").unwrap();
    let message = inbox::InboxMessage::new_send("architect", "lead", "Check this first.");
    inbox::deliver_to_inbox(&root, &message).unwrap();
    write_owned_task_file(tmp.path(), 191, "owned-task", "in-progress", "lead");

    backdate_idle_grace(&mut daemon, "lead");
    daemon.maybe_intervene_owned_tasks().unwrap();

    let pending = inbox::pending_messages(&root, "lead").unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].from, "architect");
    assert!(
        !daemon.owned_task_interventions.contains_key("lead"),
        "pending inbox should block new interventions"
    );
}

#[test]
fn maybe_intervene_owned_tasks_ignores_review_only_claims() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![manager_member("lead", Some("architect"))])
        .pane_map(HashMap::from([("lead".to_string(), "%999".to_string())]))
        .states(HashMap::from([("lead".to_string(), MemberState::Idle)]))
        .build();

    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "lead").unwrap();
    write_owned_task_file(tmp.path(), 191, "review-task", "review", "lead");

    daemon.update_automation_timers_for_state("lead", MemberState::Working);
    daemon.update_automation_timers_for_state("lead", MemberState::Idle);
    daemon.maybe_intervene_owned_tasks().unwrap();

    assert!(!daemon.owned_task_interventions.contains_key("lead"));
    assert!(inbox::pending_messages(&root, "lead").unwrap().is_empty());
}

#[test]
fn maybe_intervene_owned_tasks_dedupes_same_active_signature_across_idle_epochs() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![manager_member("lead", Some("architect"))])
        .pane_map(HashMap::from([("lead".to_string(), "%999".to_string())]))
        .states(HashMap::from([("lead".to_string(), MemberState::Idle)]))
        .build();

    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "lead").unwrap();
    write_owned_task_file(tmp.path(), 191, "owned-task", "in-progress", "lead");

    daemon.update_automation_timers_for_state("lead", MemberState::Working);
    daemon.update_automation_timers_for_state("lead", MemberState::Idle);
    backdate_idle_grace(&mut daemon, "lead");
    daemon.maybe_intervene_owned_tasks().unwrap();

    daemon
        .states
        .insert("lead".to_string(), MemberState::Working);
    daemon.update_automation_timers_for_state("lead", MemberState::Working);
    daemon.states.insert("lead".to_string(), MemberState::Idle);
    daemon.update_automation_timers_for_state("lead", MemberState::Idle);
    backdate_idle_grace(&mut daemon, "lead");
    daemon.maybe_intervene_owned_tasks().unwrap();

    let pending = inbox::pending_messages(&root, "lead").unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        daemon
            .owned_task_interventions
            .get("lead")
            .map(|state| state.idle_epoch),
        Some(2)
    );
}

fn backdate_intervention_cooldown(daemon: &mut TeamDaemon, key: &str) {
    let cooldown = Duration::from_secs(
        daemon
            .config
            .team_config
            .automation
            .intervention_cooldown_secs,
    ) + Duration::from_secs(1);
    daemon
        .intervention_cooldowns
        .insert(key.to_string(), Instant::now() - cooldown);
}

#[test]
fn owned_task_intervention_updates_signature_when_board_state_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![
            manager_member("lead", Some("architect")),
            engineer_member("eng-1", Some("lead"), false),
        ])
        .pane_map(HashMap::from([("eng-1".to_string(), "%999".to_string())]))
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();

    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "eng-1").unwrap();
    write_owned_task_file(tmp.path(), 191, "first-task", "in-progress", "eng-1");

    daemon.update_automation_timers_for_state("eng-1", MemberState::Working);
    daemon.update_automation_timers_for_state("eng-1", MemberState::Idle);
    backdate_idle_grace(&mut daemon, "eng-1");
    daemon.maybe_intervene_owned_tasks().unwrap();

    let initial = daemon.owned_task_interventions.get("eng-1").unwrap();
    assert_eq!(initial.signature, "191:in-progress");

    for message in inbox::pending_messages(&root, "eng-1").unwrap() {
        inbox::mark_delivered(&root, "eng-1", &message.id).unwrap();
    }

    write_owned_task_file(tmp.path(), 192, "second-task", "in-progress", "eng-1");
    backdate_intervention_cooldown(&mut daemon, "eng-1");
    daemon.maybe_intervene_owned_tasks().unwrap();

    let pending = inbox::pending_messages(&root, "eng-1").unwrap();
    assert_eq!(pending.len(), 1);
    assert!(pending[0].body.contains("Task #191"));
    assert!(pending[0].body.contains("#192 (in-progress) second-task"));

    let updated = daemon.owned_task_interventions.get("eng-1").unwrap();
    assert_eq!(updated.signature, "191:in-progress|192:in-progress");
    assert!(!updated.escalation_sent);
}

#[test]
fn owned_task_intervention_respects_cooldown() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![manager_member("lead", Some("architect"))])
        .pane_map(HashMap::from([("lead".to_string(), "%999".to_string())]))
        .states(HashMap::from([("lead".to_string(), MemberState::Idle)]))
        .build();

    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "lead").unwrap();
    write_owned_task_file(tmp.path(), 191, "first-task", "in-progress", "lead");

    // First fire: should deliver intervention.
    backdate_idle_grace(&mut daemon, "lead");
    daemon.maybe_intervene_owned_tasks().unwrap();
    let pending = inbox::pending_messages(&root, "lead").unwrap();
    assert_eq!(pending.len(), 1, "first intervention should fire");

    // Acknowledge the message so inbox is clear for next check.
    for msg in pending {
        inbox::mark_delivered(&root, "lead", &msg.id).unwrap();
    }

    // Change signature (add another task) — should still be blocked by cooldown.
    write_owned_task_file(tmp.path(), 192, "second-task", "in-progress", "lead");
    daemon.maybe_intervene_owned_tasks().unwrap();
    let pending = inbox::pending_messages(&root, "lead").unwrap();
    assert_eq!(pending.len(), 0, "cooldown should prevent refire");

    // Expire the cooldown — should fire again.
    backdate_intervention_cooldown(&mut daemon, "lead");
    daemon.maybe_intervene_owned_tasks().unwrap();
    let pending = inbox::pending_messages(&root, "lead").unwrap();
    assert_eq!(pending.len(), 1, "should fire after cooldown expires");
}

#[test]
fn triage_intervention_respects_cooldown() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![
            manager_member("lead", Some("architect")),
            engineer_member("eng-1", Some("lead"), false),
        ])
        .pane_map(HashMap::from([
            ("lead".to_string(), "%999".to_string()),
            ("eng-1".to_string(), "%998".to_string()),
        ]))
        .states(HashMap::from([("lead".to_string(), MemberState::Idle)]))
        .build();
    daemon.triage_idle_epochs = HashMap::from([("lead".to_string(), 1)]);

    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "lead").unwrap();
    inbox::init_inbox(&root, "eng-1").unwrap();

    // Deliver a message from eng-1 to lead's inbox so triage finds something.
    let msg = inbox::InboxMessage::new_send("eng-1", "lead", "done with task 42");
    let msg_id = inbox::deliver_to_inbox(&root, &msg).unwrap();
    inbox::mark_delivered(&root, "lead", &msg_id).unwrap();

    // First fire: should work.
    backdate_idle_grace(&mut daemon, "lead");
    daemon.maybe_intervene_triage_backlog().unwrap();
    let pending = inbox::pending_messages(&root, "lead").unwrap();
    assert_eq!(pending.len(), 1, "first triage intervention should fire");

    // Acknowledge so inbox is clear.
    for p in pending {
        inbox::mark_delivered(&root, "lead", &p.id).unwrap();
    }

    // Advance epoch (Working → Idle transition).
    daemon.update_automation_timers_for_state("lead", MemberState::Working);
    daemon.update_automation_timers_for_state("lead", MemberState::Idle);
    backdate_idle_grace(&mut daemon, "lead");

    // New epoch should normally allow refire, but cooldown blocks it.
    daemon.maybe_intervene_triage_backlog().unwrap();
    let pending = inbox::pending_messages(&root, "lead").unwrap();
    assert_eq!(pending.len(), 0, "cooldown should prevent triage refire");

    // Expire cooldown — should fire.
    backdate_intervention_cooldown(&mut daemon, "triage::lead");
    daemon.maybe_intervene_triage_backlog().unwrap();
    let pending = inbox::pending_messages(&root, "lead").unwrap();
    assert_eq!(
        pending.len(),
        1,
        "triage should fire after cooldown expires"
    );
}

#[test]
fn maybe_intervene_owned_tasks_escalates_stuck_signature_to_parent() {
    let tmp = tempfile::tempdir().unwrap();
    let events_path = tmp.path().join("events.jsonl");
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![
            manager_member("lead", Some("architect")),
            engineer_member("eng-1", Some("lead"), false),
        ])
        .pane_map(HashMap::from([("eng-1".to_string(), "%999".to_string())]))
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .workflow_policy(WorkflowPolicy {
            escalation_threshold_secs: 120,
            ..WorkflowPolicy::default()
        })
        .build();
    daemon.event_sink = EventSink::new(&events_path).unwrap();

    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "eng-1").unwrap();
    inbox::init_inbox(&root, "lead").unwrap();
    write_owned_task_file(tmp.path(), 191, "owned-task", "in-progress", "eng-1");

    daemon.update_automation_timers_for_state("eng-1", MemberState::Working);
    daemon.update_automation_timers_for_state("eng-1", MemberState::Idle);
    backdate_idle_grace(&mut daemon, "eng-1");
    daemon.maybe_intervene_owned_tasks().unwrap();

    let state = daemon.owned_task_interventions.get_mut("eng-1").unwrap();
    state.detected_at = Instant::now() - Duration::from_secs(121);

    daemon.maybe_intervene_owned_tasks().unwrap();

    let engineer_pending = inbox::pending_messages(&root, "eng-1").unwrap();
    assert_eq!(engineer_pending.len(), 1);
    let lead_pending = inbox::pending_messages(&root, "lead").unwrap();
    assert_eq!(lead_pending.len(), 1);
    assert_eq!(lead_pending[0].from, "daemon");
    assert!(lead_pending[0].body.contains("Stuck task escalation"));
    assert!(lead_pending[0].body.contains("eng-1"));
    assert!(lead_pending[0].body.contains("Task #191"));
    assert!(lead_pending[0].body.contains("kanban-md edit --dir"));
    assert!(lead_pending[0].body.contains("batty assign eng-1"));
    assert!(
        daemon
            .owned_task_interventions
            .get("eng-1")
            .is_some_and(|state| state.escalation_sent)
    );

    let events = super::super::events::read_events(&events_path).unwrap();
    assert!(
        events.iter().any(|event| {
            event.event == "task_escalated"
                && event.role.as_deref() == Some("eng-1")
                && event.task.as_deref() == Some("191")
        }),
        "expected task_escalated event for stuck owned task"
    );
}

#[test]
fn maybe_intervene_owned_tasks_only_escalates_stuck_signature_once() {
    let tmp = tempfile::tempdir().unwrap();
    let events_path = tmp.path().join("events.jsonl");
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![
            manager_member("lead", Some("architect")),
            engineer_member("eng-1", Some("lead"), false),
        ])
        .pane_map(HashMap::from([("eng-1".to_string(), "%999".to_string())]))
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .workflow_policy(WorkflowPolicy {
            escalation_threshold_secs: 120,
            ..WorkflowPolicy::default()
        })
        .build();
    daemon.event_sink = EventSink::new(&events_path).unwrap();

    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "eng-1").unwrap();
    inbox::init_inbox(&root, "lead").unwrap();
    write_owned_task_file(tmp.path(), 191, "owned-task", "in-progress", "eng-1");

    daemon.update_automation_timers_for_state("eng-1", MemberState::Working);
    daemon.update_automation_timers_for_state("eng-1", MemberState::Idle);
    backdate_idle_grace(&mut daemon, "eng-1");
    daemon.maybe_intervene_owned_tasks().unwrap();

    let state = daemon.owned_task_interventions.get_mut("eng-1").unwrap();
    state.detected_at = Instant::now() - Duration::from_secs(121);

    daemon.maybe_intervene_owned_tasks().unwrap();
    daemon.maybe_intervene_owned_tasks().unwrap();

    let lead_pending = inbox::pending_messages(&root, "lead").unwrap();
    assert_eq!(lead_pending.len(), 1);
    assert!(lead_pending[0].body.contains("Stuck task escalation"));
    assert!(
        daemon
            .owned_task_interventions
            .get("eng-1")
            .is_some_and(|state| state.escalation_sent)
    );

    let events = super::super::events::read_events(&events_path).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| {
                event.event == "task_escalated"
                    && event.role.as_deref() == Some("eng-1")
                    && event.task.as_deref() == Some("191")
            })
            .count(),
        1
    );
}

#[test]
fn maybe_intervene_owned_tasks_waits_for_escalation_threshold() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![
            manager_member("lead", Some("architect")),
            engineer_member("eng-1", Some("lead"), false),
        ])
        .pane_map(HashMap::from([("eng-1".to_string(), "%999".to_string())]))
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .workflow_policy(WorkflowPolicy {
            escalation_threshold_secs: 120,
            ..WorkflowPolicy::default()
        })
        .build();

    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "eng-1").unwrap();
    inbox::init_inbox(&root, "lead").unwrap();
    write_owned_task_file(tmp.path(), 191, "owned-task", "in-progress", "eng-1");

    daemon.update_automation_timers_for_state("eng-1", MemberState::Working);
    daemon.update_automation_timers_for_state("eng-1", MemberState::Idle);
    backdate_idle_grace(&mut daemon, "eng-1");
    daemon.maybe_intervene_owned_tasks().unwrap();

    let state = daemon.owned_task_interventions.get_mut("eng-1").unwrap();
    state.detected_at = Instant::now() - Duration::from_secs(119);

    daemon.maybe_intervene_owned_tasks().unwrap();

    assert!(inbox::pending_messages(&root, "lead").unwrap().is_empty());
    assert!(
        daemon
            .owned_task_interventions
            .get("eng-1")
            .is_some_and(|state| !state.escalation_sent)
    );
}

#[test]
fn maybe_intervene_review_backlog_queues_for_idle_manager_with_branch_and_worktree_context() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-daemon-test");
    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
    write_owned_task_file(&repo, 191, "review-task", "review", "eng-1");

    let mut daemon = TestDaemonBuilder::new(&repo)
        .members(vec![
            manager_member("lead", Some("architect")),
            engineer_member("eng-1", Some("lead"), true),
        ])
        .pane_map(HashMap::from([("lead".to_string(), "%999".to_string())]))
        .states(HashMap::from([("lead".to_string(), MemberState::Idle)]))
        .build();

    let root = inbox::inboxes_root(&repo);
    inbox::init_inbox(&root, "lead").unwrap();

    daemon.update_automation_timers_for_state("lead", MemberState::Working);
    daemon.update_automation_timers_for_state("lead", MemberState::Idle);
    backdate_idle_grace(&mut daemon, "lead");
    daemon.maybe_intervene_review_backlog().unwrap();

    let pending = inbox::pending_messages(&root, "lead").unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].from, "architect");
    assert!(pending[0].body.contains("Review backlog detected"));
    assert!(pending[0].body.contains("#191 by eng-1"));
    assert!(pending[0].body.contains("batty inbox lead"));
    assert!(pending[0].body.contains("batty read lead <ref>"));
    assert!(pending[0].body.contains("batty merge eng-1"));
    assert!(pending[0].body.contains("kanban-md move --dir"));
    assert!(pending[0].body.contains("191 done"));
    assert!(pending[0].body.contains("191 archived"));
    assert!(pending[0].body.contains("191 in-progress"));
    assert!(pending[0].body.contains("batty assign eng-1"));
    assert!(pending[0].body.contains("batty send architect"));
    assert!(
        pending[0]
            .body
            .contains(worktree_dir.to_string_lossy().as_ref())
    );
    assert!(pending[0].body.contains("branch: eng-1"));
    assert_eq!(
        daemon
            .owned_task_interventions
            .get("review::lead")
            .map(|state| state.idle_epoch),
        Some(1)
    );
}

#[test]
fn maybe_intervene_review_backlog_does_not_fire_on_startup_idle() {
    let tmp = tempfile::tempdir().unwrap();
    write_owned_task_file(tmp.path(), 191, "review-task", "review", "eng-1");

    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![
            manager_member("lead", Some("architect")),
            engineer_member("eng-1", Some("lead"), false),
        ])
        .pane_map(HashMap::from([("lead".to_string(), "%999".to_string())]))
        .states(HashMap::from([("lead".to_string(), MemberState::Idle)]))
        .build();

    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "lead").unwrap();

    daemon.maybe_intervene_review_backlog().unwrap();

    assert!(inbox::pending_messages(&root, "lead").unwrap().is_empty());
    assert!(!daemon.owned_task_interventions.contains_key("review::lead"));
}

#[test]
fn maybe_intervene_manager_dispatch_gap_queues_for_idle_lead_with_idle_reports() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![
            architect_member("architect"),
            manager_member("lead", Some("architect")),
            engineer_member("eng-1", Some("lead"), false),
            engineer_member("eng-2", Some("lead"), false),
        ])
        .pane_map(HashMap::from([("lead".to_string(), "%999".to_string())]))
        .states(HashMap::from([
            ("lead".to_string(), MemberState::Idle),
            ("eng-1".to_string(), MemberState::Idle),
            ("eng-2".to_string(), MemberState::Idle),
        ]))
        .build();

    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "lead").unwrap();
    inbox::init_inbox(&root, "eng-1").unwrap();
    inbox::init_inbox(&root, "eng-2").unwrap();
    write_owned_task_file(tmp.path(), 191, "active-task", "in-progress", "eng-1");
    let tasks_dir = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    std::fs::write(
            tasks_dir.join("192-open-task.md"),
            "---\nid: 192\ntitle: open-task\nstatus: todo\npriority: high\nclass: standard\n---\n\nTask description.\n",
        )
        .unwrap();

    backdate_idle_grace(&mut daemon, "lead");
    daemon.maybe_intervene_manager_dispatch_gap().unwrap();

    let pending = inbox::pending_messages(&root, "lead").unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].from, "architect");
    assert!(pending[0].body.contains("Dispatch recovery needed"));
    assert!(pending[0].body.contains("eng-1 on #191"));
    assert!(pending[0].body.contains("eng-2"));
    assert!(pending[0].body.contains("batty status"));
    assert!(pending[0].body.contains("batty send eng-1"));
    assert!(pending[0].body.contains("batty assign eng-2"));
    assert!(pending[0].body.contains("batty send architect"));
    assert!(
        daemon
            .owned_task_interventions
            .contains_key("dispatch::lead")
    );
}

#[test]
fn maybe_intervene_architect_utilization_queues_for_underloaded_idle_architect() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![
            architect_member("architect"),
            manager_member("lead", Some("architect")),
            engineer_member("eng-1", Some("lead"), false),
            engineer_member("eng-2", Some("lead"), false),
        ])
        .pane_map(HashMap::from([(
            "architect".to_string(),
            "%999".to_string(),
        )]))
        .states(HashMap::from([
            ("architect".to_string(), MemberState::Idle),
            ("lead".to_string(), MemberState::Idle),
            ("eng-1".to_string(), MemberState::Idle),
            ("eng-2".to_string(), MemberState::Idle),
        ]))
        .build();

    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "architect").unwrap();
    write_owned_task_file(tmp.path(), 191, "active-task", "in-progress", "eng-1");
    let tasks_dir = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    std::fs::write(
            tasks_dir.join("192-open-task.md"),
            "---\nid: 192\ntitle: open-task\nstatus: backlog\npriority: high\nclass: standard\n---\n\nTask description.\n",
        )
        .unwrap();

    backdate_idle_grace(&mut daemon, "architect");
    daemon.maybe_intervene_architect_utilization().unwrap();

    let pending = inbox::pending_messages(&root, "architect").unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].from, "daemon");
    assert!(pending[0].body.contains("Utilization recovery needed"));
    assert!(pending[0].body.contains("eng-1 on #191"));
    assert!(pending[0].body.contains("eng-2"));
    assert!(pending[0].body.contains("batty status"));
    assert!(pending[0].body.contains("batty send lead"));
    assert!(pending[0].body.contains("Start Task #192 on eng-2"));
    assert!(
        daemon
            .owned_task_interventions
            .contains_key("utilization::architect")
    );
}

#[test]
fn zero_engineers_topology_skips_executor_interventions() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![
            architect_member("architect"),
            manager_member("lead", Some("architect")),
        ])
        .pane_map(HashMap::from([
            ("architect".to_string(), "%998".to_string()),
            ("lead".to_string(), "%999".to_string()),
        ]))
        .states(HashMap::from([
            ("architect".to_string(), MemberState::Idle),
            ("lead".to_string(), MemberState::Idle),
        ]))
        .build();

    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "architect").unwrap();
    inbox::init_inbox(&root, "lead").unwrap();
    write_open_task_file(tmp.path(), 191, "queued-task", "todo");

    backdate_idle_grace(&mut daemon, "architect");
    backdate_idle_grace(&mut daemon, "lead");
    daemon.maybe_intervene_manager_dispatch_gap().unwrap();
    daemon.maybe_intervene_architect_utilization().unwrap();

    assert!(
        inbox::pending_messages(&root, "architect")
            .unwrap()
            .is_empty()
    );
    assert!(inbox::pending_messages(&root, "lead").unwrap().is_empty());
    assert!(
        !daemon
            .owned_task_interventions
            .contains_key("dispatch::lead")
    );
    assert!(
        !daemon
            .owned_task_interventions
            .contains_key("utilization::architect")
    );
}

#[test]
fn single_role_topology_nudges_idle_member() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![architect_member("solo")])
        .pane_map(HashMap::from([("solo".to_string(), "%999".to_string())]))
        .states(HashMap::from([("solo".to_string(), MemberState::Idle)]))
        .nudges(HashMap::from([(
            "solo".to_string(),
            NudgeSchedule {
                text: "Solo mode should keep moving.".to_string(),
                interval: Duration::from_secs(1),
                idle_since: Some(Instant::now() - Duration::from_secs(5)),
                fired_this_idle: false,
                paused: false,
            },
        )]))
        .build();

    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "solo").unwrap();

    backdate_idle_grace(&mut daemon, "solo");
    daemon.maybe_fire_nudges().unwrap();

    let pending = inbox::pending_messages(&root, "solo").unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].from, "daemon");
    assert!(pending[0].body.contains("Solo mode should keep moving."));
    assert!(pending[0].body.contains("Idle nudge:"));
    assert_eq!(daemon.states.get("solo"), Some(&MemberState::Idle));
    assert!(
        daemon
            .nudges
            .get("solo")
            .is_some_and(|schedule| schedule.fired_this_idle)
    );
}

#[test]
fn all_members_working_suppresses_interventions() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![
            architect_member("architect"),
            manager_member("lead", Some("architect")),
            engineer_member("eng-1", Some("lead"), false),
            engineer_member("eng-2", Some("lead"), false),
        ])
        .pane_map(HashMap::from([
            ("architect".to_string(), "%997".to_string()),
            ("lead".to_string(), "%998".to_string()),
            ("eng-1".to_string(), "%999".to_string()),
            ("eng-2".to_string(), "%996".to_string()),
        ]))
        .states(HashMap::from([
            ("architect".to_string(), MemberState::Working),
            ("lead".to_string(), MemberState::Working),
            ("eng-1".to_string(), MemberState::Working),
            ("eng-2".to_string(), MemberState::Working),
        ]))
        .build();

    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "architect").unwrap();
    inbox::init_inbox(&root, "lead").unwrap();
    inbox::init_inbox(&root, "eng-1").unwrap();
    inbox::init_inbox(&root, "eng-2").unwrap();

    let mut triage_message = inbox::InboxMessage::new_send("eng-1", "lead", "Task complete.");
    triage_message.timestamp = super::now_unix();
    let triage_id = inbox::deliver_to_inbox(&root, &triage_message).unwrap();
    inbox::mark_delivered(&root, "lead", &triage_id).unwrap();

    write_owned_task_file(tmp.path(), 191, "owned-task", "in-progress", "eng-1");
    write_owned_task_file(tmp.path(), 192, "review-task", "review", "eng-2");
    write_open_task_file(tmp.path(), 193, "open-task", "todo");

    daemon.maybe_intervene_triage_backlog().unwrap();
    daemon.maybe_intervene_owned_tasks().unwrap();
    daemon.maybe_intervene_review_backlog().unwrap();
    daemon.maybe_intervene_manager_dispatch_gap().unwrap();
    daemon.maybe_intervene_architect_utilization().unwrap();

    assert!(
        inbox::pending_messages(&root, "architect")
            .unwrap()
            .is_empty()
    );
    assert!(inbox::pending_messages(&root, "lead").unwrap().is_empty());
    assert!(inbox::pending_messages(&root, "eng-1").unwrap().is_empty());
    assert!(inbox::pending_messages(&root, "eng-2").unwrap().is_empty());
    assert!(daemon.triage_interventions.is_empty());
    assert!(daemon.owned_task_interventions.is_empty());
}

#[test]
fn manager_dispatch_gap_skips_when_pending_inbox_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![
            architect_member("architect"),
            manager_member("lead", Some("architect")),
            engineer_member("eng-1", Some("lead"), false),
            engineer_member("eng-2", Some("lead"), false),
        ])
        .pane_map(HashMap::from([("lead".to_string(), "%999".to_string())]))
        .states(HashMap::from([
            ("lead".to_string(), MemberState::Idle),
            ("eng-1".to_string(), MemberState::Idle),
            ("eng-2".to_string(), MemberState::Idle),
        ]))
        .build();

    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "lead").unwrap();
    inbox::init_inbox(&root, "eng-1").unwrap();
    inbox::init_inbox(&root, "eng-2").unwrap();
    let message = inbox::InboxMessage::new_send("architect", "lead", "Handle this first.");
    inbox::deliver_to_inbox(&root, &message).unwrap();

    write_owned_task_file(tmp.path(), 191, "active-task", "in-progress", "eng-1");
    write_open_task_file(tmp.path(), 192, "open-task", "todo");

    backdate_idle_grace(&mut daemon, "lead");
    daemon.maybe_intervene_manager_dispatch_gap().unwrap();

    let pending = inbox::pending_messages(&root, "lead").unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].from, "architect");
    assert!(
        !daemon
            .owned_task_interventions
            .contains_key("dispatch::lead")
    );
}

#[test]
fn owned_task_intervention_refires_at_exact_cooldown_boundary() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![manager_member("lead", Some("architect"))])
        .pane_map(HashMap::from([("lead".to_string(), "%999".to_string())]))
        .states(HashMap::from([("lead".to_string(), MemberState::Idle)]))
        .build();

    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "lead").unwrap();
    write_owned_task_file(tmp.path(), 191, "owned-task", "in-progress", "lead");

    let cooldown = Duration::from_secs(
        daemon
            .config
            .team_config
            .automation
            .intervention_cooldown_secs,
    );

    backdate_idle_grace(&mut daemon, "lead");
    daemon.intervention_cooldowns.insert(
        "lead".to_string(),
        Instant::now() - (cooldown - Duration::from_secs(1)),
    );
    daemon.maybe_intervene_owned_tasks().unwrap();
    assert!(inbox::pending_messages(&root, "lead").unwrap().is_empty());

    daemon
        .intervention_cooldowns
        .insert("lead".to_string(), Instant::now() - cooldown);
    daemon.maybe_intervene_owned_tasks().unwrap();

    let pending = inbox::pending_messages(&root, "lead").unwrap();
    assert_eq!(pending.len(), 1);
    assert!(
        pending[0]
            .body
            .contains("Owned active task backlog detected")
    );
    assert!(daemon.owned_task_interventions.contains_key("lead"));
}

#[test]
fn empty_board_skips_interventions() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![
            architect_member("architect"),
            manager_member("lead", Some("architect")),
            engineer_member("eng-1", Some("lead"), false),
        ])
        .pane_map(HashMap::from([
            ("architect".to_string(), "%997".to_string()),
            ("lead".to_string(), "%998".to_string()),
            ("eng-1".to_string(), "%999".to_string()),
        ]))
        .states(HashMap::from([
            ("architect".to_string(), MemberState::Idle),
            ("lead".to_string(), MemberState::Idle),
            ("eng-1".to_string(), MemberState::Idle),
        ]))
        .build();

    std::fs::create_dir_all(
        tmp.path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks"),
    )
    .unwrap();
    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "architect").unwrap();
    inbox::init_inbox(&root, "lead").unwrap();
    inbox::init_inbox(&root, "eng-1").unwrap();

    backdate_idle_grace(&mut daemon, "architect");
    backdate_idle_grace(&mut daemon, "lead");
    backdate_idle_grace(&mut daemon, "eng-1");

    daemon.maybe_intervene_triage_backlog().unwrap();
    daemon.maybe_intervene_owned_tasks().unwrap();
    daemon.maybe_intervene_review_backlog().unwrap();
    daemon.maybe_intervene_manager_dispatch_gap().unwrap();
    daemon.maybe_intervene_architect_utilization().unwrap();

    assert!(
        inbox::pending_messages(&root, "architect")
            .unwrap()
            .is_empty()
    );
    assert!(inbox::pending_messages(&root, "lead").unwrap().is_empty());
    assert!(inbox::pending_messages(&root, "eng-1").unwrap().is_empty());
    assert!(daemon.triage_interventions.is_empty());
    assert!(daemon.owned_task_interventions.is_empty());
}

#[test]
fn test_starvation_detected() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = starvation_test_daemon(&tmp, Some(1));
    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "architect").unwrap();
    write_open_task_file(tmp.path(), 101, "queued-task", "todo");

    daemon.maybe_detect_pipeline_starvation().unwrap();

    let pending = inbox::pending_messages(&root, "architect").unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].from, "daemon");
    assert_eq!(
        pending[0].body,
        "Pipeline running dry: 2 idle engineers, 1 todo tasks."
    );
    assert!(daemon.pipeline_starvation_fired);
}

#[test]
fn test_debounce() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = starvation_test_daemon(&tmp, Some(1));
    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "architect").unwrap();
    write_open_task_file(tmp.path(), 101, "queued-task", "todo");

    daemon.maybe_detect_pipeline_starvation().unwrap();
    daemon.maybe_detect_pipeline_starvation().unwrap();

    let pending = inbox::pending_messages(&root, "architect").unwrap();
    assert_eq!(pending.len(), 1);
    assert!(daemon.pipeline_starvation_fired);
}

#[test]
fn test_threshold_config() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = starvation_test_daemon(&tmp, Some(2));
    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "architect").unwrap();
    write_open_task_file(tmp.path(), 101, "queued-task", "todo");

    daemon.maybe_detect_pipeline_starvation().unwrap();
    assert!(
        inbox::pending_messages(&root, "architect")
            .unwrap()
            .is_empty()
    );
    assert!(!daemon.pipeline_starvation_fired);

    let disabled_tmp = tempfile::tempdir().unwrap();
    let mut disabled_daemon = starvation_test_daemon(&disabled_tmp, None);
    let disabled_root = inbox::inboxes_root(disabled_tmp.path());
    inbox::init_inbox(&disabled_root, "architect").unwrap();
    write_open_task_file(disabled_tmp.path(), 101, "queued-task", "todo");

    disabled_daemon.maybe_detect_pipeline_starvation().unwrap();
    assert!(
        inbox::pending_messages(&disabled_root, "architect")
            .unwrap()
            .is_empty()
    );
    assert!(!disabled_daemon.pipeline_starvation_fired);
}

#[test]
fn test_reset_when_work_added() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = starvation_test_daemon(&tmp, Some(1));
    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "architect").unwrap();
    write_open_task_file(tmp.path(), 101, "queued-task", "todo");

    daemon.maybe_detect_pipeline_starvation().unwrap();
    assert!(daemon.pipeline_starvation_fired);

    // Parity (2 tasks == 2 engineers) does NOT clear — need surplus to reset
    write_open_task_file(tmp.path(), 102, "queued-task-2", "backlog");
    daemon.maybe_detect_pipeline_starvation().unwrap();
    assert!(daemon.pipeline_starvation_fired);

    // Surplus (3 tasks > 2 engineers) clears the flag
    write_open_task_file(tmp.path(), 103, "queued-task-3", "backlog");
    daemon.maybe_detect_pipeline_starvation().unwrap();
    assert!(!daemon.pipeline_starvation_fired);

    // Remove surplus — back to 1 task for 2 engineers, starvation re-fires after cooldown
    std::fs::remove_file(
        tmp.path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks")
            .join("102-queued-task-2.md"),
    )
    .unwrap();
    std::fs::remove_file(
        tmp.path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks")
            .join("103-queued-task-3.md"),
    )
    .unwrap();
    daemon.pipeline_starvation_last_fired = Some(Instant::now() - Duration::from_secs(301));
    daemon.maybe_detect_pipeline_starvation().unwrap();

    let pending = inbox::pending_messages(&root, "architect").unwrap();
    assert_eq!(pending.len(), 2);
    assert!(daemon.pipeline_starvation_fired);
}

#[test]
fn starvation_suppressed_when_engineer_has_active_board_item() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = starvation_test_daemon(&tmp, Some(1));
    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "architect").unwrap();

    // Create one unclaimed todo task and one in-review task claimed by eng-1
    write_open_task_file(tmp.path(), 101, "queued-task", "todo");
    write_board_task_file(
        tmp.path(),
        102,
        "review-task",
        "review",
        Some("eng-1"),
        &[],
        None,
    );

    daemon.maybe_detect_pipeline_starvation().unwrap();

    // eng-1 has an active board item (review), so only eng-2 is truly idle.
    // 1 idle engineer, 1 unclaimed todo task => no deficit => no alert
    let pending = inbox::pending_messages(&root, "architect").unwrap();
    assert!(pending.is_empty());
    assert!(!daemon.pipeline_starvation_fired);
}

#[test]
fn starvation_suppressed_when_manager_working() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![
            architect_member("architect"),
            manager_member("lead", Some("architect")),
            engineer_member("eng-1", Some("lead"), false),
            engineer_member("eng-2", Some("lead"), false),
        ])
        .workflow_policy(WorkflowPolicy {
            pipeline_starvation_threshold: Some(1),
            ..WorkflowPolicy::default()
        })
        .build();
    daemon.states = HashMap::from([
        ("lead".to_string(), MemberState::Working),
        ("eng-1".to_string(), MemberState::Idle),
        ("eng-2".to_string(), MemberState::Idle),
    ]);
    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "architect").unwrap();
    write_open_task_file(tmp.path(), 101, "queued-task", "todo");

    daemon.maybe_detect_pipeline_starvation().unwrap();

    // Manager is working, so starvation alert should be suppressed
    let pending = inbox::pending_messages(&root, "architect").unwrap();
    assert!(pending.is_empty());
}

#[test]
fn maybe_intervene_triage_backlog_does_not_refire_while_prior_intervention_remains_pending() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![
            manager_member("lead", Some("architect")),
            engineer_member("eng-1", Some("lead"), false),
        ])
        .pane_map(HashMap::from([("lead".to_string(), "%999".to_string())]))
        .states(HashMap::from([("lead".to_string(), MemberState::Idle)]))
        .build();

    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "lead").unwrap();
    inbox::init_inbox(&root, "eng-1").unwrap();
    let mut result = inbox::InboxMessage::new_send("eng-1", "lead", "Task complete.");
    result.timestamp = super::now_unix();
    let id = inbox::deliver_to_inbox(&root, &result).unwrap();
    inbox::mark_delivered(&root, "lead", &id).unwrap();

    daemon.update_automation_timers_for_state("lead", MemberState::Working);
    daemon.update_automation_timers_for_state("lead", MemberState::Idle);
    backdate_idle_grace(&mut daemon, "lead");
    daemon.maybe_intervene_triage_backlog().unwrap();

    daemon
        .states
        .insert("lead".to_string(), MemberState::Working);
    daemon.update_automation_timers_for_state("lead", MemberState::Working);
    daemon.states.insert("lead".to_string(), MemberState::Idle);
    daemon.update_automation_timers_for_state("lead", MemberState::Idle);
    backdate_idle_grace(&mut daemon, "lead");
    daemon.maybe_intervene_triage_backlog().unwrap();

    assert_eq!(daemon.triage_interventions.get("lead"), Some(&1));
    let pending = inbox::pending_messages(&root, "lead").unwrap();
    assert_eq!(pending.len(), 1);
    assert!(pending.iter().all(|message| message.from == "architect"));
}

#[test]
fn maybe_fire_nudges_keeps_member_idle_when_delivery_falls_back_to_inbox() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![architect_member("scientist")])
        .pane_map(HashMap::from([(
            "scientist".to_string(),
            "%999".to_string(),
        )]))
        .states(HashMap::from([(
            "scientist".to_string(),
            MemberState::Idle,
        )]))
        .nudges(HashMap::from([(
            "scientist".to_string(),
            NudgeSchedule {
                text: "Please make progress.".to_string(),
                interval: Duration::from_secs(1),
                idle_since: Some(Instant::now() - Duration::from_secs(5)),
                fired_this_idle: false,
                paused: false,
            },
        )]))
        .build();

    backdate_idle_grace(&mut daemon, "scientist");
    daemon.maybe_fire_nudges().unwrap();

    assert_eq!(daemon.states.get("scientist"), Some(&MemberState::Idle));
    let schedule = daemon.nudges.get("scientist").unwrap();
    assert!(!schedule.paused);
    assert!(schedule.fired_this_idle);

    let messages = inbox::pending_messages(&inbox::inboxes_root(tmp.path()), "scientist").unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].from, "daemon");
    assert!(messages[0].body.contains("Please make progress."));
    assert!(messages[0].body.contains("Idle nudge:"));
}

#[test]
fn maybe_fire_nudges_skips_when_pending_inbox_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![architect_member("scientist")])
        .pane_map(HashMap::from([(
            "scientist".to_string(),
            "%999".to_string(),
        )]))
        .states(HashMap::from([(
            "scientist".to_string(),
            MemberState::Idle,
        )]))
        .nudges(HashMap::from([(
            "scientist".to_string(),
            NudgeSchedule {
                text: "Please make progress.".to_string(),
                interval: Duration::from_secs(1),
                idle_since: Some(Instant::now()),
                fired_this_idle: false,
                paused: false,
            },
        )]))
        .build();

    let root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&root, "scientist").unwrap();
    let message = inbox::InboxMessage::new_send("architect", "scientist", "Process this first.");
    inbox::deliver_to_inbox(&root, &message).unwrap();

    backdate_idle_grace(&mut daemon, "scientist");
    daemon.maybe_fire_nudges().unwrap();

    let messages = inbox::pending_messages(&root, "scientist").unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].from, "architect");
    let schedule = daemon.nudges.get("scientist").unwrap();
    assert!(!schedule.fired_this_idle);
    assert_eq!(daemon.states.get("scientist"), Some(&MemberState::Idle));
}

#[test]
fn automation_sender_prefers_direct_manager_and_config_fallback() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![
            architect_member("architect"),
            manager_member("lead", Some("architect")),
            engineer_member("eng-1", Some("lead"), false),
        ])
        .build();
    daemon.config.team_config.automation_sender = Some("human".to_string());

    assert_eq!(daemon.automation_sender_for("eng-1"), "lead");
    assert_eq!(daemon.automation_sender_for("lead"), "architect");
    assert_eq!(daemon.automation_sender_for("architect"), "human");

    daemon.config.team_config.automation_sender = None;
    assert_eq!(daemon.automation_sender_for("architect"), "daemon");
}

#[test]
fn hot_reload_marker_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let marker = hot_reload_marker_path(tmp.path());

    write_hot_reload_marker(tmp.path()).unwrap();
    assert!(marker.exists());
    assert!(consume_hot_reload_marker(tmp.path()));
    assert!(!marker.exists());
    assert!(!consume_hot_reload_marker(tmp.path()));
}

#[test]
fn hot_reload_resume_args_include_resume_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let args = hot_reload_daemon_args(tmp.path());
    let canonical_root = tmp.path().canonicalize().unwrap();
    assert_eq!(
        args,
        vec![
            "-v".to_string(),
            "daemon".to_string(),
            "--project-root".to_string(),
            canonical_root.to_string_lossy().to_string(),
            "--resume".to_string(),
        ]
    );
}

#[test]
fn hot_reload_fingerprint_detects_binary_change() {
    let tmp = tempfile::tempdir().unwrap();
    let binary = tmp.path().join("batty");
    fs::write(&binary, "old-binary").unwrap();
    let before = BinaryFingerprint::capture(&binary).unwrap();

    std::thread::sleep(Duration::from_millis(1100));
    fs::write(&binary, "new-binary-build").unwrap();
    let after = BinaryFingerprint::capture(&binary).unwrap();

    assert!(after.changed_from(&before));
}

#[test]
fn resume_decision_logged_to_orchestrator() {
    let tmp = tempfile::tempdir().unwrap();
    let member = MemberInstance {
        name: "architect".to_string(),
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
        session: "test".to_string(),
        members: vec![member],
        pane_map: HashMap::from([("architect".to_string(), "%999".to_string())]),
    })
    .unwrap();

    daemon.spawn_all_agents(false).unwrap();

    let content = fs::read_to_string(tmp.path().join(".batty").join("orchestrator.log")).unwrap();
    assert!(content.contains("resume: architect=no (resume disabled)"));
}

#[test]
fn reconcile_clears_done_task() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
    write_owned_task_file(tmp.path(), 42, "finished-work", "done", "eng-1");
    let mut daemon = make_test_daemon(
        tmp.path(),
        vec![engineer_member("eng-1", Some("manager"), false)],
    );
    daemon.active_tasks.insert("eng-1".to_string(), 42);

    daemon.reconcile_active_tasks().unwrap();

    assert_eq!(daemon.active_task_id("eng-1"), None);
}

#[test]
fn reconcile_keeps_in_progress_task() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
    write_owned_task_file(tmp.path(), 42, "active-work", "in-progress", "eng-1");
    let mut daemon = make_test_daemon(
        tmp.path(),
        vec![engineer_member("eng-1", Some("manager"), false)],
    );
    daemon.active_tasks.insert("eng-1".to_string(), 42);

    daemon.reconcile_active_tasks().unwrap();

    assert_eq!(daemon.active_task_id("eng-1"), Some(42));
}

#[test]
fn spawn_all_agents_resume_reports_missing_sessions_across_primary_roles() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
    let previous_launch_state = HashMap::from([
        (
            "architect".to_string(),
            super::launcher::LaunchIdentity {
                agent: "claude-code".to_string(),
                prompt: String::new(),
                session_id: Some("missing-architect-session".to_string()),
            },
        ),
        (
            "manager".to_string(),
            super::launcher::LaunchIdentity {
                agent: "claude-code".to_string(),
                prompt: String::new(),
                session_id: Some("missing-manager-session".to_string()),
            },
        ),
        (
            "eng-1".to_string(),
            super::launcher::LaunchIdentity {
                agent: "codex-cli".to_string(),
                prompt: String::new(),
                session_id: Some("missing-engineer-session".to_string()),
            },
        ),
    ]);

    let daemon = TeamDaemon::new(DaemonConfig {
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
        session: "test".to_string(),
        members: vec![
            architect_member("architect"),
            manager_member("manager", Some("architect")),
            engineer_member("eng-1", Some("manager"), false),
        ],
        pane_map: HashMap::from([
            ("architect".to_string(), "%901".to_string()),
            ("manager".to_string(), "%902".to_string()),
            ("eng-1".to_string(), "%903".to_string()),
        ]),
    })
    .unwrap();

    let duplicate_claude_session_ids =
        super::launcher::duplicate_claude_session_ids(&previous_launch_state);
    let architect_plan = daemon
        .prepare_member_launch(
            &architect_member("architect"),
            true,
            &previous_launch_state,
            &duplicate_claude_session_ids,
        )
        .unwrap();
    let manager_plan = daemon
        .prepare_member_launch(
            &manager_member("manager", Some("architect")),
            true,
            &previous_launch_state,
            &duplicate_claude_session_ids,
        )
        .unwrap();
    let engineer_plan = daemon
        .prepare_member_launch(
            &engineer_member("eng-1", Some("manager"), false),
            true,
            &previous_launch_state,
            &duplicate_claude_session_ids,
        )
        .unwrap();

    assert_eq!(
        architect_plan.resume_summary,
        "architect=no (prompt changed)"
    );
    assert_eq!(manager_plan.resume_summary, "manager=no (prompt changed)");
    assert_eq!(engineer_plan.resume_summary, "eng-1=no (prompt changed)");
    assert_eq!(architect_plan.identity.agent, "claude-code");
    assert_eq!(manager_plan.identity.agent, "claude-code");
    assert_eq!(engineer_plan.identity.agent, "codex-cli");
}

#[test]
fn reconcile_clears_missing_task() {
    let tmp = tempfile::tempdir().unwrap();
    let tasks_dir = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    // No task file for ID 99 — it doesn't exist on the board
    let mut daemon = make_test_daemon(
        tmp.path(),
        vec![engineer_member("eng-1", Some("manager"), false)],
    );
    daemon.active_tasks.insert("eng-1".to_string(), 99);

    daemon.reconcile_active_tasks().unwrap();

    assert_eq!(daemon.active_task_id("eng-1"), None);
}

#[test]
fn production_daemon_has_no_unwrap_or_expect_calls() {
    // file!() resolves to this test file; check the actual daemon.rs instead.
    let daemon_path = Path::new(file!()).parent().unwrap().join("../daemon.rs");
    let count = production_unwrap_expect_count(&daemon_path);
    assert_eq!(count, 0, "production daemon.rs should avoid unwrap/expect");
}

#[test]
fn non_git_repo_disables_worktrees() {
    use crate::team::harness::TestHarness;
    use crate::team::test_support::engineer_member;

    let harness = TestHarness::new()
        .with_member(engineer_member("eng-1", Some("manager"), true))
        .with_member_state("eng-1", MemberState::Idle);
    let daemon = harness.build_daemon().unwrap();

    // Test harness temp dir is not a git repo
    assert!(!daemon.is_git_repo);
    // member_uses_worktrees should return false even though the member config has use_worktrees=true
    assert!(
        !daemon.member_uses_worktrees("eng-1"),
        "worktrees should be disabled when project is not a git repo"
    );
}

#[test]
fn git_repo_enables_worktrees() {
    use crate::team::harness::TestHarness;
    use crate::team::test_support::engineer_member;

    let harness = TestHarness::new()
        .with_member(engineer_member("eng-1", Some("manager"), true))
        .with_member_state("eng-1", MemberState::Idle);
    let mut daemon = harness.build_daemon().unwrap();

    // Simulate being in a git repo
    daemon.is_git_repo = true;

    assert!(
        daemon.member_uses_worktrees("eng-1"),
        "worktrees should be enabled when project is a git repo and member has use_worktrees=true"
    );
}

fn clean_room_test_daemon(project_root: &Path) -> TeamDaemon {
    let team_config = TeamConfig {
        name: "clean-room".to_string(),
        agent: None,
        workflow_mode: WorkflowMode::Legacy,
        board: BoardConfig::default(),
        standup: StandupConfig::default(),
        automation: AutomationConfig::default(),
        automation_sender: None,
        external_senders: Vec::new(),
        orchestrator_pane: true,
        orchestrator_position: OrchestratorPosition::Bottom,
        layout: None,
        workflow_policy: WorkflowPolicy {
            clean_room_mode: true,
            barrier_groups: HashMap::from([
                (
                    "analysis".to_string(),
                    vec![
                        "decompiler".to_string(),
                        "spec-writer".to_string(),
                        "analyst".to_string(),
                    ],
                ),
                ("implementation".to_string(), vec!["engineer".to_string()]),
            ]),
            ..WorkflowPolicy::default()
        },
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
        roles: vec![
            RoleDef {
                name: "decompiler".to_string(),
                role_type: RoleType::Architect,
                agent: Some("claude".to_string()),
                instances: 1,
                prompt: None,
                talks_to: vec!["spec-writer".to_string()],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                barrier_group: Some("analysis".to_string()),
                use_worktrees: true,
            },
            RoleDef {
                name: "spec-writer".to_string(),
                role_type: RoleType::Manager,
                agent: Some("claude".to_string()),
                instances: 1,
                prompt: None,
                talks_to: vec!["decompiler".to_string(), "engineer".to_string()],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                barrier_group: Some("analysis".to_string()),
                use_worktrees: true,
            },
            RoleDef {
                name: "analyst".to_string(),
                role_type: RoleType::Architect,
                agent: Some("claude".to_string()),
                instances: 1,
                prompt: None,
                talks_to: vec![],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                barrier_group: Some("analysis".to_string()),
                use_worktrees: true,
            },
            RoleDef {
                name: "engineer".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("codex".to_string()),
                instances: 1,
                prompt: None,
                talks_to: vec![],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                barrier_group: Some("implementation".to_string()),
                use_worktrees: true,
            },
        ],
    };

    TeamDaemon::new(DaemonConfig {
        project_root: project_root.to_path_buf(),
        team_config,
        session: "test".to_string(),
        members: vec![
            MemberInstance {
                name: "decompiler".to_string(),
                role_name: "decompiler".to_string(),
                role_type: RoleType::Architect,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: Some("spec-writer".to_string()),
                use_worktrees: true,
            },
            MemberInstance {
                name: "spec-writer".to_string(),
                role_name: "spec-writer".to_string(),
                role_type: RoleType::Manager,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: None,
                use_worktrees: true,
            },
            MemberInstance {
                name: "analyst".to_string(),
                role_name: "analyst".to_string(),
                role_type: RoleType::Architect,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: None,
                use_worktrees: true,
            },
            MemberInstance {
                name: "eng-1".to_string(),
                role_name: "engineer".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("codex".to_string()),
                prompt: None,
                reports_to: None,
                use_worktrees: true,
            },
        ],
        pane_map: HashMap::new(),
    })
    .unwrap()
}

fn cleanroom_pipeline_daemon(project_root: &Path) -> TeamDaemon {
    let config_path = crate::team::team_config_path(project_root);
    let team_config = TeamConfig::load(&config_path).unwrap();
    TeamDaemon::new(DaemonConfig {
        project_root: project_root.to_path_buf(),
        team_config,
        session: "cleanroom-test".to_string(),
        members: vec![
            MemberInstance {
                name: "decompiler".to_string(),
                role_name: "decompiler".to_string(),
                role_type: RoleType::Architect,
                agent: Some("claude".to_string()),
                prompt: Some("batty_decompiler.md".to_string()),
                reports_to: None,
                use_worktrees: false,
            },
            MemberInstance {
                name: "spec-writer".to_string(),
                role_name: "spec-writer".to_string(),
                role_type: RoleType::Manager,
                agent: Some("claude".to_string()),
                prompt: Some("batty_spec_writer.md".to_string()),
                reports_to: Some("decompiler".to_string()),
                use_worktrees: false,
            },
            MemberInstance {
                name: "test-writer".to_string(),
                role_name: "test-writer".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("codex".to_string()),
                prompt: Some("batty_test_writer.md".to_string()),
                reports_to: Some("spec-writer".to_string()),
                use_worktrees: true,
            },
            MemberInstance {
                name: "implementer".to_string(),
                role_name: "implementer".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("codex".to_string()),
                prompt: Some("batty_implementer.md".to_string()),
                reports_to: Some("spec-writer".to_string()),
                use_worktrees: true,
            },
        ],
        pane_map: HashMap::new(),
    })
    .unwrap()
}

#[test]
fn clean_room_worktree_dir_uses_barrier_group_root() {
    let tmp = tempfile::tempdir().unwrap();
    let daemon = clean_room_test_daemon(tmp.path());
    assert_eq!(
        daemon.worktree_dir("eng-1"),
        tmp.path()
            .join(".batty")
            .join("worktrees")
            .join("implementation")
            .join("eng-1")
    );
}

#[test]
fn clean_room_handoff_write_and_read_emit_audit_events() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = clean_room_test_daemon(tmp.path());
    let events_path = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("events.jsonl");
    daemon.event_sink = EventSink::new(&events_path).unwrap();

    let artifact_path = daemon
        .write_handoff_artifact(
            "analyst",
            Path::new("specs/spec.md"),
            b"observable behavior",
        )
        .unwrap();
    assert!(artifact_path.exists());

    let content = daemon
        .read_handoff_artifact("eng-1", Path::new("specs/spec.md"))
        .unwrap();
    assert_eq!(content, b"observable behavior");

    let events = std::fs::read_to_string(events_path).unwrap();
    assert!(events.contains("barrier_artifact_created"));
    assert!(events.contains("barrier_artifact_read"));
    assert!(events.contains("content_hash"));
    assert!(events.contains("specs/spec.md"));
}

#[test]
fn clean_room_spec_sync_exports_specs_and_updates_parity() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = clean_room_test_daemon(tmp.path());
    let events_path = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("events.jsonl");
    daemon.event_sink = EventSink::new(&events_path).unwrap();

    let spec_path = tmp.path().join("specs/player-movement/SPEC.md");
    std::fs::create_dir_all(spec_path.parent().unwrap()).unwrap();
    std::fs::write(
        &spec_path,
        r#"# Behavior: Player movement

## Purpose

Describe visible movement in response to directional input.

## Inputs

- Directional input.

## Outputs

- The player sprite moves on screen.

## State Transitions

- Movement starts when input is active and stops when input ends.

## Edge Cases

- Movement stops at solid obstacles.

## Acceptance Criteria

- Given a movement input, the player advances by one visible step.
"#,
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("PARITY.md"),
        r#"---
project: manic-miner
target: analysis-artifacts
source_platform: zx-spectrum
target_language: rust
last_verified: pending
overall_parity: 0%
---

| Behavior | Spec | Test | Implementation | Verified | Notes |
| --- | --- | --- | --- | --- | --- |
| Player movement | draft | -- | -- | -- | |
"#,
    )
    .unwrap();

    daemon.sync_cleanroom_specs().unwrap();

    let exported = tmp
        .path()
        .join(".batty")
        .join("handoff")
        .join("specs/player-movement/SPEC.md");
    assert!(exported.exists());

    let parity = crate::team::parity::ParityReport::load(tmp.path()).unwrap();
    assert_eq!(parity.rows.len(), 1);
    assert_eq!(
        parity.rows[0].spec,
        crate::team::parity::ParityStatus::Complete
    );
    assert_eq!(parity.rows[0].notes, "spec: specs/player-movement/SPEC.md");

    let events = std::fs::read_to_string(events_path).unwrap();
    assert!(events.contains("barrier_artifact_created"));
    assert!(events.contains("specs/player-movement/SPEC.md"));
}

#[test]
fn clean_room_barrier_violation_attempt_is_logged() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = clean_room_test_daemon(tmp.path());
    let events_path = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("events.jsonl");
    daemon.event_sink = EventSink::new(&events_path).unwrap();

    let forbidden = tmp
        .path()
        .join(".batty")
        .join("worktrees")
        .join("analysis")
        .join("analyst");
    let err = daemon
        .validate_member_barrier_path("eng-1", &forbidden, "read")
        .unwrap_err()
        .to_string();
    assert!(err.contains("barrier violation"));

    let events = std::fs::read_to_string(events_path).unwrap();
    assert!(events.contains("barrier_violation_attempt"));
    assert!(events.contains("outside barrier group"));
}

#[test]
fn clean_room_handoff_rejects_parent_directory_escape() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = clean_room_test_daemon(tmp.path());
    let events_path = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("events.jsonl");
    daemon.event_sink = EventSink::new(&events_path).unwrap();

    let err = daemon
        .write_handoff_artifact("analyst", Path::new("../analysis/raw.txt"), b"nope")
        .unwrap_err()
        .to_string();
    assert!(err.contains("invalid handoff artifact path"));

    let events = std::fs::read_to_string(events_path).unwrap();
    assert!(events.contains("barrier_violation_attempt"));
    assert!(events.contains("shared handoff directory"));
}

#[test]
fn clean_room_analysis_artifact_stays_readable_to_analysis_and_blocked_from_implementation() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = clean_room_test_daemon(tmp.path());
    let events_path = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("events.jsonl");
    daemon.event_sink = EventSink::new(&events_path).unwrap();

<<<<<<< HEAD
    let artifact = daemon
        .write_analysis_artifact("decompiler", Path::new("snapshots/game.skool"), b"; notes")
        .unwrap();
    assert!(artifact.exists());

    daemon
        .validate_member_barrier_path("spec-writer", &artifact, "read")
        .unwrap();

    let err = daemon
        .validate_member_barrier_path("eng-1", &artifact, "read")
        .unwrap_err()
        .to_string();
    assert!(err.contains("barrier violation"));

    let events = std::fs::read_to_string(events_path).unwrap();
    assert!(events.contains("barrier_artifact_created"));
    assert!(events.contains("barrier_violation_attempt"));
    assert!(events.contains("snapshots/game.skool"));
}

#[cfg(unix)]
#[test]
fn clean_room_skoolkit_disassembly_supports_z80_and_sna_snapshots() {
    use std::os::unix::fs::PermissionsExt;

    let _path_lock = PATH_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = clean_room_test_daemon(tmp.path());
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let sna2skool = bin_dir.join("sna2skool");
    std::fs::write(
        &sna2skool,
        "#!/bin/sh\nprintf '; disassembly for %s\\n' \"$1\"\n",
    )
    .unwrap();
    let mut perms = std::fs::metadata(&sna2skool).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&sna2skool, perms).unwrap();
    let _sna2skool_guard = EnvVarGuard::set(
        "BATTY_SKOOLKIT_SNA2SKOOL",
        sna2skool.to_string_lossy().as_ref(),
    );

    let z80 = tmp.path().join("game.z80");
    let sna = tmp.path().join("game.sna");
    std::fs::write(&z80, b"z80").unwrap();
    std::fs::write(&sna, b"sna").unwrap();

    let z80_output = daemon
        .run_skoolkit_disassembly("decompiler", &z80, Path::new("snapshots/game.z80.skool"))
        .unwrap();
    let sna_output = daemon
        .run_skoolkit_disassembly("decompiler", &sna, Path::new("snapshots/game.sna.skool"))
        .unwrap();

    assert!(
        std::fs::read_to_string(z80_output)
            .unwrap()
            .contains("game.z80")
    );
    assert!(
        std::fs::read_to_string(sna_output)
            .unwrap()
            .contains("game.sna")
    );
}

#[test]
fn clean_room_pipeline_integration_flow_preserves_barrier_and_updates_parity() {
    let tmp = tempfile::tempdir().unwrap();
    crate::team::init_team(tmp.path(), "cleanroom", Some("zx-spectrum-fixture"), None, false)
        .unwrap();

    let config = TeamConfig::load(&crate::team::team_config_path(tmp.path())).unwrap();
    assert!(config.workflow_policy.clean_room_mode);
    assert_eq!(config.role_barrier_group("decompiler"), Some("analysis"));
    assert_eq!(config.role_barrier_group("spec-writer"), Some("analysis"));
    assert_eq!(config.role_barrier_group("test-writer"), Some("implementation"));
    assert_eq!(config.role_barrier_group("implementer"), Some("implementation"));

    let mut daemon = cleanroom_pipeline_daemon(tmp.path());
    let events_path = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("events.jsonl");
    daemon.event_sink = EventSink::new(&events_path).unwrap();

    let fixture_path = tmp.path().join("analysis").join("zx-spectrum-fixture.z80");
    std::fs::write(&fixture_path, b"FRAME:STARTUP\nBORDER:BLUE\nINPUT:START").unwrap();

    let disassembly_path = tmp.path().join("analysis").join("annotated-disassembly.md");
    std::fs::write(
        &disassembly_path,
        "# Annotated disassembly\n- Observable boot frame flashes blue before start.\n",
    )
    .unwrap();

    let disassembly_err = daemon
        .validate_member_barrier_path("test-writer", &disassembly_path, "read")
        .unwrap_err()
        .to_string();
    assert!(disassembly_err.contains("barrier violation"));

    let spec = r#"# Behavior Specification

## Feature: Startup behavior

#### Purpose

Show a blue border flash before entering the playable state.

#### Inputs

- Pressing start from the title snapshot

#### Outputs

- The game leaves the title frame and begins play

#### State Transitions

- Title snapshot transitions to active play after the start input

#### Timing And Ordering

- One blue border flash occurs before the first playable frame

#### Edge Cases

- Ignoring idle frames must not skip the border flash

#### Acceptance Criteria

- A black-box replay observes exactly one blue flash before play begins
"#;
    let spec_path = daemon
        .write_handoff_artifact("spec-writer", Path::new("pipeline/SPEC.md"), spec.as_bytes())
        .unwrap();
    assert!(spec_path.exists());

    let shared_spec = daemon
        .read_handoff_artifact("test-writer", Path::new("pipeline/SPEC.md"))
        .unwrap();
    assert!(String::from_utf8(shared_spec).unwrap().contains("blue border flash"));

    let original_binary_err = daemon
        .validate_member_barrier_path("implementer", &fixture_path, "read")
        .unwrap_err()
        .to_string();
    assert!(original_binary_err.contains("barrier violation"));

    let tests_dir = tmp.path().join("implementation").join("tests");
    std::fs::create_dir_all(&tests_dir).unwrap();
    std::fs::write(
        tests_dir.join("startup_behavior.md"),
        "- assert one blue border flash before the first playable frame\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("implementation").join("stub_runtime.rs"),
        "pub fn replay_startup() -> &'static str { \"blue flash -> play\" }\n",
    )
    .unwrap();

    let parity_draft = r#"---
project: zx-spectrum-fixture
target: zx-spectrum-fixture.z80
source_platform: zx-spectrum-z80
target_language: rust
last_verified: in-progress
overall_parity: 0%
---

| Behavior | Spec | Test | Implementation | Verified | Notes |
| --- | --- | --- | --- | --- | --- |
| Startup behavior | complete | complete | draft | -- | Acceptance test drafted from behavior-only spec |
"#;
    std::fs::write(tmp.path().join("PARITY.md"), parity_draft).unwrap();

    let draft_report = crate::team::parity::ParityReport::load(tmp.path()).unwrap();
    let draft_summary = draft_report.summary();
    assert_eq!(draft_summary.total_behaviors, 1);
    assert_eq!(draft_summary.spec_complete, 1);
    assert_eq!(draft_summary.tests_complete, 1);
    assert_eq!(draft_summary.implementation_complete, 0);
    assert_eq!(draft_summary.verified_pass, 0);

    fn compare_fixture_behavior(original: &[u8], stub: &str) -> bool {
        original.windows(b"BORDER:BLUE".len()).any(|window| window == b"BORDER:BLUE")
            && stub == "blue flash -> play"
    }

    let original_snapshot = std::fs::read(&fixture_path).unwrap();
    let stub_behavior = "blue flash -> play";
    assert!(compare_fixture_behavior(&original_snapshot, stub_behavior));

    let parity_complete = r#"---
project: zx-spectrum-fixture
target: zx-spectrum-fixture.z80
source_platform: zx-spectrum-z80
target_language: rust
last_verified: 2026-04-05
overall_parity: 100%
---

| Behavior | Spec | Test | Implementation | Verified | Notes |
| --- | --- | --- | --- | --- | --- |
| Startup behavior | complete | complete | complete | PASS | Replay matched the fixture snapshot |
"#;
    std::fs::write(tmp.path().join("PARITY.md"), parity_complete).unwrap();

    let final_report = crate::team::parity::ParityReport::load(tmp.path()).unwrap();
    let final_summary = final_report.summary();
    assert_eq!(final_summary.total_behaviors, 1);
    assert_eq!(final_summary.spec_complete, 1);
    assert_eq!(final_summary.tests_complete, 1);
    assert_eq!(final_summary.implementation_complete, 1);
    assert_eq!(final_summary.verified_pass, 1);
    assert_eq!(final_summary.overall_parity_pct, 100);

    let events = std::fs::read_to_string(events_path).unwrap();
    assert!(events.contains("barrier_artifact_created"));
    assert!(events.contains("barrier_artifact_read"));
    assert!(events.contains("barrier_violation_attempt"));
}

// --- Stale review escalation tests ---

fn write_review_task(project_root: &Path, id: u32, review_owner: &str) {
    write_review_task_with_priority(project_root, id, review_owner, "high");
}

fn write_review_task_with_priority(
    project_root: &Path,
    id: u32,
    review_owner: &str,
    priority: &str,
) {
    let tasks_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(
            tasks_dir.join(format!("{id:03}-review-task-{id}.md")),
            format!(
                "---\nid: {id}\ntitle: review-task-{id}\nstatus: review\npriority: {priority}\nclass: standard\nclaimed_by: eng-1\nreview_owner: {review_owner}\n---\n\nTask description.\n"
            ),
        )
        .unwrap();
}

fn stale_review_daemon(tmp: &tempfile::TempDir) -> TeamDaemon {
    TestDaemonBuilder::new(tmp.path())
        .members(vec![
            architect_member("architect"),
            manager_member("manager", Some("architect")),
            engineer_member("eng-1", Some("manager"), false),
        ])
        .workflow_policy(WorkflowPolicy {
            review_nudge_threshold_secs: 1800,
            review_timeout_secs: 7200,
            ..WorkflowPolicy::default()
        })
        .build()
}

#[test]
fn stale_review_sends_nudge_at_threshold() {
    let tmp = tempfile::tempdir().unwrap();
    write_review_task(tmp.path(), 42, "manager");
    let mut daemon = stale_review_daemon(&tmp);

    // Seed the first_seen time to 1801 seconds ago
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    daemon.review_first_seen.insert(42, now - 1801);

    daemon.maybe_escalate_stale_reviews().unwrap();

    // Nudge should have been sent
    assert!(daemon.review_nudge_sent.contains(&42));

    // Event should be emitted (check event sink wrote something)
    let events_path = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("events.jsonl");
    let events = std::fs::read_to_string(&events_path).unwrap_or_default();
    assert!(events.contains("review_nudge_sent"));
}

#[test]
fn stale_review_escalates_at_timeout() {
    let tmp = tempfile::tempdir().unwrap();
    write_review_task(tmp.path(), 42, "manager");
    let mut daemon = stale_review_daemon(&tmp);

    // Seed the first_seen time to 7201 seconds ago
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    daemon.review_first_seen.insert(42, now - 7201);

    daemon.maybe_escalate_stale_reviews().unwrap();

    // Task should no longer be tracked (it was escalated)
    assert!(!daemon.review_first_seen.contains_key(&42));
    assert!(!daemon.review_nudge_sent.contains(&42));

    // Task should be transitioned to blocked
    let tasks_dir = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    let tasks = crate::task::load_tasks_from_dir(&tasks_dir).unwrap();
    let task = tasks.iter().find(|t| t.id == 42).unwrap();
    assert_eq!(task.status, "blocked");
    assert_eq!(
        task.blocked_on.as_deref(),
        Some("review timeout escalated to architect")
    );

    // Event should be emitted
    let events_path = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("events.jsonl");
    let events = std::fs::read_to_string(&events_path).unwrap_or_default();
    assert!(events.contains("review_escalated"));
}

#[test]
fn nudge_only_sent_once() {
    let tmp = tempfile::tempdir().unwrap();
    write_review_task(tmp.path(), 42, "manager");
    let mut daemon = stale_review_daemon(&tmp);

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    daemon.review_first_seen.insert(42, now - 1801);

    // First call: nudge sent
    daemon.maybe_escalate_stale_reviews().unwrap();
    assert!(daemon.review_nudge_sent.contains(&42));

    // Count events
    let events_path = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("events.jsonl");
    let events_before = std::fs::read_to_string(&events_path)
        .unwrap_or_default()
        .matches("review_nudge_sent")
        .count();

    // Second call: nudge should NOT fire again
    daemon.maybe_escalate_stale_reviews().unwrap();
    let events_after = std::fs::read_to_string(&events_path)
        .unwrap_or_default()
        .matches("review_nudge_sent")
        .count();

    assert_eq!(events_before, events_after, "nudge should not fire twice");
}

#[test]
fn config_nudge_threshold_defaults() {
    let policy = WorkflowPolicy::default();
    assert_eq!(policy.review_nudge_threshold_secs, 1800);
    assert_eq!(policy.review_timeout_secs, 7200);
}

// --- Per-priority review timeout override tests ---

fn stale_review_daemon_with_overrides(tmp: &tempfile::TempDir) -> TeamDaemon {
    use crate::team::config::ReviewTimeoutOverride;
    let mut overrides = std::collections::HashMap::new();
    overrides.insert(
        "critical".to_string(),
        ReviewTimeoutOverride {
            review_nudge_threshold_secs: Some(300),
            review_timeout_secs: Some(600),
        },
    );
    overrides.insert(
        "high".to_string(),
        ReviewTimeoutOverride {
            review_nudge_threshold_secs: Some(900),
            review_timeout_secs: Some(3600),
        },
    );
    TestDaemonBuilder::new(tmp.path())
        .members(vec![
            architect_member("architect"),
            manager_member("manager", Some("architect")),
            engineer_member("eng-1", Some("manager"), false),
        ])
        .workflow_policy(WorkflowPolicy {
            review_nudge_threshold_secs: 1800,
            review_timeout_secs: 7200,
            review_timeout_overrides: overrides,
            ..WorkflowPolicy::default()
        })
        .build()
}

#[test]
fn critical_task_nudges_at_priority_override_threshold() {
    let tmp = tempfile::tempdir().unwrap();
    write_review_task_with_priority(tmp.path(), 50, "manager", "critical");
    let mut daemon = stale_review_daemon_with_overrides(&tmp);

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // 301s > critical nudge threshold of 300s
    daemon.review_first_seen.insert(50, now - 301);

    daemon.maybe_escalate_stale_reviews().unwrap();

    assert!(
        daemon.review_nudge_sent.contains(&50),
        "critical task should be nudged at 300s override"
    );
}

#[test]
fn critical_task_not_nudged_below_override_threshold() {
    let tmp = tempfile::tempdir().unwrap();
    write_review_task_with_priority(tmp.path(), 50, "manager", "critical");
    let mut daemon = stale_review_daemon_with_overrides(&tmp);

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // 200s < critical nudge threshold of 300s
    daemon.review_first_seen.insert(50, now - 200);

    daemon.maybe_escalate_stale_reviews().unwrap();

    assert!(
        !daemon.review_nudge_sent.contains(&50),
        "critical task should not be nudged before 300s"
    );
}

#[test]
fn critical_task_escalates_at_priority_override_threshold() {
    let tmp = tempfile::tempdir().unwrap();
    write_review_task_with_priority(tmp.path(), 50, "manager", "critical");
    let mut daemon = stale_review_daemon_with_overrides(&tmp);

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // 601s > critical escalation threshold of 600s
    daemon.review_first_seen.insert(50, now - 601);

    daemon.maybe_escalate_stale_reviews().unwrap();

    // Task escalated — removed from tracking
    assert!(!daemon.review_first_seen.contains_key(&50));

    // Task should be blocked
    let tasks_dir = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    let tasks = crate::task::load_tasks_from_dir(&tasks_dir).unwrap();
    let task = tasks.iter().find(|t| t.id == 50).unwrap();
    assert_eq!(task.status, "blocked");
}

#[test]
fn medium_task_uses_global_thresholds_when_no_override() {
    let tmp = tempfile::tempdir().unwrap();
    write_review_task_with_priority(tmp.path(), 51, "manager", "medium");
    let mut daemon = stale_review_daemon_with_overrides(&tmp);

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // 1000s > critical override (300s) but < global nudge (1800s)
    daemon.review_first_seen.insert(51, now - 1000);

    daemon.maybe_escalate_stale_reviews().unwrap();

    assert!(
        !daemon.review_nudge_sent.contains(&51),
        "medium task should use global 1800s threshold, not critical 300s"
    );
}

#[test]
fn mixed_priority_tasks_get_different_thresholds() {
    let tmp = tempfile::tempdir().unwrap();
    write_review_task_with_priority(tmp.path(), 60, "manager", "critical");
    write_review_task_with_priority(tmp.path(), 61, "manager", "medium");
    let mut daemon = stale_review_daemon_with_overrides(&tmp);

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Both at 400s age: exceeds critical nudge (300s) but not medium nudge (1800s)
    daemon.review_first_seen.insert(60, now - 400);
    daemon.review_first_seen.insert(61, now - 400);

    daemon.maybe_escalate_stale_reviews().unwrap();

    assert!(
        daemon.review_nudge_sent.contains(&60),
        "critical task should be nudged at 400s (threshold 300s)"
    );
    assert!(
        !daemon.review_nudge_sent.contains(&61),
        "medium task should NOT be nudged at 400s (threshold 1800s)"
    );
}

// ── Error-path sentinel tests ──────────────────────────────────────

#[test]
fn load_daemon_state_returns_none_for_missing_file() {
    let tmp = tempfile::tempdir().unwrap();
    // No daemon state file exists
    let result = load_daemon_state(tmp.path());
    assert!(result.is_none(), "missing state file should return None");
}

#[test]
fn load_daemon_state_returns_none_for_corrupt_json() {
    let tmp = tempfile::tempdir().unwrap();
    let state_path = super::daemon_state_path(tmp.path());
    std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
    std::fs::write(&state_path, "not valid json {{{").unwrap();

    let result = load_daemon_state(tmp.path());
    assert!(
        result.is_none(),
        "corrupt JSON should return None, not panic"
    );
}

#[test]
fn save_daemon_state_returns_error_on_readonly_dir() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let lock_dir = tmp.path().join(".batty");
        std::fs::create_dir_all(&lock_dir).unwrap();
        // Make directory read-only so write fails
        std::fs::set_permissions(&lock_dir, std::fs::Permissions::from_mode(0o444)).unwrap();

        let state = PersistedDaemonState {
            clean_shutdown: false,
            saved_at: 0,
            states: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            dispatch_queue: Vec::new(),
            paused_standups: HashSet::new(),
            last_standup_elapsed_secs: HashMap::new(),
            nudge_state: HashMap::new(),
            pipeline_starvation_fired: false,
        };

        let result = save_daemon_state(tmp.path(), &state);
        // Restore permissions for cleanup
        std::fs::set_permissions(&lock_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            result.is_err(),
            "writing to read-only directory should return error, not panic"
        );
    }
}

#[test]
fn watcher_mut_missing_member_returns_error_not_panic() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
    let mut daemon = make_test_daemon(tmp.path(), vec![manager_member("manager", None)]);

    let result = daemon.watcher_mut("nonexistent-member");
    match result {
        Ok(_) => panic!("expected error for missing member"),
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("nonexistent-member"),
                "error should name the missing member, got: {msg}"
            );
        }
    }
}

#[test]
fn extract_nudge_missing_file_returns_none_not_panic() {
    let result = extract_nudge_section(Path::new("/nonexistent/path/prompt.md"));
    assert!(
        result.is_none(),
        "missing prompt file should return None, not panic"
    );
}

#[test]
fn binary_fingerprint_capture_missing_file_returns_error() {
    let result = BinaryFingerprint::capture(Path::new("/nonexistent/binary"));
    assert!(
        result.is_err(),
        "capturing fingerprint of missing file should return error, not panic"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("/nonexistent/binary"),
        "error should include the file path"
    );
}

#[test]
fn load_dispatch_queue_graceful_when_no_state() {
    let tmp = tempfile::tempdir().unwrap();
    let queue = load_dispatch_queue_snapshot(tmp.path());
    assert!(
        queue.is_empty(),
        "dispatch queue from missing state should be empty, not panic"
    );
}
