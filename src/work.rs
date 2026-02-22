//! Work command — the main orchestration pipeline.
//!
//! `batty work <phase>` reads a kanban phase board, constructs a prompt
//! for the agent describing the phase context, spawns the agent in a tmux
//! session, supervises with the orchestrator (auto-answering prompts via
//! send-keys per policy, Tier 2 supervisor agent for unknowns), and writes
//! a structured execution log.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use portable_pty::PtySize;
use tracing::info;

use crate::agent;
use crate::config::ProjectConfig;
use crate::detector::DetectorConfig;
use crate::log::{ExecutionLog, LogEvent};
use crate::orchestrator::{
    self, LogFileObserver, OrchestratorConfig, OrchestratorResult, StuckConfig,
};
use crate::policy::PolicyEngine;
use crate::task;
use crate::tier2::Tier2Config;

/// Run the full work pipeline for a phase.
pub fn run_phase(
    phase: &str,
    project_config: &ProjectConfig,
    agent_name: &str,
    policy_override: Option<&str>,
    project_root: &Path,
) -> Result<()> {
    // 1. Locate the phase board
    let phase_dir = project_root.join("kanban").join(phase);
    let tasks_dir = phase_dir.join("tasks");

    if !tasks_dir.is_dir() {
        bail!(
            "phase board not found: {} (expected {})",
            phase,
            tasks_dir.display()
        );
    }

    // 2. Load tasks for context
    let tasks = task::load_tasks_from_dir(&tasks_dir)
        .with_context(|| format!("failed to load tasks from {}", tasks_dir.display()))?;

    info!(
        phase = phase,
        task_count = tasks.len(),
        "loaded phase board"
    );

    // 3. Set up execution log
    let log_dir = project_root.join(".batty").join("logs");
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let log_path = log_dir.join(format!("{phase}-{timestamp}.jsonl"));
    let execution_log = ExecutionLog::new(&log_path)
        .with_context(|| format!("failed to create execution log at {}", log_path.display()))?;

    info!(log = %log_path.display(), "execution log created");

    execution_log.log(LogEvent::SessionStarted {
        phase: phase.to_string(),
    })?;

    // Log all tasks
    for t in &tasks {
        execution_log.log(LogEvent::TaskRead {
            task_id: t.id,
            title: t.title.clone(),
            status: t.status.clone(),
        })?;
    }

    // 4. Resolve agent adapter
    let adapter = agent::adapter_from_name(agent_name)
        .with_context(|| format!("unknown agent: {agent_name}"))?;

    // 5. Resolve policy
    let policy_tier = match policy_override {
        Some("observe") => crate::config::Policy::Observe,
        Some("suggest") => crate::config::Policy::Suggest,
        Some("act") => crate::config::Policy::Act,
        Some(other) => bail!("unknown policy: {other} (expected observe/suggest/act)"),
        None => project_config.defaults.policy,
    };

    let policy_engine = PolicyEngine::new(policy_tier, project_config.policy.auto_answer.clone());

    // 6. Build the phase prompt for the agent
    let prompt = build_phase_prompt(phase, &tasks, project_root);

    // 7. Get spawn config from adapter
    let spawn_config = adapter.spawn_config(&prompt, project_root);

    execution_log.log(LogEvent::AgentLaunched {
        agent: adapter.name().to_string(),
        program: spawn_config.program.clone(),
        work_dir: spawn_config.work_dir.clone(),
    })?;

    // 8. Build orchestrator config
    let orch_log = log_dir.join("orchestrator.log");
    let observer = LogFileObserver::new(&orch_log)?;

    // Load project docs for Tier 2 supervisor context
    let system_prompt = crate::tier2::load_project_docs(project_root);
    let tier2_config = Tier2Config {
        system_prompt: Some(system_prompt),
        ..Tier2Config::default()
    };

    let config = OrchestratorConfig {
        spawn: spawn_config,
        patterns: adapter.prompt_patterns(),
        policy: policy_engine,
        detector: DetectorConfig::default(),
        phase: phase.to_string(),
        project_root: project_root.to_path_buf(),
        poll_interval: OrchestratorConfig::default_poll_interval(),
        buffer_size: OrchestratorConfig::default_buffer_size(),
        tier2: Some(tier2_config),
        log_pane: true,
        log_pane_height_pct: 20,
        stuck: Some(StuckConfig::default()),
        answer_delay: Duration::from_secs(1),
    };

    // 9. Set up stop signal (for Ctrl-C handling)
    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();
    ctrlc::set_handler(move || {
        stop_clone.store(true, std::sync::atomic::Ordering::Relaxed);
    })
    .ok(); // best-effort — may fail if handler already set

    // 10. Run the orchestrator
    info!(
        agent = adapter.name(),
        phase = phase,
        "launching tmux-based supervised session"
    );

    let session = crate::tmux::session_name(phase);
    println!(
        "\x1b[36m[batty]\x1b[0m starting {} in tmux session '{}'",
        phase, session
    );
    println!(
        "\x1b[36m[batty]\x1b[0m attach with: batty attach {}",
        phase
    );

    let result = orchestrator::run(config, Box::new(observer), stop)?;

    // 11. Log the result
    match &result {
        OrchestratorResult::Completed => {
            execution_log.log(LogEvent::RunCompleted {
                summary: "executor completed".to_string(),
            })?;
            info!("session completed");
        }
        OrchestratorResult::Detached => {
            execution_log.log(LogEvent::SessionEnded {
                result: "detached/stopped".to_string(),
            })?;
            info!("session detached");
        }
        OrchestratorResult::Error { detail } => {
            execution_log.log(LogEvent::RunFailed {
                reason: detail.clone(),
            })?;
            info!(detail = %detail, "session error");
        }
    }

    execution_log.log(LogEvent::SessionEnded {
        result: format!("{result:?}"),
    })?;

    println!(
        "\n\x1b[36m[batty]\x1b[0m session complete. Log: {}",
        log_path.display()
    );

    Ok(())
}

/// Build a prompt describing the phase context for the agent.
fn build_phase_prompt(phase: &str, tasks: &[task::Task], project_root: &Path) -> String {
    let mut prompt = String::new();

    prompt.push_str(&format!(
        "You are working on the {} board for the project at {}.\n\n",
        phase,
        project_root.display()
    ));

    // Summarize tasks by status
    let backlog: Vec<_> = tasks.iter().filter(|t| t.status == "backlog").collect();
    let in_progress: Vec<_> = tasks.iter().filter(|t| t.status == "in-progress").collect();
    let done: Vec<_> = tasks.iter().filter(|t| t.status == "done").collect();

    prompt.push_str(&format!(
        "Board status: {} backlog, {} in-progress, {} done (of {} total)\n\n",
        backlog.len(),
        in_progress.len(),
        done.len(),
        tasks.len()
    ));

    if !backlog.is_empty() {
        prompt.push_str("Backlog tasks:\n");
        for t in &backlog {
            let deps = if t.depends_on.is_empty() {
                String::new()
            } else {
                format!(
                    " (depends on: {})",
                    t.depends_on
                        .iter()
                        .map(|d| format!("#{d}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            prompt.push_str(&format!("  #{}: {}{}\n", t.id, t.title, deps));
            if !t.description.is_empty() {
                prompt.push_str(&format!("     {}\n", t.description));
            }
        }
        prompt.push('\n');
    }

    if !in_progress.is_empty() {
        prompt.push_str("In-progress tasks:\n");
        for t in &in_progress {
            prompt.push_str(&format!("  #{}: {}\n", t.id, t.title));
        }
        prompt.push('\n');
    }

    prompt.push_str(
        "Follow the workflow in CLAUDE.md to pick tasks, implement, test, and close them.\n",
    );
    prompt.push_str("Work through the backlog in dependency order.\n");

    prompt
}

/// Get the current terminal size, falling back to 80x24.
fn terminal_size() -> PtySize {
    // Try to get the actual terminal size
    let (cols, rows) = term_size::dimensions().unwrap_or((80, 24));
    PtySize {
        rows: rows as u16,
        cols: cols as u16,
        pixel_width: 0,
        pixel_height: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::task::Task;

    fn make_task(id: u32, title: &str, status: &str, deps: Vec<u32>) -> Task {
        Task {
            id,
            title: title.to_string(),
            status: status.to_string(),
            priority: "high".to_string(),
            tags: vec![],
            depends_on: deps,
            description: format!("Description for task {id}"),
            batty_config: None,
            source_path: PathBuf::new(),
        }
    }

    #[test]
    fn phase_prompt_includes_board_summary() {
        let tasks = vec![
            make_task(1, "scaffolding", "done", vec![]),
            make_task(2, "CI setup", "done", vec![1]),
            make_task(3, "task reader", "backlog", vec![1]),
            make_task(4, "prompt detection", "in-progress", vec![]),
        ];

        let prompt = build_phase_prompt("phase-1", &tasks, Path::new("/project"));

        assert!(prompt.contains("phase-1"));
        assert!(prompt.contains("/project"));
        assert!(prompt.contains("1 backlog"));
        assert!(prompt.contains("1 in-progress"));
        assert!(prompt.contains("2 done"));
        assert!(prompt.contains("4 total"));
    }

    #[test]
    fn phase_prompt_shows_backlog_with_deps() {
        let tasks = vec![
            make_task(1, "first task", "done", vec![]),
            make_task(2, "second task", "backlog", vec![1]),
        ];

        let prompt = build_phase_prompt("phase-1", &tasks, Path::new("/work"));

        assert!(prompt.contains("#2: second task"));
        assert!(prompt.contains("depends on: #1"));
    }

    #[test]
    fn phase_prompt_shows_descriptions() {
        let tasks = vec![make_task(5, "adapter", "backlog", vec![])];

        let prompt = build_phase_prompt("phase-1", &tasks, Path::new("/work"));

        assert!(prompt.contains("Description for task 5"));
    }

    #[test]
    fn phase_prompt_empty_board() {
        let tasks = vec![];
        let prompt = build_phase_prompt("phase-1", &tasks, Path::new("/work"));

        assert!(prompt.contains("0 backlog"));
        assert!(prompt.contains("0 total"));
    }

    #[test]
    fn terminal_size_returns_valid_dimensions() {
        let size = terminal_size();
        assert!(size.rows > 0);
        assert!(size.cols > 0);
    }

    #[test]
    fn missing_phase_board_is_error() {
        let tmp = tempfile::tempdir().unwrap();
        let config = ProjectConfig::default();

        let result = run_phase("nonexistent", &config, "claude", None, tmp.path());
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("phase board not found")
        );
    }
}
