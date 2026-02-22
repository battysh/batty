//! Work command — the main orchestration pipeline.
//!
//! `batty work <phase>` reads a kanban phase board, constructs a prompt
//! for the agent describing the phase context, spawns the agent in a tmux
//! session, supervises with the orchestrator (auto-answering prompts via
//! send-keys per policy, Tier 2 supervisor agent for unknowns), and writes
//! a structured execution log.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use portable_pty::PtySize;
use tracing::{info, warn};

#[path = "worktree.rs"]
mod phase_worktree;

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
use phase_worktree::{CleanupDecision, RunOutcome};

/// Run the full work pipeline for a phase.
pub fn run_phase(
    phase: &str,
    project_config: &ProjectConfig,
    agent_name: &str,
    policy_override: Option<&str>,
    auto_attach: bool,
    project_root: &Path,
) -> Result<()> {
    // 1. Validate the phase board exists before creating an isolated worktree.
    let source_phase_dir = project_root.join("kanban").join(phase);
    let source_tasks_dir = source_phase_dir.join("tasks");

    if !source_tasks_dir.is_dir() {
        bail!(
            "phase board not found: {} (expected {})",
            phase,
            source_tasks_dir.display()
        );
    }

    // 2. Create worktree for this run (earliest isolation boundary).
    let phase_worktree = phase_worktree::prepare_phase_worktree(project_root, phase)
        .with_context(|| format!("failed to create isolated worktree for phase '{phase}'"))?;
    let execution_root = phase_worktree.path.clone();

    info!(
        phase = phase,
        branch = %phase_worktree.branch,
        base_branch = %phase_worktree.base_branch,
        worktree = %execution_root.display(),
        "phase worktree prepared"
    );

    // 3. Load tasks for context from the isolated worktree.
    let phase_dir = execution_root.join("kanban").join(phase);
    let tasks_dir = phase_dir.join("tasks");
    let tasks = task::load_tasks_from_dir(&tasks_dir)
        .with_context(|| format!("failed to load tasks from {}", tasks_dir.display()))?;

    info!(
        phase = phase,
        task_count = tasks.len(),
        "loaded phase board"
    );

    // 4. Set up execution log
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
    execution_log.log(LogEvent::PhaseWorktreeCreated {
        phase: phase.to_string(),
        path: execution_root.display().to_string(),
        branch: phase_worktree.branch.clone(),
        base_branch: phase_worktree.base_branch.clone(),
    })?;

    // Log all tasks
    for t in &tasks {
        execution_log.log(LogEvent::TaskRead {
            task_id: t.id,
            title: t.title.clone(),
            status: t.status.clone(),
        })?;
    }

    // 5. Resolve agent adapter
    let adapter = agent::adapter_from_name(agent_name)
        .with_context(|| format!("unknown agent: {agent_name}"))?;

    // 6. Resolve policy
    let policy_tier = match policy_override {
        Some("observe") => crate::config::Policy::Observe,
        Some("suggest") => crate::config::Policy::Suggest,
        Some("act") => crate::config::Policy::Act,
        Some(other) => bail!("unknown policy: {other} (expected observe/suggest/act)"),
        None => project_config.defaults.policy,
    };

    let policy_engine = PolicyEngine::new(policy_tier, project_config.policy.auto_answer.clone());

    // 7. Build the phase prompt for the agent
    let prompt = build_phase_prompt(phase, &tasks, &execution_root);

    // 8. Get spawn config from adapter
    let spawn_config = adapter.spawn_config(&prompt, &execution_root);

    execution_log.log(LogEvent::AgentLaunched {
        agent: adapter.name().to_string(),
        program: spawn_config.program.clone(),
        work_dir: spawn_config.work_dir.clone(),
    })?;

    // 9. Build orchestrator config
    let orch_log = log_dir.join("orchestrator.log");
    let observer = LogFileObserver::new(&orch_log)?;

    // Load project docs for Tier 2 supervisor context
    let tier2_config = if project_config.supervisor.enabled {
        let system_prompt = crate::tier2::load_project_docs(&execution_root);
        Some(Tier2Config {
            program: project_config.supervisor.program.clone(),
            args: project_config.supervisor.args.clone(),
            timeout: Duration::from_secs(project_config.supervisor.timeout_secs),
            system_prompt: Some(system_prompt),
            trace_io: project_config.supervisor.trace_io,
        })
    } else {
        None
    };

    let config = OrchestratorConfig {
        spawn: spawn_config,
        patterns: adapter.prompt_patterns(),
        policy: policy_engine,
        detector: DetectorConfig {
            silence_timeout: Duration::from_secs(project_config.detector.silence_timeout_secs),
            answer_cooldown: Duration::from_millis(project_config.detector.answer_cooldown_millis),
            unknown_request_fallback: project_config.detector.unknown_request_fallback,
        },
        phase: phase.to_string(),
        project_root: project_root.to_path_buf(),
        poll_interval: OrchestratorConfig::default_poll_interval(),
        buffer_size: OrchestratorConfig::default_buffer_size(),
        tier2: tier2_config,
        log_pane: true,
        log_pane_height_pct: 20,
        stuck: Some(StuckConfig::default()),
        answer_delay: Duration::from_secs(1),
        auto_attach,
    };

    // 10. Set up stop signal (for Ctrl-C handling)
    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();
    ctrlc::set_handler(move || {
        stop_clone.store(true, std::sync::atomic::Ordering::Relaxed);
    })
    .ok(); // best-effort — may fail if handler already set

    // 11. Run the orchestrator
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
        "\x1b[36m[batty]\x1b[0m worktree: {} ({})",
        execution_root.display(),
        phase_worktree.branch
    );
    if !auto_attach {
        println!("\x1b[36m[batty]\x1b[0m attach with: batty attach {}", phase);
    }

    let result = match orchestrator::run(config, Box::new(observer), stop) {
        Ok(result) => result,
        Err(e) => {
            handle_worktree_finalize(phase, &execution_log, &phase_worktree, RunOutcome::Failed);
            return Err(e);
        }
    };

    // 12. Log the result
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

    let run_outcome = match &result {
        OrchestratorResult::Completed => RunOutcome::Completed,
        OrchestratorResult::Detached | OrchestratorResult::Error { .. } => RunOutcome::Failed,
    };
    handle_worktree_finalize(phase, &execution_log, &phase_worktree, run_outcome);

    execution_log.log(LogEvent::SessionEnded {
        result: format!("{result:?}"),
    })?;

    println!(
        "\n\x1b[36m[batty]\x1b[0m session complete. Log: {}",
        log_path.display()
    );

    Ok(())
}

fn handle_worktree_finalize(
    phase: &str,
    execution_log: &ExecutionLog,
    phase_worktree: &phase_worktree::PhaseWorktree,
    outcome: RunOutcome,
) {
    match phase_worktree.finalize(outcome) {
        Ok(CleanupDecision::Cleaned) => {
            if let Err(e) = execution_log.log(LogEvent::PhaseWorktreeCleaned {
                phase: phase.to_string(),
                path: phase_worktree.path.display().to_string(),
                branch: phase_worktree.branch.clone(),
            }) {
                warn!(error = %e, "failed to log worktree cleanup");
            }
            info!(
                phase = phase,
                branch = %phase_worktree.branch,
                "worktree cleaned after successful merge"
            );
        }
        Ok(CleanupDecision::KeptForReview) => {
            if let Err(e) = execution_log.log(LogEvent::PhaseWorktreeRetained {
                phase: phase.to_string(),
                path: phase_worktree.path.display().to_string(),
                branch: phase_worktree.branch.clone(),
                reason: "run completed but branch is not merged yet".to_string(),
            }) {
                warn!(error = %e, "failed to log retained worktree");
            }
            println!(
                "\x1b[36m[batty]\x1b[0m retained worktree for review: {} ({})",
                phase_worktree.path.display(),
                phase_worktree.branch
            );
        }
        Ok(CleanupDecision::KeptForFailure) => {
            if let Err(e) = execution_log.log(LogEvent::PhaseWorktreeRetained {
                phase: phase.to_string(),
                path: phase_worktree.path.display().to_string(),
                branch: phase_worktree.branch.clone(),
                reason: "run failed/detached".to_string(),
            }) {
                warn!(error = %e, "failed to log retained failure worktree");
            }
            println!(
                "\x1b[36m[batty]\x1b[0m retained failed worktree: {} ({})",
                phase_worktree.path.display(),
                phase_worktree.branch
            );
        }
        Err(e) => {
            warn!(
                error = %e,
                branch = %phase_worktree.branch,
                "failed to finalize phase worktree"
            );
        }
    }
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

        let result = run_phase("nonexistent", &config, "claude", None, false, tmp.path());
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("phase board not found")
        );
    }
}
