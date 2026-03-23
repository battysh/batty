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

// --- Mixed-backend / team-level agent tests ---

#[test]
fn team_level_agent_parsed() {
    let yaml = r#"
name: test
agent: codex
roles:
  - name: worker
    role_type: engineer
    instances: 2
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.agent.as_deref(), Some("codex"));
    assert!(config.validate().is_ok());
}

#[test]
fn team_level_agent_absent_defaults_to_none() {
    let config: TeamConfig = serde_yaml::from_str(minimal_yaml()).unwrap();
    assert!(config.agent.is_none());
}

#[test]
fn resolve_agent_role_overrides_team() {
    let yaml = r#"
name: test
agent: codex
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: worker
    role_type: engineer
    instances: 2
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let architect = &config.roles[0];
    let worker = &config.roles[1];
    // Role-level agent overrides team-level
    assert_eq!(config.resolve_agent(architect).as_deref(), Some("claude"));
    // No role-level agent, falls back to team-level
    assert_eq!(config.resolve_agent(worker).as_deref(), Some("codex"));
}

#[test]
fn resolve_agent_defaults_to_claude_when_nothing_set() {
    let yaml = r#"
name: test
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    // Override the role to have no agent for testing
    let mut role = config.roles[0].clone();
    role.agent = None;
    let mut config_no_team = config.clone();
    config_no_team.agent = None;
    assert_eq!(
        config_no_team.resolve_agent(&role).as_deref(),
        Some("claude")
    );
}

#[test]
fn resolve_agent_returns_none_for_user() {
    let yaml = r#"
name: test
agent: codex
roles:
  - name: human
    role_type: user
  - name: worker
    role_type: engineer
    instances: 1
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let user = &config.roles[0];
    assert!(config.resolve_agent(user).is_none());
}

#[test]
fn validate_team_level_agent_rejects_unknown() {
    let yaml = r#"
name: test
agent: mystery
roles:
  - name: worker
    role_type: engineer
    instances: 1
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let err = config.validate().unwrap_err().to_string();
    assert!(err.contains("team-level agent"));
    assert!(err.contains("mystery"));
}

#[test]
fn validate_accepts_team_level_agent_without_role_agent() {
    let yaml = r#"
name: test
agent: codex
roles:
  - name: architect
    role_type: architect
  - name: manager
    role_type: manager
  - name: engineer
    role_type: engineer
    instances: 2
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert!(config.validate().is_ok());
}

#[test]
fn validate_rejects_no_agent_at_any_level() {
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
fn validate_mixed_backend_team() {
    let yaml = r#"
name: mixed
agent: codex
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: manager
    role_type: manager
    agent: claude
  - name: eng-claude
    role_type: engineer
    agent: claude
    instances: 2
    talks_to: [manager]
  - name: eng-codex
    role_type: engineer
    instances: 2
    talks_to: [manager]
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert!(config.validate().is_ok());
    // eng-claude has explicit agent
    assert_eq!(config.roles[2].agent.as_deref(), Some("claude"));
    // eng-codex inherits team default
    assert!(config.roles[3].agent.is_none());
    assert_eq!(
        config.resolve_agent(&config.roles[3]).as_deref(),
        Some("codex")
    );
}

// --- Edge case tests: missing fields ---

#[test]
fn validate_rejects_empty_roles_list() {
    let yaml = r#"
name: test
roles: []
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let err = config.validate().unwrap_err().to_string();
    assert!(err.contains("at least one role"));
}

#[test]
fn validate_rejects_role_with_empty_name() {
    let yaml = r#"
name: test
roles:
  - name: ""
    role_type: engineer
    agent: codex
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let err = config.validate().unwrap_err().to_string();
    assert!(err.contains("empty name"));
}

#[test]
fn validate_rejects_two_roles_with_empty_names() {
    let yaml = r#"
name: test
roles:
  - name: ""
    role_type: engineer
    agent: codex
  - name: ""
    role_type: manager
    agent: claude
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let err = config.validate().unwrap_err().to_string();
    // Now fails at the first empty name before reaching duplicate check
    assert!(err.contains("empty name"));
}

// --- Edge case tests: wrong types in YAML ---

#[test]
fn parse_rejects_string_for_instances() {
    let yaml = r#"
name: test
roles:
  - name: worker
    role_type: engineer
    agent: codex
    instances: many
"#;
    assert!(serde_yaml::from_str::<TeamConfig>(yaml).is_err());
}

#[test]
fn parse_rejects_boolean_for_name() {
    let yaml = r#"
name: true
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
    // YAML coerces true to "true" string — should parse but name is "true"
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.name, "true");
}

#[test]
fn parse_null_name_deserializes_as_literal_string() {
    let yaml = r#"
name: null
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
    // serde_yaml 0.9 deserializes YAML null as literal "null" for String fields
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.name, "null");
}

#[test]
fn parse_tilde_name_deserializes_as_tilde_string() {
    let yaml = r#"
name: ~
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
    // serde_yaml 0.9 coerces ~ (YAML null) to "~" for non-Option String fields
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.name, "~");
}

#[test]
fn parse_rejects_invalid_role_type() {
    let yaml = r#"
name: test
roles:
  - name: wizard
    role_type: wizard
    agent: claude
"#;
    let err = serde_yaml::from_str::<TeamConfig>(yaml)
        .unwrap_err()
        .to_string();
    assert!(!err.is_empty());
}

#[test]
fn parse_rejects_invalid_workflow_mode() {
    let yaml = r#"
name: test
workflow_mode: turbo
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
    assert!(serde_yaml::from_str::<TeamConfig>(yaml).is_err());
}

#[test]
fn parse_rejects_invalid_orchestrator_position() {
    let yaml = r#"
name: test
orchestrator_position: top
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
    assert!(serde_yaml::from_str::<TeamConfig>(yaml).is_err());
}

#[test]
fn parse_rejects_negative_instances() {
    let yaml = r#"
name: test
roles:
  - name: worker
    role_type: engineer
    agent: codex
    instances: -1
"#;
    assert!(serde_yaml::from_str::<TeamConfig>(yaml).is_err());
}

#[test]
fn parse_rejects_string_for_interval_secs() {
    let yaml = r#"
name: test
standup:
  interval_secs: forever
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
    assert!(serde_yaml::from_str::<TeamConfig>(yaml).is_err());
}

// --- Edge case tests: hierarchy and talks_to ---

#[test]
fn can_talk_role_to_self_via_talks_to() {
    let config: TeamConfig = serde_yaml::from_str(
        r#"
name: test
roles:
  - name: solo
    role_type: architect
    agent: claude
    talks_to: [solo]
"#,
    )
    .unwrap();
    // Self-referencing talks_to — allowed by current rules, role can talk to itself
    assert!(config.can_talk("solo", "solo"));
    config.validate().unwrap(); // Should not crash
}

#[test]
fn can_talk_nonexistent_sender_returns_false() {
    let config: TeamConfig = serde_yaml::from_str(minimal_yaml()).unwrap();
    assert!(!config.can_talk("ghost", "architect"));
}

#[test]
fn can_talk_nonexistent_target_returns_false() {
    let config: TeamConfig = serde_yaml::from_str(minimal_yaml()).unwrap();
    assert!(!config.can_talk("architect", "ghost"));
}

#[test]
fn validate_accepts_single_user_only_team() {
    let yaml = r#"
name: test
roles:
  - name: human
    role_type: user
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    // User roles don't need agents — should be valid
    config.validate().unwrap();
}

#[test]
fn validate_accepts_multiple_user_roles() {
    let yaml = r#"
name: test
roles:
  - name: alice
    role_type: user
  - name: bob
    role_type: user
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    config.validate().unwrap();
}

// --- Edge case tests: boundary values ---

#[test]
fn validate_accepts_large_instance_count() {
    let yaml = r#"
name: test
roles:
  - name: army
    role_type: engineer
    agent: codex
    instances: 100
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.roles[0].instances, 100);
    config.validate().unwrap();
}

#[test]
fn parse_workflow_policy_zero_wip_limits() {
    let yaml = r#"
name: test
workflow_policy:
  wip_limit_per_engineer: 0
  wip_limit_per_reviewer: 0
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.workflow_policy.wip_limit_per_engineer, Some(0));
    assert_eq!(config.workflow_policy.wip_limit_per_reviewer, Some(0));
}

#[test]
fn parse_workflow_policy_defaults_all_applied() {
    let config: TeamConfig = serde_yaml::from_str(minimal_yaml()).unwrap();
    let p = &config.workflow_policy;
    assert!(p.wip_limit_per_engineer.is_none());
    assert!(p.wip_limit_per_reviewer.is_none());
    assert_eq!(p.pipeline_starvation_threshold, Some(1));
    assert_eq!(p.escalation_threshold_secs, 3600);
    assert_eq!(p.review_nudge_threshold_secs, 1800);
    assert_eq!(p.review_timeout_secs, 7200);
    assert!(p.review_timeout_overrides.is_empty());
    assert!(p.auto_archive_done_after_secs.is_none());
    assert!(p.capability_overrides.is_empty());
    assert_eq!(p.stall_threshold_secs, 300);
    assert_eq!(p.max_stall_restarts, 2);
    assert_eq!(p.health_check_interval_secs, 60);
    assert_eq!(p.uncommitted_warn_threshold, 200);
}

#[test]
fn parse_workflow_policy_zero_escalation_threshold() {
    let yaml = r#"
name: test
workflow_policy:
  escalation_threshold_secs: 0
  stall_threshold_secs: 0
  max_stall_restarts: 0
  health_check_interval_secs: 0
  uncommitted_warn_threshold: 0
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.workflow_policy.escalation_threshold_secs, 0);
    assert_eq!(config.workflow_policy.stall_threshold_secs, 0);
    assert_eq!(config.workflow_policy.max_stall_restarts, 0);
    assert_eq!(config.workflow_policy.health_check_interval_secs, 0);
    assert_eq!(config.workflow_policy.uncommitted_warn_threshold, 0);
}

#[test]
fn validate_layout_zones_exactly_100_pct() {
    let yaml = r#"
name: test
layout:
  zones:
    - name: left
      width_pct: 50
    - name: right
      width_pct: 50
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    config.validate().unwrap();
}

#[test]
fn validate_layout_empty_zones_accepted() {
    let yaml = r#"
name: test
layout:
  zones: []
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    config.validate().unwrap();
}

#[test]
fn validate_layout_zone_width_zero() {
    let yaml = r#"
name: test
layout:
  zones:
    - name: invisible
      width_pct: 0
    - name: full
      width_pct: 100
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    config.validate().unwrap();
}

// --- Edge case tests: auto-merge policy ---

#[test]
fn parse_auto_merge_policy_defaults() {
    let config: TeamConfig = serde_yaml::from_str(minimal_yaml()).unwrap();
    let am = &config.workflow_policy.auto_merge;
    assert!(!am.enabled);
    assert_eq!(am.max_diff_lines, 200);
    assert_eq!(am.max_files_changed, 5);
    assert_eq!(am.max_modules_touched, 2);
    assert_eq!(am.confidence_threshold, 0.8);
    assert!(am.require_tests_pass);
    assert!(!am.sensitive_paths.is_empty());
}

#[test]
fn parse_auto_merge_policy_custom() {
    let yaml = r#"
name: test
workflow_policy:
  auto_merge:
    enabled: true
    max_diff_lines: 50
    max_files_changed: 2
    max_modules_touched: 1
    confidence_threshold: 0.95
    require_tests_pass: false
    sensitive_paths: ["secrets.yaml"]
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let am = &config.workflow_policy.auto_merge;
    assert!(am.enabled);
    assert_eq!(am.max_diff_lines, 50);
    assert_eq!(am.max_files_changed, 2);
    assert_eq!(am.max_modules_touched, 1);
    assert_eq!(am.confidence_threshold, 0.95);
    assert!(!am.require_tests_pass);
    assert_eq!(am.sensitive_paths, vec!["secrets.yaml"]);
}

#[test]
fn parse_auto_merge_zero_thresholds() {
    let yaml = r#"
name: test
workflow_policy:
  auto_merge:
    enabled: true
    max_diff_lines: 0
    max_files_changed: 0
    max_modules_touched: 0
    confidence_threshold: 0.0
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let am = &config.workflow_policy.auto_merge;
    assert_eq!(am.max_diff_lines, 0);
    assert_eq!(am.max_files_changed, 0);
    assert_eq!(am.max_modules_touched, 0);
    assert_eq!(am.confidence_threshold, 0.0);
}

// --- Edge case tests: cost config ---

#[test]
fn parse_cost_config_empty_models_map() {
    let yaml = r#"
name: test
cost:
  models: {}
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert!(config.cost.models.is_empty());
}

#[test]
fn parse_cost_config_zero_pricing() {
    let yaml = r#"
name: test
cost:
  models:
    free-model:
      input_usd_per_mtok: 0.0
      output_usd_per_mtok: 0.0
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let model = config.cost.models.get("free-model").unwrap();
    assert_eq!(model.input_usd_per_mtok, 0.0);
    assert_eq!(model.output_usd_per_mtok, 0.0);
}

// --- Edge case tests: orchestrator position ---

#[test]
fn parse_orchestrator_position_left() {
    let yaml = r#"
name: test
orchestrator_position: left
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.orchestrator_position, OrchestratorPosition::Left);
}

#[test]
fn parse_orchestrator_position_defaults_to_bottom() {
    let config: TeamConfig = serde_yaml::from_str(minimal_yaml()).unwrap();
    assert_eq!(config.orchestrator_position, OrchestratorPosition::Bottom);
}

// --- Edge case tests: event log and retro ---

#[test]
fn parse_event_log_max_bytes_zero() {
    let yaml = r#"
name: test
event_log_max_bytes: 0
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.event_log_max_bytes, 0);
}

#[test]
fn parse_retro_min_duration_zero() {
    let yaml = r#"
name: test
retro_min_duration_secs: 0
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.retro_min_duration_secs, 0);
}

// --- Edge case tests: planning directives ---

#[test]
fn load_planning_directive_returns_none_for_empty_file() {
    let tmp = tempfile::tempdir().unwrap();
    let config_dir = tmp.path().join(".batty").join("team_config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("review_policy.md"), "").unwrap();

    let loaded =
        load_planning_directive(tmp.path(), PlanningDirectiveFile::ReviewPolicy, 120).unwrap();
    assert_eq!(loaded, None);
}

#[test]
fn load_planning_directive_returns_none_for_whitespace_only() {
    let tmp = tempfile::tempdir().unwrap();
    let config_dir = tmp.path().join(".batty").join("team_config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("review_policy.md"), "   \n  \n  ").unwrap();

    let loaded =
        load_planning_directive(tmp.path(), PlanningDirectiveFile::ReviewPolicy, 120).unwrap();
    assert_eq!(loaded, None);
}

#[test]
fn load_planning_directive_truncation_boundary_exact_length() {
    let tmp = tempfile::tempdir().unwrap();
    let config_dir = tmp.path().join(".batty").join("team_config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("review_policy.md"), "abcde").unwrap();

    // Exact length — no truncation
    let loaded = load_planning_directive(tmp.path(), PlanningDirectiveFile::ReviewPolicy, 5)
        .unwrap()
        .unwrap();
    assert_eq!(loaded, "abcde");
    assert!(!loaded.contains("truncated"));
}

#[test]
fn load_planning_directive_truncation_boundary_one_over() {
    let tmp = tempfile::tempdir().unwrap();
    let config_dir = tmp.path().join(".batty").join("team_config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("review_policy.md"), "abcdef").unwrap();

    // One char over — truncated
    let loaded = load_planning_directive(tmp.path(), PlanningDirectiveFile::ReviewPolicy, 5)
        .unwrap()
        .unwrap();
    assert!(loaded.starts_with("abcde"));
    assert!(loaded.contains("truncated"));
}

// --- Edge case tests: capability overrides ---

#[test]
fn parse_capability_overrides() {
    let yaml = r#"
name: test
workflow_policy:
  capability_overrides:
    engineer:
      - review
      - merge
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let overrides = &config.workflow_policy.capability_overrides;
    assert_eq!(
        overrides.get("engineer").unwrap(),
        &vec!["review".to_string(), "merge".to_string()]
    );
}

#[test]
fn parse_capability_overrides_empty() {
    let config: TeamConfig = serde_yaml::from_str(minimal_yaml()).unwrap();
    assert!(config.workflow_policy.capability_overrides.is_empty());
}

// --- Edge case tests: board config boundaries ---

#[test]
fn parse_board_config_zero_thresholds() {
    let yaml = r#"
name: test
board:
  rotation_threshold: 0
  dispatch_stabilization_delay_secs: 0
  dispatch_dedup_window_secs: 0
  dispatch_manual_cooldown_secs: 0
roles:
  - name: worker
    role_type: engineer
    agent: codex
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.board.rotation_threshold, 0);
    assert_eq!(config.board.dispatch_stabilization_delay_secs, 0);
    assert_eq!(config.board.dispatch_dedup_window_secs, 0);
    assert_eq!(config.board.dispatch_manual_cooldown_secs, 0);
}

// --- Edge case tests: load from file errors ---

#[test]
fn load_from_nonexistent_file_returns_error() {
    let result = TeamConfig::load(std::path::Path::new("/nonexistent/path/team.yaml"));
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("failed to read"));
}

#[test]
fn load_from_invalid_yaml_file_returns_error() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("team.yaml");
    std::fs::write(&path, "{{{{not valid yaml}}}}").unwrap();
    let result = TeamConfig::load(&path);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("failed to parse"));
}

#[test]
fn load_from_empty_file_returns_error() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("team.yaml");
    std::fs::write(&path, "").unwrap();
    let result = TeamConfig::load(&path);
    assert!(result.is_err());
}

// --- Task #291: Config validation improvements ---

#[test]
fn invalid_talks_to_error_shows_defined_roles() {
    let yaml = r#"
name: test
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: engineer
    role_type: engineer
    agent: codex
    talks_to: [nonexistent]
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let err = config.validate().unwrap_err().to_string();
    assert!(
        err.contains("references unknown role 'nonexistent'"),
        "expected unknown role message, got: {err}"
    );
    assert!(
        err.contains("defined roles:"),
        "expected defined roles list, got: {err}"
    );
    assert!(
        err.contains("architect"),
        "expected architect in defined roles, got: {err}"
    );
    assert!(
        err.contains("engineer"),
        "expected engineer in defined roles, got: {err}"
    );
}

#[test]
fn missing_field_error_lists_valid_agents() {
    let yaml = r#"
name: test
roles:
  - name: worker
    role_type: engineer
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let err = config.validate().unwrap_err().to_string();
    assert!(
        err.contains("no agent configured"),
        "expected missing agent message, got: {err}"
    );
    assert!(
        err.contains("valid agents:"),
        "expected valid agents list, got: {err}"
    );
    assert!(
        err.contains("claude"),
        "expected claude in valid agents, got: {err}"
    );
}

#[test]
fn unknown_backend_error_lists_valid_agents() {
    let yaml = r#"
name: test
roles:
  - name: worker
    role_type: engineer
    agent: gpt4
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let err = config.validate().unwrap_err().to_string();
    assert!(
        err.contains("unknown agent 'gpt4'"),
        "expected unknown agent message, got: {err}"
    );
    assert!(
        err.contains("valid agents:"),
        "expected valid agents list, got: {err}"
    );
    assert!(err.contains("claude"), "expected claude listed, got: {err}");
    assert!(err.contains("codex"), "expected codex listed, got: {err}");
    assert!(err.contains("kiro"), "expected kiro listed, got: {err}");
}

#[test]
fn verbose_shows_checks_all_pass() {
    let yaml = r#"
name: test-team
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: engineer
    role_type: engineer
    agent: codex
    talks_to: [architect]
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let checks = config.validate_verbose();

    assert!(!checks.is_empty(), "expected at least one check");
    assert!(
        checks.iter().all(|c| c.passed),
        "expected all checks to pass, failures: {:?}",
        checks.iter().filter(|c| !c.passed).collect::<Vec<_>>()
    );

    // Verify specific checks are present
    assert!(
        checks.iter().any(|c| c.name == "team_name"),
        "expected team_name check"
    );
    assert!(
        checks.iter().any(|c| c.name == "roles_present"),
        "expected roles_present check"
    );
    assert!(
        checks.iter().any(|c| c.name == "team_agent"),
        "expected team_agent check"
    );
    assert!(
        checks.iter().any(|c| c.name.starts_with("role_unique:")),
        "expected role_unique check"
    );
    assert!(
        checks.iter().any(|c| c.name.starts_with("role_agent:")),
        "expected role_agent check"
    );
    assert!(
        checks.iter().any(|c| c.name.starts_with("talks_to:")),
        "expected talks_to check"
    );
}

#[test]
fn verbose_shows_checks_with_failures() {
    let yaml = r#"
name: test
roles:
  - name: worker
    role_type: engineer
    agent: mystery
"#;
    let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
    let checks = config.validate_verbose();

    let failed: Vec<_> = checks.iter().filter(|c| !c.passed).collect();
    assert!(!failed.is_empty(), "expected at least one failing check");
    assert!(
        failed.iter().any(|c| c.name.contains("role_agent_valid")),
        "expected role_agent_valid failure, failures: {:?}",
        failed
    );
    assert!(
        failed.iter().any(|c| c.detail.contains("unknown agent")),
        "expected unknown agent detail in failure"
    );
}
