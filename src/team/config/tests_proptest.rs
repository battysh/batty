use super::*;
use proptest::prelude::*;

/// Valid agent backend names recognized by adapter_from_name.
const VALID_AGENTS: &[&str] = &[
    "claude",
    "claude-code",
    "codex",
    "codex-cli",
    "kiro",
    "kiro-cli",
];

/// Valid role type strings for YAML.
const VALID_ROLE_TYPES: &[&str] = &["user", "architect", "manager", "engineer"];

/// Valid workflow mode strings for YAML.
const VALID_WORKFLOW_MODES: &[&str] = &["legacy", "hybrid", "workflow_first"];

/// Valid orchestrator position strings for YAML.
const VALID_ORCH_POSITIONS: &[&str] = &["bottom", "left"];

/// Strategy for a valid agent name.
fn valid_agent() -> impl Strategy<Value = String> {
    proptest::sample::select(VALID_AGENTS).prop_map(|s| s.to_string())
}

/// Strategy for a valid role type.
fn valid_role_type() -> impl Strategy<Value = String> {
    proptest::sample::select(VALID_ROLE_TYPES).prop_map(|s| s.to_string())
}

/// Strategy for a safe YAML name (alphanumeric + hyphens, non-empty).
fn safe_name() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9\\-]{0,15}".prop_map(|s| s.to_string())
}

// 1. Random role count: valid configs with 1-10 roles never panic on parse
proptest! {
    #[test]
    fn valid_random_role_count_parses_without_panic(
        team_name in safe_name(),
        role_count in 1usize..=10,
    ) {
        let mut roles_yaml = String::new();
        for i in 0..role_count {
            let role_type = if i == 0 { "architect" } else { "engineer" };
            roles_yaml.push_str(&format!(
                "  - name: role-{i}\n    role_type: {role_type}\n    agent: claude\n"
            ));
        }
        let yaml = format!("name: {team_name}\nroles:\n{roles_yaml}");
        let result = serde_yaml::from_str::<TeamConfig>(&yaml);
        prop_assert!(result.is_ok(), "Failed to parse: {:?}", result.err());
        let config = result.unwrap();
        prop_assert_eq!(config.roles.len(), role_count);
    }
}

// 2. Random agent backends: all valid agent names parse successfully
proptest! {
    #[test]
    fn valid_agent_backend_parses(agent in valid_agent()) {
        let yaml = format!(
            "name: test\nroles:\n  - name: worker\n    role_type: engineer\n    agent: {agent}\n"
        );
        let config: TeamConfig = serde_yaml::from_str(&yaml).unwrap();
        prop_assert_eq!(config.roles[0].agent.as_deref(), Some(agent.as_str()));
    }
}

// 3. Random invalid agent names: parse succeeds but validate rejects
proptest! {
    #[test]
    fn invalid_agent_backend_rejected_by_validate(
        agent in "[a-z]{3,10}".prop_filter(
            "must not be a valid agent",
            |s| !VALID_AGENTS.contains(&s.as_str()),
        ),
    ) {
        let yaml = format!(
            "name: test\nroles:\n  - name: worker\n    role_type: engineer\n    agent: {agent}\n"
        );
        let config: TeamConfig = serde_yaml::from_str(&yaml).unwrap();
        let err = config.validate().unwrap_err().to_string();
        prop_assert!(err.contains("unknown agent"), "Error was: {err}");
    }
}

// 4. Team-level agent applied to roles without explicit agent
proptest! {
    #[test]
    fn team_level_agent_applied_to_agentless_roles(
        team_agent in valid_agent(),
        role_count in 1usize..=5,
    ) {
        let mut roles_yaml = String::new();
        for i in 0..role_count {
            let role_type = if i == 0 { "architect" } else { "engineer" };
            roles_yaml.push_str(&format!(
                "  - name: role-{i}\n    role_type: {role_type}\n"
            ));
        }
        let yaml = format!("name: test\nagent: {team_agent}\nroles:\n{roles_yaml}");
        let config: TeamConfig = serde_yaml::from_str(&yaml).unwrap();
        prop_assert!(config.validate().is_ok());
        for role in &config.roles {
            let resolved = config.resolve_agent(role);
            prop_assert_eq!(resolved.as_deref(), Some(team_agent.as_str()));
        }
    }
}

// 5. Random workflow policy values: never panic on parse
proptest! {
    #[test]
    fn random_workflow_policy_values_parse(
        wip_eng in proptest::option::of(0u32..100),
        wip_rev in proptest::option::of(0u32..100),
        escalation in 0u64..100_000,
        stall in 0u64..100_000,
        max_restarts in 0u32..20,
        health_interval in 0u64..10_000,
        uncommitted_warn in 0usize..1000,
    ) {
        let mut policy_yaml = String::from("workflow_policy:\n");
        if let Some(v) = wip_eng {
            policy_yaml.push_str(&format!("  wip_limit_per_engineer: {v}\n"));
        }
        if let Some(v) = wip_rev {
            policy_yaml.push_str(&format!("  wip_limit_per_reviewer: {v}\n"));
        }
        policy_yaml.push_str(&format!("  escalation_threshold_secs: {escalation}\n"));
        policy_yaml.push_str(&format!("  stall_threshold_secs: {stall}\n"));
        policy_yaml.push_str(&format!("  max_stall_restarts: {max_restarts}\n"));
        policy_yaml.push_str(&format!("  health_check_interval_secs: {health_interval}\n"));
        policy_yaml.push_str(&format!("  uncommitted_warn_threshold: {uncommitted_warn}\n"));

        let yaml = format!(
            "name: test\n{policy_yaml}roles:\n  - name: w\n    role_type: engineer\n    agent: codex\n"
        );
        let result = serde_yaml::from_str::<TeamConfig>(&yaml);
        prop_assert!(result.is_ok(), "Parse failed: {:?}", result.err());
        let config = result.unwrap();
        prop_assert_eq!(config.workflow_policy.escalation_threshold_secs, escalation);
        prop_assert_eq!(config.workflow_policy.stall_threshold_secs, stall);
        prop_assert_eq!(config.workflow_policy.max_stall_restarts, max_restarts);
    }
}

// 6. Missing optional fields: config parses with defaults
proptest! {
    #[test]
    fn missing_optional_fields_use_defaults(
        team_name in safe_name(),
        instances in 1u32..=20,
    ) {
        // Minimal config: only required fields (name, roles with name+role_type+agent)
        let yaml = format!(
            "name: {team_name}\nroles:\n  - name: w\n    role_type: engineer\n    agent: codex\n    instances: {instances}\n"
        );
        let config: TeamConfig = serde_yaml::from_str(&yaml).unwrap();
        // All optional fields should have defaults
        prop_assert_eq!(config.workflow_mode, WorkflowMode::Legacy);
        prop_assert!(config.orchestrator_pane);
        prop_assert_eq!(config.orchestrator_position, OrchestratorPosition::Bottom);
        prop_assert!(config.layout.is_none());
        prop_assert!(config.agent.is_none());
        prop_assert!(config.automation_sender.is_none());
        prop_assert!(config.external_senders.is_empty());
        prop_assert_eq!(config.board.rotation_threshold, 20);
        prop_assert!(config.board.auto_dispatch);
        prop_assert_eq!(config.roles[0].instances, instances);
    }
}

// 7. Extra unknown fields: serde ignores them (forward compatibility)
proptest! {
    #[test]
    fn extra_unknown_fields_ignored(
        extra_key in "[a-z_]{3,12}",
        extra_val in "[a-z0-9]{1,10}",
    ) {
        let yaml = format!(
            "name: test\n{extra_key}: {extra_val}\nroles:\n  - name: w\n    role_type: engineer\n    agent: codex\n    {extra_key}: {extra_val}\n"
        );
        let result = serde_yaml::from_str::<TeamConfig>(&yaml);
        prop_assert!(result.is_ok(), "Unknown fields should be ignored: {:?}", result.err());
    }
}

// 8. Random workflow mode: all valid modes parse correctly
proptest! {
    #[test]
    fn valid_workflow_modes_parse(
        mode_idx in 0usize..VALID_WORKFLOW_MODES.len(),
    ) {
        let mode = VALID_WORKFLOW_MODES[mode_idx];
        let yaml = format!(
            "name: test\nworkflow_mode: {mode}\nroles:\n  - name: w\n    role_type: engineer\n    agent: codex\n"
        );
        let config: TeamConfig = serde_yaml::from_str(&yaml).unwrap();
        match mode {
            "legacy" => prop_assert_eq!(config.workflow_mode, WorkflowMode::Legacy),
            "hybrid" => prop_assert_eq!(config.workflow_mode, WorkflowMode::Hybrid),
            "workflow_first" => prop_assert_eq!(config.workflow_mode, WorkflowMode::WorkflowFirst),
            _ => unreachable!(),
        }
    }
}

// 9. Invalid workflow modes: produce parse errors, not panics
proptest! {
    #[test]
    fn invalid_workflow_mode_produces_error(
        mode in "[a-z]{3,10}".prop_filter(
            "must not be valid",
            |s| !VALID_WORKFLOW_MODES.contains(&s.as_str()),
        ),
    ) {
        let yaml = format!(
            "name: test\nworkflow_mode: {mode}\nroles:\n  - name: w\n    role_type: engineer\n    agent: codex\n"
        );
        let result = serde_yaml::from_str::<TeamConfig>(&yaml);
        prop_assert!(result.is_err(), "Should reject invalid workflow_mode '{mode}'");
    }
}

// 10. Random orchestrator position: valid ones parse, invalid error
proptest! {
    #[test]
    fn valid_orchestrator_positions_parse(
        pos_idx in 0usize..VALID_ORCH_POSITIONS.len(),
    ) {
        let pos = VALID_ORCH_POSITIONS[pos_idx];
        let yaml = format!(
            "name: test\norchestrator_position: {pos}\nroles:\n  - name: w\n    role_type: engineer\n    agent: codex\n"
        );
        let config: TeamConfig = serde_yaml::from_str(&yaml).unwrap();
        match pos {
            "bottom" => prop_assert_eq!(config.orchestrator_position, OrchestratorPosition::Bottom),
            "left" => prop_assert_eq!(config.orchestrator_position, OrchestratorPosition::Left),
            _ => unreachable!(),
        }
    }
}

// 11. Layout zone widths: parsing never panics, validation catches >100%
proptest! {
    #[test]
    fn layout_zone_widths_parse_and_validate(
        zone_count in 1usize..=6,
        width in 0u32..=100,
    ) {
        let mut zones_yaml = String::new();
        for i in 0..zone_count {
            zones_yaml.push_str(&format!(
                "    - name: zone-{i}\n      width_pct: {width}\n"
            ));
        }
        let yaml = format!(
            "name: test\nlayout:\n  zones:\n{zones_yaml}roles:\n  - name: w\n    role_type: engineer\n    agent: codex\n"
        );
        let config: TeamConfig = serde_yaml::from_str(&yaml).unwrap();
        let total: u32 = config.layout.as_ref().unwrap().zones.iter().map(|z| z.width_pct).sum();
        if total > 100 {
            prop_assert!(config.validate().is_err());
        } else {
            prop_assert!(config.validate().is_ok());
        }
    }
}

// 12. Random automation config booleans: never panic
proptest! {
    #[test]
    fn random_automation_booleans_parse(
        nudges in proptest::bool::ANY,
        standups in proptest::bool::ANY,
        failure_det in proptest::bool::ANY,
        triage in proptest::bool::ANY,
        review in proptest::bool::ANY,
        owned in proptest::bool::ANY,
        dispatch in proptest::bool::ANY,
        arch_util in proptest::bool::ANY,
    ) {
        let yaml = format!(
            "name: test\nautomation:\n  timeout_nudges: {nudges}\n  standups: {standups}\n  failure_pattern_detection: {failure_det}\n  triage_interventions: {triage}\n  review_interventions: {review}\n  owned_task_interventions: {owned}\n  manager_dispatch_interventions: {dispatch}\n  architect_utilization_interventions: {arch_util}\nroles:\n  - name: w\n    role_type: engineer\n    agent: codex\n"
        );
        let config: TeamConfig = serde_yaml::from_str(&yaml).unwrap();
        prop_assert_eq!(config.automation.timeout_nudges, nudges);
        prop_assert_eq!(config.automation.standups, standups);
        prop_assert_eq!(config.automation.failure_pattern_detection, failure_det);
        prop_assert_eq!(config.automation.triage_interventions, triage);
        prop_assert_eq!(config.automation.review_interventions, review);
        prop_assert_eq!(config.automation.owned_task_interventions, owned);
        prop_assert_eq!(config.automation.manager_dispatch_interventions, dispatch);
        prop_assert_eq!(config.automation.architect_utilization_interventions, arch_util);
    }
}

// 13. Random standup/board config values: parse without panic
proptest! {
    #[test]
    fn random_standup_and_board_values_parse(
        interval in 0u64..=1_000_000,
        output_lines in 0u32..=500,
        rotation in 0u32..=1000,
        auto_dispatch in proptest::bool::ANY,
    ) {
        let yaml = format!(
            "name: test\nstandup:\n  interval_secs: {interval}\n  output_lines: {output_lines}\nboard:\n  rotation_threshold: {rotation}\n  auto_dispatch: {auto_dispatch}\nroles:\n  - name: w\n    role_type: engineer\n    agent: codex\n"
        );
        let config: TeamConfig = serde_yaml::from_str(&yaml).unwrap();
        prop_assert_eq!(config.standup.interval_secs, interval);
        prop_assert_eq!(config.standup.output_lines, output_lines);
        prop_assert_eq!(config.board.rotation_threshold, rotation);
        prop_assert_eq!(config.board.auto_dispatch, auto_dispatch);
    }
}

// 14. Random role type: all valid types parse
proptest! {
    #[test]
    fn all_role_types_parse(role_type in valid_role_type()) {
        let agent_line = if role_type == "user" { "" } else { "    agent: claude\n" };
        let yaml = format!(
            "name: test\nroles:\n  - name: r\n    role_type: {role_type}\n{agent_line}"
        );
        let config: TeamConfig = serde_yaml::from_str(&yaml).unwrap();
        prop_assert_eq!(config.roles.len(), 1);
    }
}

// 15. Auto-merge policy random values: never panic
proptest! {
    #[test]
    fn random_auto_merge_policy_parses(
        enabled in proptest::bool::ANY,
        max_diff in 0usize..10_000,
        max_files in 0usize..100,
        max_modules in 0usize..50,
        confidence in 0.0f64..=1.0,
        require_tests in proptest::bool::ANY,
    ) {
        let yaml = format!(
            "name: test\nworkflow_policy:\n  auto_merge:\n    enabled: {enabled}\n    max_diff_lines: {max_diff}\n    max_files_changed: {max_files}\n    max_modules_touched: {max_modules}\n    confidence_threshold: {confidence}\n    require_tests_pass: {require_tests}\nroles:\n  - name: w\n    role_type: engineer\n    agent: codex\n"
        );
        let result = serde_yaml::from_str::<TeamConfig>(&yaml);
        prop_assert!(result.is_ok(), "Parse failed: {:?}", result.err());
        let config = result.unwrap();
        prop_assert_eq!(config.workflow_policy.auto_merge.enabled, enabled);
        prop_assert_eq!(config.workflow_policy.auto_merge.max_diff_lines, max_diff);
        prop_assert_eq!(config.workflow_policy.auto_merge.max_files_changed, max_files);
        prop_assert_eq!(config.workflow_policy.auto_merge.require_tests_pass, require_tests);
    }
}

// 16. Completely random YAML strings: never panic (errors OK)
proptest! {
    #[test]
    fn arbitrary_yaml_never_panics(yaml in "\\PC{0,200}") {
        // Parsing arbitrary bytes should either succeed or return Err, never panic
        let _ = serde_yaml::from_str::<TeamConfig>(&yaml);
    }
}
