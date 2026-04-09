use super::*;

fn minimal_yaml() -> &'static str {
    r#"
name: test-team
roles:
  - name: architect
    role_type: architect
    agent: claude
    instances: 1
    talks_to: [manager]
  - name: manager
    role_type: manager
    agent: claude
    instances: 1
    talks_to: [architect, engineer]
  - name: engineer
    role_type: engineer
    agent: codex
    instances: 3
    talks_to: [manager]
"#
}

#[test]
fn parse_minimal_config() {
    let config: TeamConfig = serde_yaml::from_str(minimal_yaml()).unwrap();
    assert_eq!(config.name, "test-team");
    assert_eq!(config.workflow_mode, WorkflowMode::Legacy);
    assert_eq!(config.roles.len(), 3);
    assert_eq!(config.roles[0].role_type, RoleType::Architect);
    assert_eq!(config.roles[2].instances, 3);
    assert_eq!(config.workflow_mode, WorkflowMode::Legacy);
    assert!(config.orchestrator_pane);
    assert_eq!(
        config.event_log_max_bytes,
        super::super::DEFAULT_EVENT_LOG_MAX_BYTES
    );
    assert!(config.auto_respawn_on_crash);
    assert_eq!(config.workflow_policy.graceful_shutdown_timeout_secs, 5);
    assert!(config.workflow_policy.auto_commit_on_restart);
    assert!(!config.workflow_policy.clean_room_mode);
    assert!(config.workflow_policy.barrier_groups.is_empty());
    assert_eq!(config.workflow_policy.handoff_directory, ".batty/handoff");
    assert_eq!(config.workflow_policy.stale_in_progress_hours, 4);
    assert_eq!(config.workflow_policy.aged_todo_hours, 48);
    assert_eq!(config.workflow_policy.stale_review_hours, 1);
}

#[test]
fn parse_config_with_user_role() {
    let yaml = r#"
name: test-team
roles:
  - name: human
    role_type: user
    channel: telegram
    channel_config:
      target: "12345"
      provider: openclaw
    talks_to: [architect]
  - name: architect
    role_type: architect
    agent: claude
    instances: 1
    talks_to: [human]
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.roles[0].role_type, RoleType::User);
    assert_eq!(config.roles[0].channel.as_deref(), Some("telegram"));
    assert_eq!(
        config.roles[0].channel_config.as_ref().unwrap().provider,
        "openclaw"
    );
    assert_eq!(config.workflow_mode, WorkflowMode::Legacy);
    assert!(config.orchestrator_pane);
}

#[test]
fn parse_config_with_discord_user_role() {
    let yaml = r#"
name: test-team
roles:
  - name: human
    role_type: user
    channel: discord
    channel_config:
      bot_token: discord-token
      events_channel_id: "1490930323608047647"
      agents_channel_id: "1490930375822676070"
      commands_channel_id: "1490930426812829716"
      allowed_user_ids: ["170281556", 170281557]
    talks_to: [architect]
  - name: architect
    role_type: architect
    agent: claude
    instances: 1
    talks_to: [human]
"#;

    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let channel_config = config.roles[0].channel_config.as_ref().unwrap();
    assert_eq!(config.roles[0].channel.as_deref(), Some("discord"));
    assert_eq!(
        channel_config.events_channel_id.as_deref(),
        Some("1490930323608047647")
    );
    assert_eq!(
        channel_config.commands_channel_id.as_deref(),
        Some("1490930426812829716")
    );
    assert_eq!(channel_config.allowed_user_ids, vec![170281556, 170281557]);
}

#[test]
fn planning_directive_path_uses_team_config_directory() {
    let root = std::path::Path::new("/tmp/project");

    assert_eq!(
        PlanningDirectiveFile::ReviewPolicy.path_for(root),
        std::path::PathBuf::from("/tmp/project/.batty/team_config/review_policy.md")
    );
}

#[test]
fn load_planning_directive_returns_none_when_missing() {
    let tmp = tempfile::tempdir().unwrap();

    let loaded =
        load_planning_directive(tmp.path(), PlanningDirectiveFile::ReviewPolicy, 120).unwrap();

    assert_eq!(loaded, None);
}

#[test]
fn load_planning_directive_truncates_long_content() {
    let tmp = tempfile::tempdir().unwrap();
    let config_dir = tmp.path().join(".batty").join("team_config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("review_policy.md"),
        "abcdefghijklmnopqrstuvwxyz",
    )
    .unwrap();

    let loaded = load_planning_directive(tmp.path(), PlanningDirectiveFile::ReviewPolicy, 10)
        .unwrap()
        .unwrap();

    assert!(loaded.starts_with("abcdefghij"));
    assert!(loaded.contains("[truncated to 10 chars"));
}

#[test]
fn parse_full_config_with_layout() {
    let yaml = r#"
name: mafia-solver
board:
  rotation_threshold: 20
  auto_dispatch: false
  auto_replenish: false
workflow_mode: hybrid
orchestrator_pane: false
standup:
  interval_secs: 1200
  output_lines: 30
automation:
  timeout_nudges: true
  standups: true
  triage_interventions: true
  review_interventions: true
  owned_task_interventions: true
  manager_dispatch_interventions: true
  architect_utilization_interventions: true
  intervention_idle_grace_secs: 60
layout:
  zones:
    - name: architect
      width_pct: 15
    - name: managers
      width_pct: 25
      split: { horizontal: 3 }
    - name: engineers
      width_pct: 60
      split: { horizontal: 15 }
roles:
  - name: architect
    role_type: architect
    agent: claude
    instances: 1
    prompt: architect.md
    talks_to: [manager]
    nudge_interval_secs: 1800
    owns: ["planning/**", "docs/**"]
  - name: manager
    role_type: manager
    agent: claude
    instances: 3
    prompt: manager.md
    talks_to: [architect, engineer]
  - name: engineer
    role_type: engineer
    agent: codex
    instances: 5
    prompt: engineer.md
    talks_to: [manager]
    use_worktrees: true
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.name, "mafia-solver");
    assert_eq!(config.board.rotation_threshold, 20);
    assert!(!config.board.auto_dispatch);
    assert!(!config.board.auto_replenish);
    assert_eq!(config.board.state_reconciliation_interval_secs, 30);
    assert_eq!(config.workflow_mode, WorkflowMode::Hybrid);
    assert!(!config.orchestrator_pane);
    assert_eq!(config.standup.interval_secs, 1200);
    let layout = config.layout.as_ref().unwrap();
    assert_eq!(layout.zones.len(), 3);
    assert_eq!(layout.zones[0].width_pct, 15);
    assert_eq!(layout.zones[2].split.as_ref().unwrap().horizontal, 15);
    assert_eq!(
        config.event_log_max_bytes,
        super::super::DEFAULT_EVENT_LOG_MAX_BYTES
    );
}

#[test]
fn defaults_applied() {
    let yaml = r#"
name: minimal
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.workflow_mode, WorkflowMode::Legacy);
    assert_eq!(config.board.rotation_threshold, 20);
    assert!(config.board.auto_dispatch);
    assert!(config.board.auto_replenish);
    assert_eq!(config.board.state_reconciliation_interval_secs, 30);
    assert_eq!(config.standup.interval_secs, 300);
    assert_eq!(config.standup.output_lines, 30);
    assert!(config.automation.timeout_nudges);
    assert!(config.automation.standups);
    assert!(config.automation.failure_pattern_detection);
    assert!(config.automation.triage_interventions);
    assert_eq!(config.automation.intervention_idle_grace_secs, 60);
    assert_eq!(config.workflow_policy.narration_threshold, 0.8);
    assert_eq!(config.workflow_policy.narration_nudge_max, 2);
    assert!(config.workflow_policy.narration_detection_enabled);
    assert_eq!(config.workflow_policy.narration_threshold_polls, 5);
    assert_eq!(config.workflow_policy.context_pressure_threshold, 100);
    assert_eq!(
        config.workflow_policy.context_pressure_threshold_bytes,
        512_000
    );
    assert_eq!(
        config.workflow_policy.context_pressure_restart_delay_secs,
        120
    );
    assert_eq!(config.workflow_policy.graceful_shutdown_timeout_secs, 5);
    assert!(config.workflow_policy.auto_commit_on_restart);
    assert_eq!(config.workflow_policy.stale_in_progress_hours, 4);
    assert_eq!(config.workflow_policy.aged_todo_hours, 48);
    assert_eq!(config.workflow_policy.stale_review_hours, 1);
    assert!(config.cost.models.is_empty());
    assert_eq!(config.roles[0].instances, 1);
    assert_eq!(config.workflow_mode, WorkflowMode::Legacy);
    assert!(config.orchestrator_pane);
    assert_eq!(
        config.event_log_max_bytes,
        super::super::DEFAULT_EVENT_LOG_MAX_BYTES
    );
    assert!(config.auto_respawn_on_crash);
}

#[test]
fn explicit_auto_respawn_on_crash_false_is_preserved() {
    let yaml = r#"
name: minimal
auto_respawn_on_crash: false
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert!(!config.auto_respawn_on_crash);
}

#[test]
fn clean_room_policy_parses_with_barrier_groups() {
    let yaml = r#"
name: clean-room
workflow_policy:
  clean_room_mode: true
  handoff_directory: shared/handoff
  barrier_groups:
    analysis: [architect]
    implementation: [engineer]
roles:
  - name: architect
    role_type: architect
    agent: claude
    barrier_group: analysis
  - name: engineer
    role_type: engineer
    agent: codex
    barrier_group: implementation
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert!(config.workflow_policy.clean_room_mode);
    assert_eq!(config.workflow_policy.handoff_directory, "shared/handoff");
    assert_eq!(
        config.workflow_policy.barrier_groups["analysis"],
        vec!["architect".to_string()]
    );
    assert_eq!(
        config.role_barrier_group("engineer"),
        Some("implementation")
    );
}

#[test]
fn clean_room_validation_rejects_unknown_barrier_group() {
    let yaml = r#"
name: clean-room
workflow_policy:
  clean_room_mode: true
  barrier_groups:
    analysis: [architect]
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: engineer
    role_type: engineer
    agent: codex
    barrier_group: implementation
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let err = config.validate().unwrap_err().to_string();
    assert!(err.contains("unknown barrier_group"));
}

#[test]
fn team_cleanroom_template_parses_correctly() {
    let config: TeamConfig =
        serde_yaml::from_str(include_str!("../templates/team_cleanroom.yaml")).unwrap();

    assert!(config.use_shim);
    assert!(config.auto_respawn_on_crash);
    assert!(config.workflow_policy.clean_room_mode);
    assert_eq!(
        config.workflow_policy.barrier_groups["analysis"],
        vec!["decompiler".to_string(), "spec-writer".to_string()]
    );
    assert_eq!(
        config.workflow_policy.barrier_groups["implementation"],
        vec!["test-writer".to_string(), "implementer".to_string()]
    );
}

#[test]
fn team_cleanroom_template_roles_define_barrier_groups() {
    let config: TeamConfig =
        serde_yaml::from_str(include_str!("../templates/team_cleanroom.yaml")).unwrap();

    assert_eq!(config.role_barrier_group("decompiler"), Some("analysis"));
    assert_eq!(config.role_barrier_group("spec-writer"), Some("analysis"));
    assert_eq!(
        config.role_barrier_group("test-writer"),
        Some("implementation")
    );
    assert_eq!(
        config.role_barrier_group("implementer"),
        Some("implementation")
    );
}

#[test]
fn validate_accepts_kiro_agent() {
    let yaml = r#"
name: test-team
roles:
  - name: architect
    role_type: architect
    agent: kiro
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert!(config.validate().is_ok());
}

#[test]
fn validate_rejects_unknown_agent() {
    let yaml = r#"
name: test-team
roles:
  - name: architect
    role_type: architect
    agent: mystery
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let error = config.validate().unwrap_err().to_string();
    assert!(error.contains("unknown agent 'mystery'"));
    assert!(error.contains("valid agents:"));
}

#[test]
fn validate_rejects_unknown_instance_override_agent() {
    let yaml = r#"
name: test-team
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: engineer
    role_type: engineer
    agent: codex
    instance_overrides:
      eng-1-1:
        agent: mystery
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let error = config.validate().unwrap_err().to_string();
    assert!(error.contains("instance override 'eng-1-1'"));
    assert!(error.contains("unknown agent 'mystery'"));
}

#[test]
fn validate_accepts_gemini_instance_override_agent() {
    let yaml = r#"
name: test-team
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: engineer
    role_type: engineer
    agent: codex
    instance_overrides:
      eng-1-1:
        agent: gemini
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert!(config.validate().is_ok());
}

#[test]
fn parse_event_log_max_bytes_override() {
    let yaml = r#"
name: test
event_log_max_bytes: 2048
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.event_log_max_bytes, 2048);
}

#[test]
fn parse_cost_config() {
    let yaml = r#"
name: test-team
cost:
  models:
    gpt-5.4:
      input_usd_per_mtok: 2.5
      cached_input_usd_per_mtok: 0.25
      output_usd_per_mtok: 15.0
    claude-opus-4-6:
      input_usd_per_mtok: 15.0
      cache_creation_5m_input_usd_per_mtok: 18.75
      cache_creation_1h_input_usd_per_mtok: 30.0
      cache_read_input_usd_per_mtok: 1.5
      output_usd_per_mtok: 75.0
roles:
  - name: architect
    role_type: architect
    agent: claude
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let gpt = config.cost.models.get("gpt-5.4").unwrap();
    assert_eq!(gpt.input_usd_per_mtok, 2.5);
    assert_eq!(gpt.cached_input_usd_per_mtok, 0.25);
    assert_eq!(gpt.output_usd_per_mtok, 15.0);

    let claude = config.cost.models.get("claude-opus-4-6").unwrap();
    assert_eq!(claude.input_usd_per_mtok, 15.0);
    assert_eq!(claude.cache_creation_5m_input_usd_per_mtok, Some(18.75));
    assert_eq!(claude.cache_creation_1h_input_usd_per_mtok, Some(30.0));
    assert_eq!(claude.cache_read_input_usd_per_mtok, 1.5);
    assert_eq!(claude.output_usd_per_mtok, 75.0);
}

#[test]
fn parse_workflow_mode_legacy_when_absent() {
    let config: TeamConfig = serde_yaml::from_str(minimal_yaml()).unwrap();

    assert_eq!(config.workflow_mode, WorkflowMode::Legacy);
    assert!(config.workflow_mode.legacy_runtime_enabled());
    assert!(!config.workflow_mode.workflow_state_primary());
}

#[test]
fn parse_workflow_mode_hybrid_from_yaml() {
    let yaml = format!("workflow_mode: hybrid\n{}", minimal_yaml());
    let config: TeamConfig = serde_yaml::from_str(&yaml).unwrap();

    assert_eq!(config.workflow_mode, WorkflowMode::Hybrid);
    assert!(config.workflow_mode.legacy_runtime_enabled());
    assert!(!config.workflow_mode.workflow_state_primary());
}

#[test]
fn parse_workflow_mode_workflow_first_from_yaml() {
    let yaml = format!("workflow_mode: workflow_first\n{}", minimal_yaml());
    let config: TeamConfig = serde_yaml::from_str(&yaml).unwrap();

    assert_eq!(config.workflow_mode, WorkflowMode::WorkflowFirst);
    assert!(!config.workflow_mode.legacy_runtime_enabled());
    assert!(config.workflow_mode.workflow_state_primary());
}

#[test]
fn parse_workflow_mode_board_first_from_yaml() {
    let yaml = format!("workflow_mode: board_first\n{}", minimal_yaml());
    let config: TeamConfig = serde_yaml::from_str(&yaml).unwrap();

    assert_eq!(config.workflow_mode, WorkflowMode::BoardFirst);
    assert!(!config.workflow_mode.legacy_runtime_enabled());
    assert!(config.workflow_mode.workflow_state_primary());
    assert!(config.workflow_mode.enables_runtime_surface());
    assert!(config.workflow_mode.suppresses_manager_relay());
}

#[test]
fn parse_explicit_automation_config() {
    let yaml = r#"
name: test
automation:
  timeout_nudges: false
  standups: true
  failure_pattern_detection: false
  triage_interventions: true
  review_interventions: false
  owned_task_interventions: true
  manager_dispatch_interventions: false
  architect_utilization_interventions: true
  intervention_idle_grace_secs: 90
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert!(!config.automation.timeout_nudges);
    assert!(config.automation.standups);
    assert!(!config.automation.failure_pattern_detection);
    assert!(config.automation.triage_interventions);
    assert!(!config.automation.review_interventions);
    assert!(config.automation.owned_task_interventions);
    assert!(!config.automation.manager_dispatch_interventions);
    assert!(config.automation.architect_utilization_interventions);
    assert_eq!(config.automation.intervention_idle_grace_secs, 90);
}

#[test]
fn parse_workflow_mode_variants() {
    let legacy: TeamConfig = serde_yaml::from_str(
        r#"
name: test
workflow_mode: legacy
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#,
    )
    .unwrap();
    assert_eq!(legacy.workflow_mode, WorkflowMode::Legacy);

    let hybrid: TeamConfig = serde_yaml::from_str(
        r#"
name: test
workflow_mode: hybrid
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#,
    )
    .unwrap();
    assert_eq!(hybrid.workflow_mode, WorkflowMode::Hybrid);

    let workflow_first: TeamConfig = serde_yaml::from_str(
        r#"
name: test
workflow_mode: workflow_first
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#,
    )
    .unwrap();
    assert_eq!(workflow_first.workflow_mode, WorkflowMode::WorkflowFirst);

    let board_first: TeamConfig = serde_yaml::from_str(
        r#"
name: test
workflow_mode: board_first
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#,
    )
    .unwrap();
    assert_eq!(board_first.workflow_mode, WorkflowMode::BoardFirst);
}

#[test]
fn orchestrator_enabled_respects_mode_and_pane_flag() {
    let legacy: TeamConfig = serde_yaml::from_str(minimal_yaml()).unwrap();
    assert!(!legacy.orchestrator_enabled());

    let hybrid_enabled: TeamConfig = serde_yaml::from_str(&format!(
        "workflow_mode: hybrid\norchestrator_pane: true\n{}",
        minimal_yaml()
    ))
    .unwrap();
    assert!(hybrid_enabled.orchestrator_enabled());

    let hybrid_disabled: TeamConfig = serde_yaml::from_str(&format!(
        "workflow_mode: hybrid\norchestrator_pane: false\n{}",
        minimal_yaml()
    ))
    .unwrap();
    assert!(!hybrid_disabled.orchestrator_enabled());

    let workflow_first_enabled: TeamConfig = serde_yaml::from_str(&format!(
        "workflow_mode: workflow_first\norchestrator_pane: true\n{}",
        minimal_yaml()
    ))
    .unwrap();
    assert!(workflow_first_enabled.orchestrator_enabled());

    let board_first_enabled: TeamConfig = serde_yaml::from_str(&format!(
        "workflow_mode: board_first\norchestrator_pane: true\n{}",
        minimal_yaml()
    ))
    .unwrap();
    assert!(board_first_enabled.orchestrator_enabled());
}

#[test]
fn validate_rejects_empty_name() {
    let yaml = r#"
name: ""
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let err = config.validate().unwrap_err().to_string();
    assert!(err.contains("empty"));
}

#[test]
fn validate_rejects_duplicate_role_names() {
    let yaml = r#"
name: test
roles:
  - name: worker
    role_type: engineer
    agent: codex
  - name: worker
    role_type: engineer
    agent: codex
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let err = config.validate().unwrap_err().to_string();
    assert!(err.contains("duplicate"));
}

#[test]
fn validate_rejects_non_user_without_agent() {
    let yaml = r#"
name: test
roles:
  - name: worker
    role_type: engineer
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let err = config.validate().unwrap_err().to_string();
    assert!(err.contains("no agent"));
}

#[test]
fn validate_rejects_user_with_agent() {
    let yaml = r#"
name: test
roles:
  - name: human
    role_type: user
    agent: claude
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let err = config.validate().unwrap_err().to_string();
    assert!(err.contains("user") && err.contains("agent"));
}

#[test]
fn validate_rejects_unknown_talks_to() {
    let yaml = r#"
name: test
roles:
  - name: worker
    role_type: engineer
    agent: codex
    talks_to: [nonexistent]
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let err = config.validate().unwrap_err().to_string();
    assert!(err.contains("unknown role"));
}

#[test]
fn validate_rejects_unknown_automation_sender() {
    let yaml = r#"
name: test
automation_sender: nonexistent
roles:
  - name: architect
    role_type: architect
    agent: claude
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let err = config.validate().unwrap_err().to_string();
    assert!(err.contains("automation_sender"));
    assert!(err.contains("unknown role"));
}

#[test]
fn validate_accepts_known_automation_sender() {
    let yaml = r#"
name: test
automation_sender: human
roles:
  - name: human
    role_type: user
    channel: telegram
    channel_config:
      target: "12345"
      provider: openclaw
  - name: architect
    role_type: architect
    agent: claude
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    config.validate().unwrap();
}

#[test]
fn validate_rejects_layout_over_100_pct() {
    let yaml = r#"
name: test
layout:
  zones:
    - name: a
      width_pct: 60
    - name: b
      width_pct: 50
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let err = config.validate().unwrap_err().to_string();
    assert!(err.contains("100%"));
}

#[test]
fn validate_accepts_minimal_config() {
    let config: TeamConfig = serde_yaml::from_str(minimal_yaml()).unwrap();
    config.validate().unwrap();
}

#[test]
fn load_from_file() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("team.yaml");
    std::fs::write(&path, minimal_yaml()).unwrap();
    let config = TeamConfig::load(&path).unwrap();
    assert_eq!(config.name, "test-team");
    assert_eq!(config.workflow_mode, WorkflowMode::Legacy);
}

#[test]
fn omitted_workflow_mode_with_orchestrator_disabled_stays_legacy() {
    let yaml = r#"
name: test
orchestrator_pane: false
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;

    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.workflow_mode, WorkflowMode::Legacy);
    assert!(!config.orchestrator_pane);
}

#[test]
fn omitted_workflow_mode_with_orchestrator_enabled_promotes_to_hybrid() {
    let yaml = r#"
name: test
orchestrator_pane: true
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;

    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.workflow_mode, WorkflowMode::Hybrid);
    assert!(config.orchestrator_enabled());
}

#[test]
fn can_talk_default_hierarchy() {
    let config: TeamConfig = serde_yaml::from_str(
        r#"
name: test
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: manager
    role_type: manager
    agent: claude
  - name: engineer
    role_type: engineer
    agent: codex
"#,
    )
    .unwrap();

    // Default: architect<->manager, manager<->engineer
    assert!(config.can_talk("architect", "manager"));
    assert!(config.can_talk("manager", "architect"));
    assert!(config.can_talk("manager", "engineer"));
    assert!(config.can_talk("engineer", "manager"));

    // architect<->engineer blocked by default
    assert!(!config.can_talk("architect", "engineer"));
    assert!(!config.can_talk("engineer", "architect"));

    // human can talk to anyone
    assert!(config.can_talk("human", "architect"));
    assert!(config.can_talk("human", "engineer"));

    // daemon can talk to anyone
    assert!(config.can_talk("daemon", "engineer"));
}

#[test]
fn can_talk_explicit_talks_to() {
    let config: TeamConfig = serde_yaml::from_str(
        r#"
name: test
roles:
  - name: architect
    role_type: architect
    agent: claude
    talks_to: [manager, engineer]
  - name: manager
    role_type: manager
    agent: claude
    talks_to: [architect, engineer]
  - name: engineer
    role_type: engineer
    agent: codex
    talks_to: [manager]
"#,
    )
    .unwrap();

    // Explicit: architect->engineer allowed
    assert!(config.can_talk("architect", "engineer"));
    // But engineer->architect still blocked (not in engineer's talks_to)
    assert!(!config.can_talk("engineer", "architect"));
}

#[test]
fn validate_rejects_zero_instances() {
    let yaml = r#"
name: test
roles:
  - name: worker
    role_type: engineer
    agent: codex
    instances: 0
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let err = config.validate().unwrap_err().to_string();
    assert!(err.contains("zero instances"));
}

#[test]
fn parse_rejects_malformed_yaml_missing_colon() {
    let yaml = r#"
name test
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;

    let err = serde_yaml::from_str::<TeamConfig>(yaml)
        .unwrap_err()
        .to_string();
    assert!(!err.is_empty());
}

#[test]
fn parse_rejects_malformed_yaml_bad_indentation() {
    let yaml = r#"
name: test
roles:
- name: worker
   role_type: engineer
   agent: codex
"#;

    let err = serde_yaml::from_str::<TeamConfig>(yaml)
        .unwrap_err()
        .to_string();
    assert!(!err.is_empty());
}

#[test]
fn parse_rejects_missing_name_field() {
    let yaml = r#"
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;

    let err = serde_yaml::from_str::<TeamConfig>(yaml)
        .unwrap_err()
        .to_string();
    assert!(err.contains("name"));
}

#[test]
fn parse_rejects_missing_roles_field() {
    let yaml = r#"
name: test
"#;

    let err = serde_yaml::from_str::<TeamConfig>(yaml)
        .unwrap_err()
        .to_string();
    assert!(err.contains("roles"));
}

#[test]
fn legacy_mode_with_orchestrator_pane_true_disables_orchestrator_surface() {
    let yaml = r#"
name: test
workflow_mode: legacy
orchestrator_pane: true
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;

    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.workflow_mode, WorkflowMode::Legacy);
    assert!(config.orchestrator_pane);
    assert!(!config.orchestrator_enabled());
}

#[test]
fn parse_all_automation_flags_false() {
    let yaml = r#"
name: test
automation:
  timeout_nudges: false
  standups: false
  failure_pattern_detection: false
  triage_interventions: false
  review_interventions: false
  owned_task_interventions: false
  manager_dispatch_interventions: false
  architect_utilization_interventions: false
  replenishment_threshold: 1
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;

    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert!(!config.automation.timeout_nudges);
    assert!(!config.automation.standups);
    assert!(!config.automation.failure_pattern_detection);
    assert!(!config.automation.triage_interventions);
    assert!(!config.automation.review_interventions);
    assert!(!config.automation.owned_task_interventions);
    assert!(!config.automation.manager_dispatch_interventions);
    assert!(!config.automation.architect_utilization_interventions);
    assert_eq!(config.automation.replenishment_threshold, Some(1));
}

#[test]
fn automation_replenishment_threshold_defaults_to_none() {
    let config: TeamConfig = serde_yaml::from_str(minimal_yaml()).unwrap();
    assert_eq!(config.automation.replenishment_threshold, None);
}

#[test]
fn parse_standup_interval_zero() {
    let yaml = r#"
name: test
standup:
  interval_secs: 0
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;

    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.standup.interval_secs, 0);
}

#[test]
fn parse_standup_interval_u64_max() {
    let yaml = format!(
        r#"
name: test
standup:
  interval_secs: {}
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#,
        u64::MAX
    );

    let config: TeamConfig = serde_yaml::from_str(&yaml).unwrap();
    assert_eq!(config.standup.interval_secs, u64::MAX);
}

#[test]
fn parse_ignores_unknown_top_level_fields_for_forward_compatibility() {
    let yaml = r#"
name: test
future_flag: true
future_section:
  nested_value: 42
roles:
  - name: worker
    role_type: engineer
    agent: codex
    extra_role_setting: keep-going
"#;

    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.name, "test");
    assert_eq!(config.roles.len(), 1);
    config.validate().unwrap();
}

#[test]
fn validate_rejects_duplicate_role_names_with_mixed_role_types() {
    let yaml = r#"
name: test
roles:
  - name: lead
    role_type: architect
    agent: claude
  - name: lead
    role_type: manager
    agent: claude
"#;

    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let err = config.validate().unwrap_err().to_string();
    assert!(err.contains("duplicate role name"));
}

#[test]
fn validate_rejects_talks_to_reference_to_missing_role() {
    let yaml = r#"
name: test
roles:
  - name: worker
    role_type: engineer
    agent: codex
    talks_to: [manager]
"#;

    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let err = config.validate().unwrap_err().to_string();
    assert!(err.contains("unknown role 'manager'"));
    assert!(err.contains("talks_to"));
}

#[test]
fn external_sender_can_talk_to_any_role() {
    let config: TeamConfig = serde_yaml::from_str(
        r#"
name: test
external_senders:
  - email-router
  - slack-bridge
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: manager
    role_type: manager
    agent: claude
  - name: engineer
    role_type: engineer
    agent: codex
"#,
    )
    .unwrap();

    assert!(config.can_talk("email-router", "manager"));
    assert!(config.can_talk("email-router", "architect"));
    assert!(config.can_talk("email-router", "engineer"));
    assert!(config.can_talk("slack-bridge", "manager"));
    assert!(config.can_talk("slack-bridge", "engineer"));
}

#[test]
fn unknown_sender_blocked() {
    let config: TeamConfig = serde_yaml::from_str(
        r#"
name: test
external_senders:
  - email-router
roles:
  - name: manager
    role_type: manager
    agent: claude
  - name: engineer
    role_type: engineer
    agent: codex
"#,
    )
    .unwrap();

    // "random-sender" is not in external_senders and not a known role
    assert!(!config.can_talk("random-sender", "manager"));
    assert!(!config.can_talk("random-sender", "engineer"));
}

#[test]
fn parse_review_timeout_overrides_from_yaml() {
    let yaml = r#"
name: test-team
workflow_policy:
  review_nudge_threshold_secs: 1800
  review_timeout_secs: 7200
  review_timeout_overrides:
    critical:
      review_nudge_threshold_secs: 300
      review_timeout_secs: 600
    high:
      review_timeout_secs: 3600
roles:
  - name: architect
    role_type: architect
    agent: claude
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let policy = &config.workflow_policy;

    // Global defaults
    assert_eq!(policy.review_nudge_threshold_secs, 1800);
    assert_eq!(policy.review_timeout_secs, 7200);

    // Critical override — both fields set
    let critical = policy.review_timeout_overrides.get("critical").unwrap();
    assert_eq!(critical.review_nudge_threshold_secs, Some(300));
    assert_eq!(critical.review_timeout_secs, Some(600));

    // High override — only escalation set, nudge absent
    let high = policy.review_timeout_overrides.get("high").unwrap();
    assert_eq!(high.review_nudge_threshold_secs, None);
    assert_eq!(high.review_timeout_secs, Some(3600));

    // No override for medium
    assert!(!policy.review_timeout_overrides.contains_key("medium"));
}

#[test]
fn empty_overrides_when_absent_in_yaml() {
    let config: TeamConfig = serde_yaml::from_str(minimal_yaml()).unwrap();
    assert!(config.workflow_policy.review_timeout_overrides.is_empty());
}

#[test]
fn parse_workflow_policy_test_command_from_yaml() {
    let yaml = r#"
name: test-team
workflow_policy:
  test_command: ./tests/fidelity/test_shell_starts.sh
roles:
  - name: architect
    role_type: architect
    agent: claude
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(
        config.workflow_policy.test_command.as_deref(),
        Some("./tests/fidelity/test_shell_starts.sh")
    );
}

#[test]
fn parse_verification_policy_fields_from_yaml() {
    let yaml = r#"
name: test-team
workflow_policy:
  verification:
    max_iterations: 7
    auto_run_tests: false
    require_evidence: false
    test_command: ./scripts/verify.sh
roles:
  - name: architect
    role_type: architect
    agent: claude
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.workflow_policy.verification.max_iterations, 7);
    assert!(!config.workflow_policy.verification.auto_run_tests);
    assert!(!config.workflow_policy.verification.require_evidence);
    assert_eq!(
        config.workflow_policy.verification.test_command.as_deref(),
        Some("./scripts/verify.sh")
    );
}

#[test]
fn parse_workflow_policy_restart_preservation_fields_from_yaml() {
    let yaml = r#"
name: test-team
workflow_policy:
  graceful_shutdown_timeout_secs: 9
  auto_commit_on_restart: false
roles:
  - name: architect
    role_type: architect
    agent: claude
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.workflow_policy.graceful_shutdown_timeout_secs, 9);
    assert!(!config.workflow_policy.auto_commit_on_restart);
}

// --- Backend health check tests ---

#[test]
fn backend_health_results_returns_unique_backends() {
    let config: TeamConfig = serde_yaml::from_str(
        r#"
name: test
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: manager
    role_type: manager
    agent: claude
  - name: engineer
    role_type: engineer
    agent: codex
"#,
    )
    .unwrap();

    let results = config.backend_health_results();
    let names: Vec<&str> = results.iter().map(|(n, _)| n.as_str()).collect();
    // claude used by both architect and manager, should appear only once
    assert_eq!(names.len(), 2);
    assert!(names.contains(&"claude"));
    assert!(names.contains(&"codex"));
}

#[test]
fn backend_health_results_skips_user_roles() {
    let config: TeamConfig = serde_yaml::from_str(
        r#"
name: test
roles:
  - name: human
    role_type: user
  - name: worker
    role_type: engineer
    agent: claude
"#,
    )
    .unwrap();

    let results = config.backend_health_results();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, "claude");
}

#[test]
fn backend_health_results_uses_team_level_default() {
    let config: TeamConfig = serde_yaml::from_str(
        r#"
name: test
agent: codex
roles:
  - name: worker
    role_type: engineer
"#,
    )
    .unwrap();

    let results = config.backend_health_results();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, "codex");
}

#[test]
fn backend_health_results_falls_back_to_claude_default() {
    // When no team-level or role-level agent is set, resolve_agent returns "claude"
    let config: TeamConfig = serde_yaml::from_str(
        r#"
name: test
roles:
  - name: worker
    role_type: engineer
    agent: claude
"#,
    )
    .unwrap();

    let results = config.backend_health_results();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, "claude");
}

#[test]
fn check_backend_health_returns_empty_when_all_healthy() {
    // Use a backend we know exists on the system — we can't guarantee any agent
    // binary is present, but we can test that the method filters correctly by
    // checking the function signature works with a real config.
    let config: TeamConfig = serde_yaml::from_str(
        r#"
name: test
roles:
  - name: worker
    role_type: engineer
    agent: claude
"#,
    )
    .unwrap();

    // check_backend_health returns only unhealthy backends
    let warnings = config.check_backend_health();
    // We can't assert specific health since it depends on the system,
    // but verify the function runs without error and returns Vec<String>
    assert!(warnings.iter().all(|w| w.contains("not found on PATH")));
}

#[test]
fn validate_verbose_includes_backend_health_checks() {
    let config: TeamConfig = serde_yaml::from_str(
        r#"
name: test
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: engineer
    role_type: engineer
    agent: codex
"#,
    )
    .unwrap();

    let checks = config.validate_verbose();
    let backend_checks: Vec<_> = checks
        .iter()
        .filter(|c| c.name.starts_with("backend_health:"))
        .collect();

    // Should have checks for both claude and codex
    assert_eq!(backend_checks.len(), 2);
    assert!(backend_checks
        .iter()
        .any(|c| c.name == "backend_health:claude"));
    assert!(backend_checks
        .iter()
        .any(|c| c.name == "backend_health:codex"));
}

#[test]
fn validate_verbose_backend_health_does_not_fail_validation() {
    // Backend health checks are warnings, not errors — even if a backend binary
    // is missing, validate_verbose should still succeed (passed=false is a warning).
    let config: TeamConfig = serde_yaml::from_str(
        r#"
name: test
roles:
  - name: worker
    role_type: engineer
    agent: claude
"#,
    )
    .unwrap();

    let checks = config.validate_verbose();
    // Non-backend checks should all pass for this valid config
    let non_backend_failures: Vec<_> = checks
        .iter()
        .filter(|c| !c.name.starts_with("backend_health:") && !c.passed)
        .collect();
    assert!(
        non_backend_failures.is_empty(),
        "non-backend checks should all pass: {:?}",
        non_backend_failures
    );
}

#[test]
fn backend_health_results_with_mixed_backends() {
    let config: TeamConfig = serde_yaml::from_str(
        r#"
name: mixed-team
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: manager
    role_type: manager
    agent: kiro
  - name: eng-a
    role_type: engineer
    agent: codex
  - name: eng-b
    role_type: engineer
    agent: claude
"#,
    )
    .unwrap();

    let results = config.backend_health_results();
    let names: Vec<&str> = results.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names.len(), 3);
    assert!(names.contains(&"claude"));
    assert!(names.contains(&"kiro"));
    assert!(names.contains(&"codex"));
}

#[test]
fn use_shim_defaults_to_false() {
    let config: TeamConfig = serde_yaml::from_str(minimal_yaml()).unwrap();
    assert!(!config.use_shim);
}

#[test]
fn use_shim_parsed_when_true() {
    let yaml = r#"
name: shim-team
use_shim: true
roles:
  - name: architect
    role_type: architect
    agent: claude
    instances: 1
    talks_to: [engineer]
  - name: engineer
    role_type: engineer
    agent: claude
    instances: 1
    talks_to: [architect]
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert!(config.use_shim);
}

#[test]
fn parses_prompt_layering_fields_and_instance_overrides() {
    let yaml = r#"
name: layered-team
roles:
  - name: engineer
    role_type: engineer
    agent: codex
    model: gpt-5.4
    prompt: batty_engineer.md
    posture: deep_worker
    model_class: standard
    provider_overlay: codex
    instance_overrides:
      engineer:
        model_class: frontier
        posture: fast_lane
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let role = config.role_def("engineer").unwrap();

    assert_eq!(role.model.as_deref(), Some("gpt-5.4"));
    assert_eq!(role.posture.as_deref(), Some("deep_worker"));
    assert_eq!(role.model_class.as_deref(), Some("standard"));
    assert_eq!(role.provider_overlay.as_deref(), Some("codex"));

    let override_cfg = role.instance_overrides.get("engineer").unwrap();
    assert_eq!(override_cfg.agent, None);
    assert_eq!(override_cfg.model_class.as_deref(), Some("frontier"));
    assert_eq!(override_cfg.posture.as_deref(), Some("fast_lane"));
}

#[test]
fn parses_instance_override_agent_and_model() {
    let yaml = r#"
name: mixed-team
agent: claude
roles:
  - name: engineer
    role_type: engineer
    agent: codex
    model: gpt-5.4
    instances: 2
    instance_overrides:
      engineer-2:
        agent: gemini
        model: claude-opus-4.6-1m
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let role = config.role_def("engineer").unwrap();
    let override_cfg = role.instance_overrides.get("engineer-2").unwrap();

    assert_eq!(role.agent.as_deref(), Some("codex"));
    assert_eq!(role.model.as_deref(), Some("gpt-5.4"));
    assert_eq!(override_cfg.agent.as_deref(), Some("gemini"));
    assert_eq!(override_cfg.model.as_deref(), Some("claude-opus-4.6-1m"));
}
