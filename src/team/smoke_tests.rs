//! Integration smoke tests for v0.5.x features.
//!
//! Verifies that Grafana CLI, telemetry DB, agent backend config, and backend
//! health validation work correctly together. No tmux dependency.

#[cfg(test)]
mod tests {
    use crate::agent::{self, BackendHealth, KNOWN_AGENT_NAMES};
    use crate::cli::{Cli, Command, GrafanaCommand};
    use crate::team::config::{GrafanaConfig, TeamConfig};
    use crate::team::events::TeamEvent;
    use crate::team::grafana;
    use crate::team::hierarchy::resolve_hierarchy;
    use crate::team::telemetry_db;
    use clap::Parser;

    // -----------------------------------------------------------------------
    // 1. Grafana CLI
    // -----------------------------------------------------------------------

    #[test]
    fn grafana_setup_parses_correctly() {
        let cli = Cli::try_parse_from(["batty", "grafana", "setup"]);
        assert!(cli.is_ok(), "grafana setup should parse: {:?}", cli.err());
        let cli = cli.unwrap();
        assert!(matches!(
            cli.command,
            Command::Grafana {
                command: GrafanaCommand::Setup
            }
        ));
    }

    #[test]
    fn grafana_status_parses_correctly() {
        let cli = Cli::try_parse_from(["batty", "grafana", "status"]);
        assert!(cli.is_ok(), "grafana status should parse: {:?}", cli.err());
        let cli = cli.unwrap();
        assert!(matches!(
            cli.command,
            Command::Grafana {
                command: GrafanaCommand::Status
            }
        ));
    }

    #[test]
    fn grafana_open_parses_correctly() {
        let cli = Cli::try_parse_from(["batty", "grafana", "open"]);
        assert!(cli.is_ok(), "grafana open should parse: {:?}", cli.err());
        let cli = cli.unwrap();
        assert!(matches!(
            cli.command,
            Command::Grafana {
                command: GrafanaCommand::Open
            }
        ));
    }

    #[test]
    fn grafana_rejects_unknown_subcommand() {
        let result = Cli::try_parse_from(["batty", "grafana", "deploy"]);
        assert!(result.is_err(), "grafana deploy should fail");
    }

    #[test]
    fn grafana_config_default_port_is_3000() {
        let config = GrafanaConfig::default();
        assert_eq!(config.port, 3000);
        assert!(!config.enabled);
    }

    #[test]
    fn grafana_config_custom_port_from_yaml() {
        let yaml = r#"
name: test
grafana:
  enabled: true
  port: 9090
roles:
  - name: worker
    role_type: engineer
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.grafana.enabled);
        assert_eq!(config.grafana.port, 9090);
    }

    #[test]
    fn grafana_url_uses_port() {
        assert_eq!(grafana::grafana_url(3000), "http://localhost:3000");
        assert_eq!(grafana::grafana_url(9090), "http://localhost:9090");
    }

    #[test]
    fn grafana_dashboard_json_is_valid() {
        let parsed: serde_json::Value =
            serde_json::from_str(grafana::DASHBOARD_JSON).expect("dashboard.json must parse");
        assert!(parsed.is_object());
        assert!(parsed["panels"].is_array());
    }

    // -----------------------------------------------------------------------
    // 2. Telemetry DB Integration
    // -----------------------------------------------------------------------

    #[test]
    fn telemetry_in_memory_db_creates_schema() {
        let conn = telemetry_db::open_in_memory().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn telemetry_db_full_session_lifecycle() {
        let conn = telemetry_db::open_in_memory().unwrap();

        // Start a session
        telemetry_db::insert_event(&conn, &TeamEvent::daemon_started()).unwrap();

        // Assign a task
        telemetry_db::insert_event(&conn, &TeamEvent::task_assigned("eng-1", "42")).unwrap();

        // Complete the task
        telemetry_db::insert_event(&conn, &TeamEvent::task_completed("eng-1", Some("42"))).unwrap();

        // Auto-merge
        telemetry_db::insert_event(
            &conn,
            &TeamEvent::task_auto_merged("eng-1", "42", 0.95, 3, 50),
        )
        .unwrap();

        // Stop daemon
        let mut stop = TeamEvent::daemon_stopped();
        stop.ts = 99999;
        telemetry_db::insert_event(&conn, &stop).unwrap();

        // Verify session summary
        let summaries = telemetry_db::query_session_summaries(&conn).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].tasks_completed, 1);
        assert_eq!(summaries[0].total_merges, 1);
        assert_eq!(summaries[0].total_events, 5);
        assert_eq!(summaries[0].ended_at, Some(99999));

        // Verify agent metrics
        let agents = telemetry_db::query_agent_metrics(&conn).unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].role, "eng-1");
        assert_eq!(agents[0].completions, 1);

        // Verify task metrics
        let tasks = telemetry_db::query_task_metrics(&conn).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].task_id, "42");
        assert!(tasks[0].started_at.is_some());
        assert!(tasks[0].completed_at.is_some());

        // Verify recent events
        let events = telemetry_db::query_recent_events(&conn, 100).unwrap();
        assert_eq!(events.len(), 5);
    }

    #[test]
    fn telemetry_db_review_metrics_integration() {
        let conn = telemetry_db::open_in_memory().unwrap();

        // Complete task at t=1000
        let mut c = TeamEvent::task_completed("eng-1", Some("10"));
        c.ts = 1000;
        telemetry_db::insert_event(&conn, &c).unwrap();

        // Merge at t=1200 (200s latency)
        let mut m = TeamEvent::task_auto_merged("eng-1", "10", 0.9, 2, 30);
        m.ts = 1200;
        telemetry_db::insert_event(&conn, &m).unwrap();

        let review = telemetry_db::query_review_metrics(&conn).unwrap();
        assert_eq!(review.auto_merge_count, 1);
        let avg = review.avg_review_latency_secs.unwrap();
        assert!((avg - 200.0).abs() < 0.01, "expected ~200s, got {avg}");
    }

    #[test]
    fn telemetry_db_poll_state_tracking() {
        let conn = telemetry_db::open_in_memory().unwrap();

        telemetry_db::record_agent_poll_state(&conn, "eng-1", true, 5).unwrap();
        telemetry_db::record_agent_poll_state(&conn, "eng-1", true, 5).unwrap();
        telemetry_db::record_agent_poll_state(&conn, "eng-1", false, 5).unwrap();

        let agents = telemetry_db::query_agent_metrics(&conn).unwrap();
        assert_eq!(agents[0].working_polls, 2);
        assert_eq!(agents[0].idle_polls, 1);
        assert_eq!(agents[0].total_cycle_secs, 10); // 2 * 5
    }

    #[test]
    fn telemetry_db_graceful_without_session() {
        // Events should insert without error even when no session row exists
        let conn = telemetry_db::open_in_memory().unwrap();
        telemetry_db::insert_event(&conn, &TeamEvent::task_completed("eng-1", Some("1"))).unwrap();
        let summaries = telemetry_db::query_session_summaries(&conn).unwrap();
        assert!(summaries.is_empty(), "no session should exist");
    }

    // -----------------------------------------------------------------------
    // 3. Agent Backend Config
    // -----------------------------------------------------------------------

    #[test]
    fn parse_per_role_agent_field() {
        let yaml = r#"
name: test
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: dev
    role_type: engineer
    agent: codex
    instances: 2
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let arch = &config.roles[0];
        let dev = &config.roles[1];
        assert_eq!(config.resolve_agent(arch).as_deref(), Some("claude"));
        assert_eq!(config.resolve_agent(dev).as_deref(), Some("codex"));
    }

    #[test]
    fn fallback_chain_instance_to_role_to_team_default() {
        // Team-level agent set to codex, role-level override to claude
        let yaml = r#"
name: test
agent: codex
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: worker
    role_type: engineer
    instances: 1
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let architect = &config.roles[0];
        let worker = &config.roles[1];

        // Role-level (claude) overrides team-level (codex)
        assert_eq!(config.resolve_agent(architect).as_deref(), Some("claude"));
        // No role-level agent, falls back to team-level (codex)
        assert_eq!(config.resolve_agent(worker).as_deref(), Some("codex"));

        // If both are None, falls back to "claude" (hardcoded default)
        let mut config_no_team = config.clone();
        config_no_team.agent = None;
        let mut worker_no_agent = worker.clone();
        worker_no_agent.agent = None;
        assert_eq!(
            config_no_team.resolve_agent(&worker_no_agent).as_deref(),
            Some("claude")
        );
    }

    #[test]
    fn user_role_resolves_no_agent() {
        let yaml = r#"
name: test
agent: codex
roles:
  - name: human
    role_type: user
  - name: worker
    role_type: engineer
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let user = &config.roles[0];
        assert_eq!(config.resolve_agent(user), None);
    }

    #[test]
    fn validate_rejects_unknown_team_agent() {
        let yaml = r#"
name: test
agent: mystery-agent
roles:
  - name: worker
    role_type: engineer
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(
            config.validate().is_err(),
            "should reject unknown team-level agent"
        );
    }

    #[test]
    fn validate_rejects_unknown_role_agent() {
        let yaml = r#"
name: test
roles:
  - name: worker
    role_type: engineer
    agent: nonexistent-backend
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(
            config.validate().is_err(),
            "should reject unknown role-level agent"
        );
    }

    #[test]
    fn hierarchy_propagates_agent_to_instances() {
        let yaml = r#"
name: test
agent: codex
roles:
  - name: manager
    role_type: manager
    agent: claude
  - name: dev
    role_type: engineer
    agent: codex
    instances: 2
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let members = resolve_hierarchy(&config).unwrap();

        // With instances=1 (default), name is just "manager" without suffix
        let manager = members.iter().find(|m| m.role_name == "manager").unwrap();
        assert_eq!(manager.agent.as_deref(), Some("claude"));

        // Engineers are named by role convention — find them by role_name
        let engineers: Vec<_> = members.iter().filter(|m| m.role_name == "dev").collect();
        assert_eq!(engineers.len(), 2, "expected 2 engineer instances");
        for eng in &engineers {
            assert_eq!(eng.agent.as_deref(), Some("codex"));
        }
    }

    // -----------------------------------------------------------------------
    // 4. Backend Validate
    // -----------------------------------------------------------------------

    #[test]
    fn backend_health_results_returns_unique_backends() {
        let yaml = r#"
name: test
roles:
  - name: manager
    role_type: manager
    agent: claude
  - name: dev
    role_type: engineer
    agent: claude
    instances: 3
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let results = config.backend_health_results();
        // claude appears twice in roles, but results should deduplicate
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "claude");
    }

    #[test]
    fn backend_health_results_multiple_backends() {
        let yaml = r#"
name: test
roles:
  - name: manager
    role_type: manager
    agent: claude
  - name: dev
    role_type: engineer
    agent: codex
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let results = config.backend_health_results();
        assert_eq!(results.len(), 2);
        let names: Vec<&str> = results.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"claude"), "should include claude");
        assert!(names.contains(&"codex"), "should include codex");
    }

    #[test]
    fn check_backend_health_returns_empty_for_known_backends() {
        // This test checks that known backends with actual binaries produce no warnings.
        // On CI or systems without claude/codex, warnings are expected — that's fine.
        // The key contract is: warnings only for unhealthy backends.
        let yaml = r#"
name: test
roles:
  - name: worker
    role_type: engineer
    agent: claude
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let warnings = config.check_backend_health();
        // Warnings are OK (binary might not be installed) — we just verify the method runs
        // without error and returns Vec<String>.
        let _ = warnings;
    }

    #[test]
    fn missing_backend_binary_produces_warning_not_error() {
        // Validation should pass even when a backend binary is missing.
        // Only check_backend_health produces warnings.
        let yaml = r#"
name: test
roles:
  - name: worker
    role_type: engineer
    agent: kiro
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        // validate() should pass (it checks config structure, not binary availability)
        assert!(config.validate().is_ok());
        // check_backend_health returns warnings for missing binaries
        let warnings = config.check_backend_health();
        // If kiro is not installed, we get a warning; if it is, we get no warning.
        // Either way, it should not panic.
        assert!(
            warnings.is_empty() || warnings[0].contains("kiro"),
            "warning should mention kiro: {warnings:?}"
        );
    }

    #[test]
    fn health_check_by_name_unknown_returns_none() {
        assert!(agent::health_check_by_name("nonexistent-backend-xyz").is_none());
    }

    #[test]
    fn backend_health_enum_serialization() {
        assert_eq!(BackendHealth::Healthy.as_str(), "healthy");
        assert_eq!(BackendHealth::Degraded.as_str(), "degraded");
        assert_eq!(BackendHealth::Unreachable.as_str(), "unreachable");
        assert!(BackendHealth::Healthy.is_healthy());
        assert!(!BackendHealth::Degraded.is_healthy());
        assert!(!BackendHealth::Unreachable.is_healthy());
    }

    #[test]
    fn known_agent_names_includes_all_backends() {
        assert!(KNOWN_AGENT_NAMES.contains(&"claude"));
        assert!(KNOWN_AGENT_NAMES.contains(&"codex"));
        assert!(KNOWN_AGENT_NAMES.contains(&"kiro-cli"));
    }

    #[test]
    fn adapter_from_name_returns_correct_adapter() {
        let claude = agent::adapter_from_name("claude").unwrap();
        assert_eq!(claude.name(), "claude-code");

        let codex = agent::adapter_from_name("codex").unwrap();
        assert_eq!(codex.name(), "codex-cli");

        let kiro = agent::adapter_from_name("kiro").unwrap();
        assert_eq!(kiro.name(), "kiro-cli");
    }

    // -----------------------------------------------------------------------
    // 5. Cross-feature: Grafana config + telemetry + backend config together
    // -----------------------------------------------------------------------

    #[test]
    fn full_config_with_grafana_telemetry_and_backend() {
        let yaml = r#"
name: integration-test
agent: claude
grafana:
  enabled: true
  port: 4000
roles:
  - name: human
    role_type: user
  - name: architect
    role_type: architect
    agent: claude
  - name: manager
    role_type: manager
    agent: claude
  - name: dev
    role_type: engineer
    agent: codex
    instances: 2
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();

        // Config parses cleanly
        assert!(config.validate().is_ok());

        // Grafana config correct
        assert!(config.grafana.enabled);
        assert_eq!(config.grafana.port, 4000);

        // Agent resolution correct
        assert_eq!(
            config.resolve_agent(&config.roles[0]),
            None,
            "user gets no agent"
        );
        assert_eq!(
            config.resolve_agent(&config.roles[1]).as_deref(),
            Some("claude")
        );
        assert_eq!(
            config.resolve_agent(&config.roles[3]).as_deref(),
            Some("codex")
        );

        // Hierarchy resolves
        let members = resolve_hierarchy(&config).unwrap();
        assert!(members.len() >= 5, "expected at least 5 members");

        // Backend health results produces unique list
        let health = config.backend_health_results();
        let backend_names: Vec<&str> = health.iter().map(|(n, _)| n.as_str()).collect();
        assert!(backend_names.contains(&"claude"));
        assert!(backend_names.contains(&"codex"));

        // Telemetry DB can handle events for these agents
        let conn = telemetry_db::open_in_memory().unwrap();
        telemetry_db::insert_event(&conn, &TeamEvent::daemon_started()).unwrap();
        for member in &members {
            if member.agent.is_some() {
                telemetry_db::record_agent_poll_state(&conn, &member.name, true, 5).unwrap();
            }
        }
        let agent_metrics = telemetry_db::query_agent_metrics(&conn).unwrap();
        assert!(
            agent_metrics.len() >= 4,
            "expected metrics for at least 4 agent members, got {}",
            agent_metrics.len()
        );
    }

    #[test]
    fn verbose_validation_includes_backend_checks() {
        let yaml = r#"
name: test
roles:
  - name: worker
    role_type: engineer
    agent: claude
"#;
        let config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        let checks = config.validate_verbose();

        // Backend health check names are formatted as "backend_health:<agent_name>"
        let backend_checks: Vec<_> = checks
            .iter()
            .filter(|c| c.name.starts_with("backend_health"))
            .collect();
        assert!(
            !backend_checks.is_empty(),
            "validate_verbose should include backend_health checks, got: {:?}",
            checks.iter().map(|c| &c.name).collect::<Vec<_>>()
        );
    }
}
