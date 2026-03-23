//! Mock agent backend for testing.
//!
//! Provides a configurable mock that implements `AgentAdapter`, tracking all
//! method calls and supporting error injection. No real agents or tmux needed.

use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::agent::{AgentAdapter, BackendHealth, SpawnConfig};
use crate::prompt::PromptPatterns;

/// Record of a method call on MockBackend.
#[derive(Debug, Clone, PartialEq)]
pub enum MockCall {
    Name,
    SpawnConfig {
        task: String,
        work_dir: String,
    },
    PromptPatterns,
    InstructionCandidates,
    WrapLaunchPrompt {
        prompt: String,
    },
    FormatInput {
        response: String,
    },
    ResetContextKeys,
    LaunchCommand {
        prompt: String,
        idle: bool,
        resume: bool,
        session_id: Option<String>,
    },
    NewSessionId,
    SupportsResume,
    HealthCheck,
}

/// Configurable behavior for the mock backend.
#[derive(Debug, Clone)]
pub struct MockConfig {
    pub name: String,
    pub launch_command_result: Result<String, String>,
    pub session_id: Option<String>,
    pub supports_resume: bool,
    pub health: BackendHealth,
    pub format_input_suffix: String,
    pub wrap_prompt_prefix: String,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            name: "mock-agent".to_string(),
            launch_command_result: Ok("exec mock-agent --run".to_string()),
            session_id: None,
            supports_resume: false,
            health: BackendHealth::Healthy,
            format_input_suffix: "\n".to_string(),
            wrap_prompt_prefix: String::new(),
        }
    }
}

/// A mock backend that records all calls and returns configurable values.
pub struct MockBackend {
    config: MockConfig,
    calls: Arc<Mutex<Vec<MockCall>>>,
}

impl MockBackend {
    pub fn new(config: MockConfig) -> Self {
        Self {
            config,
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Create with default config.
    pub fn default_mock() -> Self {
        Self::new(MockConfig::default())
    }

    /// Get a clone of the call log shared handle.
    pub fn call_log(&self) -> Arc<Mutex<Vec<MockCall>>> {
        Arc::clone(&self.calls)
    }

    /// Get a snapshot of recorded calls.
    pub fn calls(&self) -> Vec<MockCall> {
        self.calls.lock().unwrap().clone()
    }

    fn record(&self, call: MockCall) {
        self.calls.lock().unwrap().push(call);
    }
}

impl AgentAdapter for MockBackend {
    fn name(&self) -> &str {
        self.record(MockCall::Name);
        &self.config.name
    }

    fn spawn_config(&self, task_description: &str, work_dir: &Path) -> SpawnConfig {
        self.record(MockCall::SpawnConfig {
            task: task_description.to_string(),
            work_dir: work_dir.to_string_lossy().to_string(),
        });
        SpawnConfig {
            program: self.config.name.clone(),
            args: vec![task_description.to_string()],
            work_dir: work_dir.to_string_lossy().to_string(),
            env: vec![],
        }
    }

    fn prompt_patterns(&self) -> PromptPatterns {
        self.record(MockCall::PromptPatterns);
        // Return claude patterns as a reasonable default
        PromptPatterns::claude_code()
    }

    fn instruction_candidates(&self) -> &'static [&'static str] {
        self.record(MockCall::InstructionCandidates);
        &["CLAUDE.md", "AGENTS.md"]
    }

    fn wrap_launch_prompt(&self, prompt: &str) -> String {
        self.record(MockCall::WrapLaunchPrompt {
            prompt: prompt.to_string(),
        });
        if self.config.wrap_prompt_prefix.is_empty() {
            prompt.to_string()
        } else {
            format!("{}{}", self.config.wrap_prompt_prefix, prompt)
        }
    }

    fn format_input(&self, response: &str) -> String {
        self.record(MockCall::FormatInput {
            response: response.to_string(),
        });
        format!("{response}{}", self.config.format_input_suffix)
    }

    fn reset_context_keys(&self) -> Vec<(String, bool)> {
        self.record(MockCall::ResetContextKeys);
        vec![("/clear".to_string(), true)]
    }

    fn launch_command(
        &self,
        prompt: &str,
        idle: bool,
        resume: bool,
        session_id: Option<&str>,
    ) -> anyhow::Result<String> {
        self.record(MockCall::LaunchCommand {
            prompt: prompt.to_string(),
            idle,
            resume,
            session_id: session_id.map(String::from),
        });
        self.config
            .launch_command_result
            .clone()
            .map_err(|e| anyhow::anyhow!(e))
    }

    fn new_session_id(&self) -> Option<String> {
        self.record(MockCall::NewSessionId);
        self.config.session_id.clone()
    }

    fn supports_resume(&self) -> bool {
        self.record(MockCall::SupportsResume);
        self.config.supports_resume
    }

    fn health_check(&self) -> BackendHealth {
        self.record(MockCall::HealthCheck);
        self.config.health
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- MockBackend unit tests ---

    #[test]
    fn default_mock_returns_expected_name() {
        let mock = MockBackend::default_mock();
        assert_eq!(mock.name(), "mock-agent");
    }

    #[test]
    fn mock_records_all_calls() {
        let mock = MockBackend::default_mock();
        let _ = mock.name();
        let _ = mock.spawn_config("task", Path::new("/tmp"));
        let _ = mock.format_input("y");
        let _ = mock.health_check();

        let calls = mock.calls();
        assert_eq!(calls.len(), 4);
        assert_eq!(calls[0], MockCall::Name);
        assert!(matches!(calls[1], MockCall::SpawnConfig { .. }));
        assert!(matches!(calls[2], MockCall::FormatInput { .. }));
        assert_eq!(calls[3], MockCall::HealthCheck);
    }

    #[test]
    fn mock_configurable_name() {
        let mock = MockBackend::new(MockConfig {
            name: "custom-bot".to_string(),
            ..MockConfig::default()
        });
        assert_eq!(mock.name(), "custom-bot");
    }

    #[test]
    fn mock_configurable_health() {
        let mock = MockBackend::new(MockConfig {
            health: BackendHealth::Degraded,
            ..MockConfig::default()
        });
        assert_eq!(mock.health_check(), BackendHealth::Degraded);
    }

    #[test]
    fn mock_configurable_launch_error() {
        let mock = MockBackend::new(MockConfig {
            launch_command_result: Err("API key expired".to_string()),
            ..MockConfig::default()
        });
        let result = mock.launch_command("test", false, false, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("API key expired"));
    }

    #[test]
    fn mock_call_log_shared_handle() {
        let mock = MockBackend::default_mock();
        let log = mock.call_log();
        let _ = mock.name();
        assert_eq!(log.lock().unwrap().len(), 1);
    }

    // --- Trait Contract Tests ---

    #[test]
    fn spawn_creates_agent() {
        let mock = MockBackend::new(MockConfig {
            name: "test-agent".to_string(),
            ..MockConfig::default()
        });
        let config = mock.spawn_config("Fix the auth bug", Path::new("/project/worktree"));

        assert_eq!(config.program, "test-agent");
        assert_eq!(config.args, vec!["Fix the auth bug"]);
        assert_eq!(config.work_dir, "/project/worktree");

        // Verify the call was recorded with correct parameters
        let calls = mock.calls();
        assert_eq!(
            calls[0],
            MockCall::SpawnConfig {
                task: "Fix the auth bug".to_string(),
                work_dir: "/project/worktree".to_string(),
            }
        );
    }

    #[test]
    fn send_message_delivers() {
        // "Send message" maps to format_input — the mechanism for delivering
        // text to an agent's stdin through the adapter.
        let mock = MockBackend::new(MockConfig {
            format_input_suffix: "\n".to_string(),
            ..MockConfig::default()
        });
        let formatted = mock.format_input("deploy to staging");
        assert_eq!(formatted, "deploy to staging\n");

        let calls = mock.calls();
        assert_eq!(
            calls[0],
            MockCall::FormatInput {
                response: "deploy to staging".to_string(),
            }
        );
    }

    #[test]
    fn detect_status_correct() {
        // Status detection flows through health_check + prompt_patterns.
        // Verify both paths return correct values.
        let healthy_mock = MockBackend::new(MockConfig {
            health: BackendHealth::Healthy,
            ..MockConfig::default()
        });
        assert_eq!(healthy_mock.health_check(), BackendHealth::Healthy);
        assert!(healthy_mock.health_check().is_healthy());

        let degraded_mock = MockBackend::new(MockConfig {
            health: BackendHealth::Degraded,
            ..MockConfig::default()
        });
        assert_eq!(degraded_mock.health_check(), BackendHealth::Degraded);
        assert!(!degraded_mock.health_check().is_healthy());

        let unreachable_mock = MockBackend::new(MockConfig {
            health: BackendHealth::Unreachable,
            ..MockConfig::default()
        });
        assert_eq!(unreachable_mock.health_check(), BackendHealth::Unreachable);

        // prompt_patterns should return a usable detector
        let patterns = healthy_mock.prompt_patterns();
        // It should not crash on normal text
        assert!(patterns.detect("normal log output").is_none());
    }

    #[test]
    fn restart_triggers_correctly() {
        // Restart flow: reset_context_keys + launch_command with resume flag.
        let mock = MockBackend::new(MockConfig {
            supports_resume: true,
            session_id: Some("sess-42".to_string()),
            launch_command_result: Ok("exec mock-agent --resume sess-42".to_string()),
            ..MockConfig::default()
        });

        // Phase 1: reset context
        let keys = mock.reset_context_keys();
        assert!(!keys.is_empty());

        // Phase 2: relaunch with resume
        assert!(mock.supports_resume());
        let sid = mock.new_session_id().unwrap();
        assert_eq!(sid, "sess-42");
        let cmd = mock
            .launch_command("resume prompt", false, true, Some(&sid))
            .unwrap();
        assert!(cmd.contains("resume"));

        // Verify call sequence
        let calls = mock.calls();
        assert!(calls.contains(&MockCall::ResetContextKeys));
        assert!(calls.contains(&MockCall::SupportsResume));
        assert!(calls.contains(&MockCall::NewSessionId));
    }

    #[test]
    fn get_output_returns_content() {
        // Output capture flows through launch_command (which produces the
        // agent process) and prompt_patterns (which detects completion).
        let mock = MockBackend::new(MockConfig {
            launch_command_result: Ok("exec mock-agent --run 'task prompt'".to_string()),
            ..MockConfig::default()
        });

        let cmd = mock
            .launch_command("task prompt", false, false, None)
            .unwrap();
        assert_eq!(cmd, "exec mock-agent --run 'task prompt'");

        // Prompt patterns can detect completion signals
        let patterns = mock.prompt_patterns();
        let completion = patterns.detect(r#"{"type": "result", "subtype": "success"}"#);
        assert!(completion.is_some());
    }

    #[test]
    fn error_propagation() {
        // Errors from launch_command should propagate through the trait.
        let mock = MockBackend::new(MockConfig {
            launch_command_result: Err("binary not found".to_string()),
            ..MockConfig::default()
        });

        let result = mock.launch_command("test", false, false, None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("binary not found"));

        // Health can also signal errors
        let unreachable = MockBackend::new(MockConfig {
            health: BackendHealth::Unreachable,
            ..MockConfig::default()
        });
        assert_eq!(unreachable.health_check(), BackendHealth::Unreachable);
        assert!(!unreachable.health_check().is_healthy());

        // Error patterns detected in output
        let patterns = mock.prompt_patterns();
        let err_detect = patterns.detect(r#"{"type": "result", "is_error": true}"#);
        assert!(err_detect.is_some());
    }

    #[test]
    fn trait_object_dispatch() {
        // Verify Box<dyn AgentAdapter> works — the trait must be object-safe.
        let mock: Box<dyn AgentAdapter> = Box::new(MockBackend::new(MockConfig {
            name: "boxed-mock".to_string(),
            ..MockConfig::default()
        }));

        assert_eq!(mock.name(), "boxed-mock");
        let config = mock.spawn_config("task via dyn", Path::new("/dyn/path"));
        assert_eq!(config.program, "boxed-mock");
        assert_eq!(config.args, vec!["task via dyn"]);

        let cmd = mock.launch_command("go", false, false, None).unwrap();
        assert!(!cmd.is_empty());

        assert_eq!(mock.health_check(), BackendHealth::Healthy);
    }

    #[test]
    fn multiple_backends_coexist() {
        // Two different mock backends in the same test, each independent.
        let alpha = MockBackend::new(MockConfig {
            name: "alpha".to_string(),
            health: BackendHealth::Healthy,
            launch_command_result: Ok("exec alpha --go".to_string()),
            ..MockConfig::default()
        });
        let beta = MockBackend::new(MockConfig {
            name: "beta".to_string(),
            health: BackendHealth::Degraded,
            launch_command_result: Ok("exec beta --go".to_string()),
            ..MockConfig::default()
        });

        // Each has independent name and health
        assert_eq!(alpha.name(), "alpha");
        assert_eq!(beta.name(), "beta");
        assert_eq!(alpha.health_check(), BackendHealth::Healthy);
        assert_eq!(beta.health_check(), BackendHealth::Degraded);

        // Each has independent call logs
        let _ = alpha.spawn_config("task-a", Path::new("/a"));
        assert_eq!(alpha.calls().len(), 3); // name + health + spawn
        assert_eq!(beta.calls().len(), 2); // name + health only

        // Both work as trait objects simultaneously
        let backends: Vec<Box<dyn AgentAdapter>> = vec![
            Box::new(MockBackend::new(MockConfig {
                name: "dyn-1".to_string(),
                ..MockConfig::default()
            })),
            Box::new(MockBackend::new(MockConfig {
                name: "dyn-2".to_string(),
                ..MockConfig::default()
            })),
        ];
        let names: Vec<&str> = backends.iter().map(|b| b.name()).collect();
        assert_eq!(names, vec!["dyn-1", "dyn-2"]);
    }

    // --- Consumer Tests ---

    #[test]
    fn consumer_adapter_from_name_returns_usable_trait_object() {
        // Verify that adapter_from_name returns a Box<dyn AgentAdapter> that
        // can be used by consumers without knowing the concrete type.
        use crate::agent::adapter_from_name;

        for name in &["claude", "codex", "kiro"] {
            let adapter = adapter_from_name(name).expect("should resolve");
            // Consumer can call all trait methods through the box
            assert!(!adapter.name().is_empty());
            let config = adapter.spawn_config("consumer test", Path::new("/consumer"));
            assert!(!config.program.is_empty());
            let cmd = adapter.launch_command("go", false, false, None);
            assert!(cmd.is_ok());
            let _ = adapter.health_check();
            let _ = adapter.prompt_patterns();
            let _ = adapter.format_input("yes");
            let _ = adapter.instruction_candidates();
            let _ = adapter.wrap_launch_prompt("prompt");
            let _ = adapter.reset_context_keys();
            let _ = adapter.new_session_id();
            let _ = adapter.supports_resume();
        }
    }

    #[test]
    fn consumer_health_check_by_name_dispatches_correctly() {
        use crate::agent::health_check_by_name;

        // Known backends return Some health status
        let health = health_check_by_name("claude");
        assert!(health.is_some());

        let health = health_check_by_name("codex");
        assert!(health.is_some());

        let health = health_check_by_name("kiro");
        assert!(health.is_some());

        // Unknown returns None
        assert!(health_check_by_name("nonexistent").is_none());
    }

    #[test]
    fn consumer_heterogeneous_backend_vec() {
        // Simulate what daemon code does: hold multiple backends in a Vec
        // and iterate over them for status checks.
        use crate::agent::{claude, codex, kiro};

        let backends: Vec<Box<dyn AgentAdapter>> = vec![
            Box::new(claude::ClaudeCodeAdapter::new(None)),
            Box::new(codex::CodexCliAdapter::new(None)),
            Box::new(kiro::KiroCliAdapter::new(None)),
            Box::new(MockBackend::default_mock()),
        ];

        // All backends respond to the same trait API
        for backend in &backends {
            let name = backend.name();
            assert!(!name.is_empty());

            let config = backend.spawn_config("health check", Path::new("/tmp"));
            assert_eq!(config.work_dir, "/tmp");

            let cmd = backend.launch_command("ping", true, false, None);
            assert!(cmd.is_ok());
        }

        // Can collect names
        let names: Vec<&str> = backends.iter().map(|b| b.name()).collect();
        assert_eq!(names.len(), 4);
        assert!(names.contains(&"claude-code"));
        assert!(names.contains(&"codex-cli"));
        assert!(names.contains(&"kiro-cli"));
        assert!(names.contains(&"mock-agent"));
    }

    #[test]
    fn consumer_backend_selection_pattern() {
        // Pattern used by daemon: select backend by name, configure, use.
        use crate::agent::adapter_from_name;

        let agent_name = "claude";
        let adapter = adapter_from_name(agent_name).unwrap();

        // Consumer builds launch command
        let cmd = adapter
            .launch_command("implement feature X", false, false, None)
            .unwrap();
        assert!(cmd.contains("claude"));

        // Consumer checks health before dispatching
        let health = adapter.health_check();
        // (We don't assert specific health since the binary may or may not exist,
        // but the call should not panic.)
        let _ = health.is_healthy();
        let _ = health.as_str();
    }
}
