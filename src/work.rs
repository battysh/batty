//! Work command — the main orchestration pipeline.
//!
//! `batty work <phase>` reads a kanban phase board, constructs a prompt
//! for the agent describing the phase context, spawns the agent in a tmux
//! session, supervises with the orchestrator (auto-answering prompts via
//! send-keys per policy, Tier 2 supervisor agent for unknowns), and writes
//! a structured execution log.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use portable_pty::PtySize;
use tracing::{info, warn};

#[path = "worktree.rs"]
mod phase_worktree;

use crate::agent;
use crate::config::{Policy, ProjectConfig};
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
    force_new_worktree: bool,
    dry_run: bool,
    project_root: &Path,
    config_path: Option<&Path>,
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
    let (phase_worktree, resumed_worktree) =
        phase_worktree::resolve_phase_worktree(project_root, phase, force_new_worktree)
            .with_context(|| format!("failed to resolve isolated worktree for phase '{phase}'"))?;
    let execution_root = phase_worktree.path.clone();

    info!(
        phase = phase,
        branch = %phase_worktree.branch,
        base_branch = %phase_worktree.base_branch,
        worktree = %execution_root.display(),
        resumed = resumed_worktree,
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

    // 4. Set up per-run logs under .batty/logs/<phase-run-###>/
    let log_dir = project_root
        .join(".batty")
        .join("logs")
        .join(&phase_worktree.branch);
    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("failed to create run log dir {}", log_dir.display()))?;
    let log_path = log_dir.join("execution.jsonl");
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

    // 7. Compose deterministic launch context (with required file validation).
    let launch_context = compose_launch_context(
        phase,
        &tasks,
        &execution_root,
        project_config,
        policy_tier,
        adapter.as_ref(),
        config_path,
    )?;
    let context_snapshot_path = log_dir.join(format!("{phase}-{timestamp}-launch-context.md"));
    std::fs::write(&context_snapshot_path, &launch_context.prompt).with_context(|| {
        format!(
            "failed to write launch context snapshot to {}",
            context_snapshot_path.display()
        )
    })?;
    execution_log.log(LogEvent::LaunchContextSnapshot {
        phase: phase.to_string(),
        agent: adapter.name().to_string(),
        instructions_path: launch_context.instructions_path.display().to_string(),
        phase_doc_path: launch_context.phase_doc_path.display().to_string(),
        config_source: launch_context.config_source.clone(),
        snapshot_path: context_snapshot_path.display().to_string(),
        snapshot: launch_context.prompt.clone(),
    })?;

    if dry_run {
        println!("[batty] dry-run launch context for {phase}:\n");
        println!("----- BEGIN BATTY LAUNCH CONTEXT -----");
        println!("{}", launch_context.prompt);
        println!("----- END BATTY LAUNCH CONTEXT -----");
        println!(
            "\n[batty] launch context snapshot: {}",
            context_snapshot_path.display()
        );

        execution_log.log(LogEvent::RunCompleted {
            summary: "dry-run launch context composed".to_string(),
        })?;
        handle_worktree_finalize(phase, &execution_log, &phase_worktree, RunOutcome::DryRun);
        execution_log.log(LogEvent::SessionEnded {
            result: "DryRun".to_string(),
        })?;
        return Ok(());
    }

    let policy_engine = PolicyEngine::new(policy_tier, project_config.policy.auto_answer.clone());

    // 8. Get spawn config from adapter
    let spawn_config = adapter.spawn_config(&launch_context.prompt, &execution_root);

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
        idle_input_fallback: project_config.detector.idle_input_fallback,
        phase: phase.to_string(),
        logs_dir: log_dir.clone(),
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
        "\x1b[36m[batty]\x1b[0m worktree {}: {} ({})",
        if resumed_worktree {
            "resumed"
        } else {
            "created"
        },
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
                outcome = ?outcome,
                "worktree cleaned"
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

#[derive(Debug)]
struct LaunchContextSnapshot {
    prompt: String,
    instructions_path: PathBuf,
    phase_doc_path: PathBuf,
    config_source: String,
}

/// Compose launch context for agent execution.
///
/// Includes required steering docs, phase docs, board state, and effective
/// policy/default config. The resulting prompt is adapter-wrapped.
fn compose_launch_context(
    phase: &str,
    tasks: &[task::Task],
    execution_root: &Path,
    project_config: &ProjectConfig,
    policy_tier: Policy,
    adapter: &dyn agent::AgentAdapter,
    config_path: Option<&Path>,
) -> Result<LaunchContextSnapshot> {
    let instructions_path = resolve_instruction_file(execution_root, adapter)?;
    let instructions = std::fs::read_to_string(&instructions_path).with_context(|| {
        format!(
            "failed to read required agent instructions file {}",
            instructions_path.display()
        )
    })?;

    let phase_doc_path = execution_root.join("kanban").join(phase).join("PHASE.md");
    if !phase_doc_path.is_file() {
        bail!(
            "missing required phase context file: {}. Add kanban/{}/PHASE.md before running `batty work {}`",
            phase_doc_path.display(),
            phase,
            phase
        );
    }
    let phase_doc = std::fs::read_to_string(&phase_doc_path).with_context(|| {
        format!(
            "failed to read required phase context file {}",
            phase_doc_path.display()
        )
    })?;

    let raw_prompt = build_phase_prompt(
        phase,
        tasks,
        execution_root,
        &instructions_path,
        &instructions,
        &phase_doc_path,
        &phase_doc,
        project_config,
        policy_tier,
        config_path,
    );
    let wrapped_prompt = adapter.wrap_launch_prompt(&raw_prompt);

    Ok(LaunchContextSnapshot {
        prompt: wrapped_prompt,
        instructions_path,
        phase_doc_path,
        config_source: config_source_label(config_path),
    })
}

fn resolve_instruction_file(
    execution_root: &Path,
    adapter: &dyn agent::AgentAdapter,
) -> Result<PathBuf> {
    for candidate in adapter.instruction_candidates() {
        let path = execution_root.join(candidate);
        if path.is_file() {
            return Ok(path);
        }
    }

    let candidates = adapter.instruction_candidates().join(", ");
    bail!(
        "missing required agent instruction file for '{}'. Checked [{}] in {}. Add one of these files at the project root before running `batty work`",
        adapter.name(),
        candidates,
        execution_root.display()
    );
}

/// Build prompt text describing complete launch context for the agent.
#[allow(clippy::too_many_arguments)]
fn build_phase_prompt(
    phase: &str,
    tasks: &[task::Task],
    project_root: &Path,
    instructions_path: &Path,
    instructions: &str,
    phase_doc_path: &Path,
    phase_doc: &str,
    project_config: &ProjectConfig,
    policy_tier: Policy,
    config_path: Option<&Path>,
) -> String {
    let mut prompt = String::new();

    prompt.push_str(&format!(
        "You are working on the {} board for the project at {}.\n\n",
        phase,
        project_root.display()
    ));

    let backlog = tasks.iter().filter(|t| t.status == "backlog").count();
    let in_progress = tasks.iter().filter(|t| t.status == "in-progress").count();
    let done = tasks.iter().filter(|t| t.status == "done").count();

    prompt.push_str(&format!(
        "Board status: {} backlog, {} in-progress, {} done (of {} total)\n\n",
        backlog,
        in_progress,
        done,
        tasks.len()
    ));

    prompt.push_str(&format!(
        "Agent instructions source: {}\n\n",
        display_path(project_root, instructions_path)
    ));
    prompt.push_str("## Active Agent Instructions\n");
    prompt.push_str(instructions.trim());
    prompt.push_str("\n\n");

    prompt.push_str(&format!(
        "Phase context source: {}\n\n",
        display_path(project_root, phase_doc_path)
    ));
    prompt.push_str("## Phase Context\n");
    prompt.push_str(phase_doc.trim());
    prompt.push_str("\n\n");

    prompt.push_str("## Current Board State\n");
    if tasks.is_empty() {
        prompt.push_str("(no tasks)\n");
    } else {
        for t in tasks {
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
            prompt.push_str(&format!("  #{} [{}] {}{}\n", t.id, t.status, t.title, deps));
        }
    }
    prompt.push('\n');

    prompt.push_str("## .batty/config.toml Policy and Execution Defaults\n");
    prompt.push_str(&format!("source: {}\n", config_source_label(config_path)));
    prompt.push_str(&format!(
        "defaults.agent: {}\n",
        project_config.defaults.agent
    ));
    prompt.push_str(&format!(
        "defaults.policy: {}\n",
        policy_name(project_config.defaults.policy)
    ));
    prompt.push_str(&format!("effective.policy: {}\n", policy_name(policy_tier)));
    prompt.push_str(&format!(
        "defaults.dod: {}\n",
        project_config.defaults.dod.as_deref().unwrap_or("(none)")
    ));
    prompt.push_str(&format!(
        "defaults.max_retries: {}\n",
        project_config.defaults.max_retries
    ));
    prompt.push_str(&format!(
        "supervisor.enabled: {}\n",
        project_config.supervisor.enabled
    ));
    prompt.push_str(&format!(
        "supervisor.program: {}\n",
        project_config.supervisor.program
    ));
    prompt.push_str(&format!(
        "supervisor.args: [{}]\n",
        project_config.supervisor.args.join(", ")
    ));
    prompt.push_str(&format!(
        "supervisor.timeout_secs: {}\n",
        project_config.supervisor.timeout_secs
    ));
    prompt.push_str(&format!(
        "detector.silence_timeout_secs: {}\n",
        project_config.detector.silence_timeout_secs
    ));
    prompt.push_str(&format!(
        "detector.answer_cooldown_millis: {}\n",
        project_config.detector.answer_cooldown_millis
    ));
    prompt.push_str(&format!(
        "detector.unknown_request_fallback: {}\n",
        project_config.detector.unknown_request_fallback
    ));

    let mut auto_answers: Vec<_> = project_config.policy.auto_answer.iter().collect();
    auto_answers.sort_by(|(ka, _), (kb, _)| ka.cmp(kb));
    if auto_answers.is_empty() {
        prompt.push_str("policy.auto_answer: (none)\n");
    } else {
        prompt.push_str("policy.auto_answer:\n");
        for (pattern, response) in auto_answers {
            prompt.push_str(&format!("  - {:?} => {:?}\n", pattern, response));
        }
    }
    prompt.push('\n');

    prompt.push_str(
        "Follow the workflow in the active agent instructions to pick tasks, implement, test, and close them.\n",
    );
    prompt.push_str("Work through the backlog in dependency order.\n");

    prompt
}

fn display_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

fn config_source_label(config_path: Option<&Path>) -> String {
    config_path
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(defaults — no .batty/config.toml found)".to_string())
}

fn policy_name(policy: Policy) -> &'static str {
    match policy {
        Policy::Observe => "observe",
        Policy::Suggest => "suggest",
        Policy::Act => "act",
    }
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
    use std::fs;
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
    fn compose_launch_context_includes_required_sources() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("CLAUDE.md"), "# Steering\nUse workflow.\n").unwrap();
        fs::create_dir_all(tmp.path().join("kanban/phase-1")).unwrap();
        fs::write(
            tmp.path().join("kanban/phase-1/PHASE.md"),
            "# Phase 1\nBuild it.\n",
        )
        .unwrap();

        let tasks = vec![
            make_task(1, "scaffolding", "done", vec![]),
            make_task(2, "CI setup", "backlog", vec![1]),
        ];
        let adapter = crate::agent::adapter_from_name("claude").unwrap();
        let config = ProjectConfig::default();
        let snapshot = compose_launch_context(
            "phase-1",
            &tasks,
            tmp.path(),
            &config,
            Policy::Observe,
            adapter.as_ref(),
            None,
        )
        .unwrap();

        assert!(snapshot.prompt.contains("# Steering"));
        assert!(snapshot.prompt.contains("# Phase 1"));
        assert!(snapshot.prompt.contains("#2 [backlog] CI setup"));
        assert!(snapshot.prompt.contains("depends on: #1"));
        assert!(snapshot.prompt.contains("defaults.agent: claude"));
        assert!(snapshot.prompt.contains("effective.policy: observe"));
    }

    #[test]
    fn compose_launch_context_errors_when_instruction_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("kanban/phase-1")).unwrap();
        fs::write(tmp.path().join("kanban/phase-1/PHASE.md"), "Phase doc\n").unwrap();

        let adapter = crate::agent::adapter_from_name("claude").unwrap();
        let err = compose_launch_context(
            "phase-1",
            &[],
            tmp.path(),
            &ProjectConfig::default(),
            Policy::Observe,
            adapter.as_ref(),
            None,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("missing required agent instruction file"));
    }

    #[test]
    fn compose_launch_context_errors_when_phase_doc_missing() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("CLAUDE.md"), "Steering\n").unwrap();
        fs::create_dir_all(tmp.path().join("kanban/phase-1")).unwrap();

        let adapter = crate::agent::adapter_from_name("claude").unwrap();
        let err = compose_launch_context(
            "phase-1",
            &[],
            tmp.path(),
            &ProjectConfig::default(),
            Policy::Observe,
            adapter.as_ref(),
            None,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("missing required phase context file"));
    }

    #[test]
    fn compose_launch_context_applies_codex_wrapper() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("AGENTS.md"), "Codex steering\n").unwrap();
        fs::create_dir_all(tmp.path().join("kanban/phase-1")).unwrap();
        fs::write(tmp.path().join("kanban/phase-1/PHASE.md"), "Phase doc\n").unwrap();

        let adapter = crate::agent::adapter_from_name("codex").unwrap();
        let snapshot = compose_launch_context(
            "phase-1",
            &[make_task(9, "wrapping", "backlog", vec![])],
            tmp.path(),
            &ProjectConfig::default(),
            Policy::Observe,
            adapter.as_ref(),
            None,
        )
        .unwrap();

        assert!(snapshot.prompt.contains("Codex under Batty supervision"));
        assert!(snapshot.instructions_path.ends_with("AGENTS.md"));
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

        let result = run_phase(
            "nonexistent",
            &config,
            "claude",
            None,
            false,
            false,
            false,
            tmp.path(),
            None,
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("phase board not found")
        );
    }
}
