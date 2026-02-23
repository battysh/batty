//! Work command — the main orchestration pipeline.
//!
//! `batty work <phase>` reads a kanban phase board, constructs a prompt
//! for the agent describing the phase context, spawns the agent in a tmux
//! session, supervises with the orchestrator (auto-answering prompts via
//! send-keys per policy, Tier 2 supervisor agent for unknowns), and writes
//! a structured execution log.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
#[cfg(test)]
use portable_pty::PtySize;
use serde::Deserialize;
use serde_json::Value;
use tracing::{info, warn};

#[path = "worktree.rs"]
mod phase_worktree;

use crate::agent;
use crate::completion;
use crate::config::{Policy, ProjectConfig};
use crate::detector::DetectorConfig;
use crate::log::{ExecutionLog, LogEvent};
use crate::orchestrator::{self, LogFileObserver, OrchestratorConfig, StuckConfig};
use crate::policy::PolicyEngine;
use crate::task;
use crate::tier2::Tier2Config;
use phase_worktree::{CleanupDecision, RunOutcome};

#[derive(Debug)]
struct ResumeContext {
    phase: String,
    session: String,
    execution_root: PathBuf,
    log_dir: PathBuf,
    execution_log_path: PathBuf,
}

#[derive(Debug, Clone)]
struct ReworkContext {
    attempt: u32,
    feedback: String,
}

const CLAUDE_DANGEROUS_FLAG: &str = "--dangerously-skip-permissions";
const CODEX_DANGEROUS_FLAG: &str = "--dangerously-bypass-approvals-and-sandbox";

fn dangerous_flag_for_program(program: &str) -> Option<&'static str> {
    let binary = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program);

    match binary {
        "claude" => Some(CLAUDE_DANGEROUS_FLAG),
        "codex" => Some(CODEX_DANGEROUS_FLAG),
        _ => None,
    }
}

fn apply_dangerous_mode_wrapper(
    program: String,
    mut args: Vec<String>,
    enabled: bool,
) -> (String, Vec<String>) {
    if !enabled {
        return (program, args);
    }

    let Some(flag) = dangerous_flag_for_program(&program) else {
        return (program, args);
    };
    if args.iter().any(|arg| arg == flag) {
        return (program, args);
    }

    args.insert(0, flag.to_string());
    (program, args)
}

/// Resume supervision for a running phase/session without relaunching executor.
pub fn resume_phase(
    target: &str,
    project_config: &ProjectConfig,
    default_agent_name: &str,
    project_root: &Path,
) -> Result<()> {
    let resume = resolve_resume_context(target, project_root)?;
    let tasks_dir = crate::paths::resolve_kanban_root(&resume.execution_root)
        .join(&resume.phase)
        .join("tasks");
    let tasks = task::load_tasks_from_dir(&tasks_dir)
        .with_context(|| format!("failed to load tasks from {}", tasks_dir.display()))?;

    std::fs::create_dir_all(&resume.log_dir)
        .with_context(|| format!("failed to create log dir {}", resume.log_dir.display()))?;
    let execution_log = ExecutionLog::new(&resume.execution_log_path).with_context(|| {
        format!(
            "failed to open execution log {}",
            resume.execution_log_path.display()
        )
    })?;
    execution_log.log(LogEvent::SessionStarted {
        phase: resume.phase.clone(),
    })?;
    for t in &tasks {
        execution_log.log(LogEvent::TaskRead {
            task_id: t.id,
            title: t.title.clone(),
            status: t.status.clone(),
        })?;
    }

    let inferred_agent = infer_agent_from_execution_log(&resume.execution_log_path)
        .unwrap_or_else(|| default_agent_name.to_string());
    let adapter = agent::adapter_from_name(&inferred_agent)
        .or_else(|| agent::adapter_from_name(default_agent_name))
        .with_context(|| format!("unknown agent: {inferred_agent}"))?;

    let policy_engine = PolicyEngine::new(
        project_config.defaults.policy,
        project_config.policy.auto_answer.clone(),
    );

    let tier2_config = if project_config.supervisor.enabled {
        let system_prompt = crate::tier2::load_project_docs(&resume.execution_root);
        let (supervisor_program, supervisor_args) = apply_dangerous_mode_wrapper(
            project_config.supervisor.program.clone(),
            project_config.supervisor.args.clone(),
            project_config.dangerous_mode.enabled,
        );
        Some(Tier2Config {
            program: supervisor_program,
            args: supervisor_args,
            timeout: Duration::from_secs(project_config.supervisor.timeout_secs),
            system_prompt: Some(system_prompt),
            trace_io: project_config.supervisor.trace_io,
        })
    } else {
        None
    };

    let config = OrchestratorConfig {
        spawn: crate::agent::SpawnConfig {
            program: "<resume>".to_string(),
            args: vec![],
            work_dir: resume.execution_root.display().to_string(),
            env: vec![],
        },
        patterns: adapter.prompt_patterns(),
        policy: policy_engine,
        detector: DetectorConfig {
            silence_timeout: Duration::from_secs(project_config.detector.silence_timeout_secs),
            answer_cooldown: Duration::from_millis(project_config.detector.answer_cooldown_millis),
            unknown_request_fallback: project_config.detector.unknown_request_fallback,
        },
        idle_input_fallback: project_config.detector.idle_input_fallback,
        phase: resume.phase.clone(),
        logs_dir: resume.log_dir.clone(),
        poll_interval: OrchestratorConfig::default_poll_interval(),
        buffer_size: OrchestratorConfig::default_buffer_size(),
        tier2: tier2_config,
        log_pane: true,
        log_pane_height_pct: 20,
        stuck: Some(StuckConfig::default()),
        answer_delay: Duration::from_secs(1),
        auto_attach: false,
    };

    let orch_log = resume.log_dir.join("orchestrator.log");
    let observer = LogFileObserver::new(&orch_log)?;

    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();
    ctrlc::set_handler(move || {
        stop_clone.store(true, std::sync::atomic::Ordering::Relaxed);
    })
    .ok();

    println!(
        "\x1b[36m[batty]\x1b[0m resuming {} in tmux session '{}'",
        resume.phase, resume.session
    );
    println!(
        "\x1b[36m[batty]\x1b[0m worktree: {}",
        resume.execution_root.display()
    );

    let result = orchestrator::resume(config, Box::new(observer), stop)?;

    let completion = completion::evaluate_phase_completion(
        &resume.phase,
        &resume.execution_root,
        project_config,
        &result,
    )?;

    if completion.dod_executed {
        execution_log.log(LogEvent::TestExecuted {
            command: completion.dod_command.clone(),
            passed: completion.dod_passed,
            exit_code: completion.dod_exit_code,
        })?;
        execution_log.log(LogEvent::TestResult {
            attempt: 1,
            passed: completion.dod_passed,
            output_lines: completion.dod_output_lines,
        })?;
    }

    execution_log.log(LogEvent::CompletionDecision {
        phase: resume.phase.clone(),
        passed: completion.is_complete,
        board_all_done: completion.board_all_done,
        milestone_done: completion.milestone_done,
        summary_exists: completion.summary_exists,
        dod_passed: completion.dod_passed,
        executor_stable: completion.executor_stable,
        reasons: completion.reasons.clone(),
        summary_path: completion
            .summary_path
            .as_ref()
            .map(|p| p.display().to_string()),
        dod_command: completion.dod_command.clone(),
        dod_executed: completion.dod_executed,
        dod_exit_code: completion.dod_exit_code,
        dod_output_lines: completion.dod_output_lines,
    })?;

    let completion_reason = completion.failure_summary();
    if completion.is_complete {
        execution_log.log(LogEvent::RunCompleted {
            summary: "completion contract passed".to_string(),
        })?;
        execution_log.log(LogEvent::SessionEnded {
            result: format!("{result:?}; completion=true"),
        })?;
        println!(
            "\n\x1b[36m[batty]\x1b[0m resumed session complete. Log: {}",
            resume.execution_log_path.display()
        );
        Ok(())
    } else {
        execution_log.log(LogEvent::RunFailed {
            reason: completion_reason.clone(),
        })?;
        execution_log.log(LogEvent::SessionEnded {
            result: format!("{result:?}; completion=false"),
        })?;
        println!(
            "\n\x1b[36m[batty]\x1b[0m resumed session incomplete. Log: {}",
            resume.execution_log_path.display()
        );
        Err(anyhow!(completion_reason))
    }
}

fn resolve_resume_context(target: &str, project_root: &Path) -> Result<ResumeContext> {
    let (phase, session) = if target.starts_with("batty-") {
        let session = target.to_string();
        let execution_root = PathBuf::from(crate::tmux::session_path(&session)?);
        let phase = infer_phase_for_session(&execution_root, &session)?;
        (phase, session)
    } else {
        (target.to_string(), crate::tmux::session_name(target))
    };

    if !crate::tmux::session_exists(&session) {
        bail!(
            "tmux session '{}' not found — start with `batty work {}` first",
            session,
            phase
        );
    }

    let execution_root = PathBuf::from(crate::tmux::session_path(&session)?);
    let tasks_dir = crate::paths::resolve_kanban_root(&execution_root)
        .join(&phase)
        .join("tasks");
    if !tasks_dir.is_dir() {
        bail!(
            "phase board not found in resumed worktree: {}",
            tasks_dir.display()
        );
    }

    let log_key = log_key_for_execution_root(&execution_root)?;
    let log_dir = project_root.join(".batty").join("logs").join(log_key);
    let execution_log_path = log_dir.join("execution.jsonl");

    Ok(ResumeContext {
        phase,
        session,
        execution_root,
        log_dir,
        execution_log_path,
    })
}

fn infer_phase_for_session(execution_root: &Path, session: &str) -> Result<String> {
    let kanban_root = crate::paths::resolve_kanban_root(execution_root);
    for entry in std::fs::read_dir(&kanban_root)
        .with_context(|| format!("failed to read {}", kanban_root.display()))?
    {
        let entry = entry?;
        if !entry.path().is_dir() {
            continue;
        }
        let phase = entry.file_name().to_string_lossy().to_string();
        if crate::tmux::session_name(&phase) == session {
            return Ok(phase);
        }
    }

    bail!(
        "unable to infer phase for session '{}' from {}",
        session,
        kanban_root.display()
    )
}

fn infer_agent_from_execution_log(path: &Path) -> Option<String> {
    let body = std::fs::read_to_string(path).ok()?;
    for line in body.lines().rev() {
        let parsed: Value = match serde_json::from_str(line.trim()) {
            Ok(value) => value,
            Err(_) => continue,
        };

        if parsed.get("event")?.as_str()? != "agent_launched" {
            continue;
        }
        let agent = parsed.get("data")?.get("agent")?.as_str()?.trim();
        if !agent.is_empty() {
            return Some(agent.to_string());
        }
    }

    None
}

fn current_git_branch(repo_root: &Path) -> Result<String> {
    let output = Command::new("git")
        .arg("branch")
        .arg("--show-current")
        .current_dir(repo_root)
        .output()
        .with_context(|| format!("failed to run git in {}", repo_root.display()))?;

    if !output.status.success() {
        bail!(
            "failed to determine current branch: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() {
        bail!("detached HEAD is not supported for in-place phase runs; checkout a branch first");
    }

    Ok(branch)
}

fn log_key_for_execution_root(execution_root: &Path) -> Result<String> {
    if let Ok(branch) = current_git_branch(execution_root) {
        return Ok(branch);
    }

    execution_root
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_string())
        .ok_or_else(|| {
            anyhow!(
                "unable to infer log key for execution root {}",
                execution_root.display()
            )
        })
}

fn run_git_in_repo(repo_root: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .current_dir(repo_root)
        .args(args)
        .output()
        .with_context(|| format!("failed to run git {:?} in {}", args, repo_root.display()))
}

fn run_shell_command_in_repo(repo_root: &Path, command: &str) -> Result<Output> {
    Command::new("sh")
        .arg("-lc")
        .arg(command)
        .current_dir(repo_root)
        .output()
        .with_context(|| {
            format!(
                "failed to run shell command '{}' in {}",
                command,
                repo_root.display()
            )
        })
}

fn merge_branch_with_rebase_retry(
    repo_root: &Path,
    source_branch: &str,
    base_branch: &str,
) -> Result<()> {
    let switch_base = run_git_in_repo(repo_root, &["switch", base_branch])?;
    if !switch_base.status.success() {
        bail!(
            "failed to switch to base branch '{}': {}",
            base_branch,
            String::from_utf8_lossy(&switch_base.stderr).trim()
        );
    }

    let merge_output =
        run_git_in_repo(repo_root, &["merge", "--no-ff", "--no-edit", source_branch])?;
    if merge_output.status.success() {
        return Ok(());
    }

    let _ = run_git_in_repo(repo_root, &["merge", "--abort"]);

    let switch_source = run_git_in_repo(repo_root, &["switch", source_branch])?;
    if !switch_source.status.success() {
        bail!(
            "merge failed and unable to switch to branch '{}': {}",
            source_branch,
            String::from_utf8_lossy(&switch_source.stderr).trim()
        );
    }

    let rebase_output = run_git_in_repo(repo_root, &["rebase", base_branch])?;
    if !rebase_output.status.success() {
        let _ = run_git_in_repo(repo_root, &["rebase", "--abort"]);
        let _ = run_git_in_repo(repo_root, &["switch", base_branch]);
        bail!(
            "merge conflict unresolved after rebase retry: {}",
            String::from_utf8_lossy(&rebase_output.stderr).trim()
        );
    }

    let switch_base = run_git_in_repo(repo_root, &["switch", base_branch])?;
    if !switch_base.status.success() {
        bail!(
            "rebase succeeded but failed to switch back to '{}': {}",
            base_branch,
            String::from_utf8_lossy(&switch_base.stderr).trim()
        );
    }

    let retry_merge =
        run_git_in_repo(repo_root, &["merge", "--no-ff", "--no-edit", source_branch])?;
    if !retry_merge.status.success() {
        let _ = run_git_in_repo(repo_root, &["merge", "--abort"]);
        bail!(
            "merge conflict unresolved after rebase retry: {}",
            String::from_utf8_lossy(&retry_merge.stderr).trim()
        );
    }

    Ok(())
}

fn merge_phase_branch_and_validate(
    phase_worktree: &phase_worktree::PhaseWorktree,
    project_config: &ProjectConfig,
    execution_log: &ExecutionLog,
) -> Result<()> {
    merge_branch_with_rebase_retry(
        &phase_worktree.repo_root,
        &phase_worktree.branch,
        &phase_worktree.base_branch,
    )?;
    execution_log.log(LogEvent::Merge {
        source: phase_worktree.branch.clone(),
        target: phase_worktree.base_branch.clone(),
    })?;

    let verify_command = project_config
        .defaults
        .dod
        .clone()
        .unwrap_or_else(|| "cargo test".to_string());
    let verify = run_shell_command_in_repo(&phase_worktree.repo_root, &verify_command)?;
    let output_lines = String::from_utf8_lossy(&verify.stdout).lines().count()
        + String::from_utf8_lossy(&verify.stderr).lines().count();

    execution_log.log(LogEvent::TestExecuted {
        command: verify_command.clone(),
        passed: verify.status.success(),
        exit_code: verify.status.code(),
    })?;
    execution_log.log(LogEvent::TestResult {
        attempt: 1,
        passed: verify.status.success(),
        output_lines,
    })?;

    if !verify.status.success() {
        bail!(
            "post-merge verification failed for '{}': {}",
            verify_command,
            String::from_utf8_lossy(&verify.stderr).trim()
        );
    }

    Ok(())
}

/// Run the full work pipeline for a phase.
#[allow(clippy::too_many_arguments)] // Phase launch combines config and runtime toggles; keeping explicit args avoids opaque builders.
pub fn run_phase(
    phase: &str,
    project_config: &ProjectConfig,
    agent_name: &str,
    policy_override: Option<&str>,
    auto_attach: bool,
    use_worktree: bool,
    force_new_worktree: bool,
    dry_run: bool,
    project_root: &Path,
    config_path: Option<&Path>,
) -> Result<()> {
    run_phase_with_rework(
        phase,
        project_config,
        agent_name,
        policy_override,
        auto_attach,
        use_worktree,
        force_new_worktree,
        dry_run,
        project_root,
        config_path,
        None,
        0,
    )
}

fn resolve_policy_tier(
    policy_override: Option<&str>,
    project_config: &ProjectConfig,
) -> Result<Policy> {
    let policy_tier = match policy_override {
        Some("observe") => crate::config::Policy::Observe,
        Some("suggest") => crate::config::Policy::Suggest,
        Some("act") => crate::config::Policy::Act,
        Some(other) => bail!("unknown policy: {other} (expected observe/suggest/act)"),
        None => project_config.defaults.policy,
    };
    Ok(policy_tier)
}

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn truncate_label(input: &str, max_chars: usize) -> String {
    let compact = input.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        compact
    } else {
        let truncated = compact.chars().take(max_chars).collect::<String>();
        format!("{truncated}...")
    }
}

fn parallel_agent_slot_names(parallel: u32) -> Vec<String> {
    (1..=parallel).map(|idx| format!("agent-{idx}")).collect()
}

fn generate_parallel_agent_names(parallel: u32, execution_root: &Path) -> Result<Vec<String>> {
    let mut names = Vec::with_capacity(parallel as usize);
    let mut seen = HashSet::new();

    for idx in 1..=parallel {
        let mut selected = None;
        for _ in 0..8 {
            let candidate = generate_claim_identity(execution_root)?;
            if seen.insert(candidate.clone()) {
                selected = Some(candidate);
                break;
            }
        }

        if let Some(name) = selected {
            names.push(name);
            continue;
        }

        // Deterministic fallback if kanban-md repeatedly generated collisions.
        for candidate in parallel_agent_slot_names(parallel.saturating_mul(2).max(8)) {
            if seen.insert(candidate.clone()) {
                names.push(candidate);
                break;
            }
        }

        if names.len() != idx as usize {
            bail!("failed to generate unique parallel agent names");
        }
    }

    Ok(names)
}

fn setup_parallel_log_pane(
    window_target: &str,
    executor_pane: &str,
    log_path: &Path,
    split_mode: crate::tmux::SplitMode,
) -> Result<()> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;

    let tail_cmd = vec![
        "tail".to_string(),
        "-f".to_string(),
        log_path.display().to_string(),
    ];

    match split_mode {
        crate::tmux::SplitMode::Lines => {
            if let Err(e) = crate::tmux::split_window_vertical_lines(window_target, 10, &tail_cmd) {
                warn!(error = %e, "parallel log pane creation with -l failed");
                return Ok(());
            }
        }
        crate::tmux::SplitMode::Percent => {
            if let Err(e) = crate::tmux::split_window_vertical_percent(window_target, 20, &tail_cmd)
            {
                warn!(error = %e, "parallel log pane creation with -p failed");
                return Ok(());
            }
        }
        crate::tmux::SplitMode::Disabled => return Ok(()),
    }

    let _ = Command::new("tmux")
        .args(["select-pane", "-t", executor_pane])
        .output();

    Ok(())
}

#[allow(clippy::too_many_arguments)] // Parallel launch path mirrors run_phase controls.
pub fn run_phase_parallel(
    phase: &str,
    parallel: u32,
    project_config: &ProjectConfig,
    agent_name: &str,
    policy_override: Option<&str>,
    auto_attach: bool,
    use_worktree: bool,
    force_new_worktree: bool,
    dry_run: bool,
    project_root: &Path,
    config_path: Option<&Path>,
) -> Result<()> {
    if parallel <= 1 {
        return run_phase(
            phase,
            project_config,
            agent_name,
            policy_override,
            auto_attach,
            use_worktree,
            force_new_worktree,
            dry_run,
            project_root,
            config_path,
        );
    }

    let phase_dir = crate::paths::resolve_kanban_root(project_root).join(phase);
    let source_tasks_dir = phase_dir.join("tasks");
    if !source_tasks_dir.is_dir() {
        bail!(
            "phase board not found: {} (expected {})",
            phase,
            source_tasks_dir.display()
        );
    }
    let source_dag = crate::dag::TaskDag::from_tasks_dir(&source_tasks_dir)?;
    let _ = source_dag.topological_sort()?;

    let adapter = agent::adapter_from_name(agent_name)
        .with_context(|| format!("unknown agent: {agent_name}"))?;
    let policy_tier = resolve_policy_tier(policy_override, project_config)?;
    let claim_identity = resolve_claim_identity(phase, project_root)?;
    let slots = generate_parallel_agent_names(parallel, project_root)?;
    let worktrees =
        phase_worktree::prepare_agent_worktrees(project_root, phase, &slots, force_new_worktree)
            .with_context(|| format!("failed to prepare per-agent worktrees for phase '{phase}'"))?;
    let mut branch_by_agent = HashMap::new();
    for (idx, worktree) in worktrees.iter().enumerate() {
        branch_by_agent.insert(slots[idx].clone(), worktree.branch.clone());
    }

    let tmux_caps = crate::tmux::probe_capabilities()?;
    if !tmux_caps.pipe_pane {
        bail!("{}", tmux_caps.remediation_message());
    }

    let session = crate::tmux::session_name(phase);
    if crate::tmux::session_exists(&session) {
        bail!(
            "tmux session '{}' already exists — attach with `batty attach {}` or kill it first",
            session,
            phase
        );
    }

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let run_log_dir = project_root
        .join(".batty")
        .join("logs")
        .join(format!("parallel-{phase}-{timestamp}"));
    std::fs::create_dir_all(&run_log_dir)
        .with_context(|| format!("failed to create parallel log dir {}", run_log_dir.display()))?;
    let mut agent_panes: HashMap<String, String> = HashMap::new();

    for (index, worktree) in worktrees.iter().enumerate() {
        let slot = &slots[index];
        let tasks_dir = crate::paths::resolve_kanban_root(&worktree.path)
            .join(phase)
            .join("tasks");
        let tasks = task::load_tasks_from_dir(&tasks_dir).with_context(|| {
            format!(
                "failed to load tasks from {} for {}",
                tasks_dir.display(),
                slot
            )
        })?;

        let launch_context = compose_launch_context(
            phase,
            &tasks,
            &worktree.path,
            None,
            &claim_identity.agent,
            claim_identity.source.as_str(),
            project_config,
            policy_tier,
            adapter.as_ref(),
            config_path,
        )?;

        let prompt = format!(
            "## Parallel Agent Slot\n\
slot.name: {slot}\n\
slot.index: {}\n\
slot.total: {parallel}\n\
slot.claim_agent_name: {slot}\n\n\
Use claim.agent_name = {slot} for all kanban-md --claim operations in this slot, even if other context shows a different claim identity.\n\n\
{}",
            index + 1,
            launch_context.prompt
        );

        let mut spawn_config = adapter.spawn_config(&prompt, &worktree.path);
        let (program, args) = apply_dangerous_mode_wrapper(
            spawn_config.program,
            spawn_config.args,
            project_config.dangerous_mode.enabled,
        );
        spawn_config.program = program;
        spawn_config.args = args;

        if dry_run {
            println!(
                "[batty] dry-run {} -> worktree={} branch={} cmd={} {}",
                slot,
                worktree.path.display(),
                worktree.branch,
                spawn_config.program,
                spawn_config.args.join(" ")
            );
            continue;
        }

        if index == 0 {
            crate::tmux::create_session(
                &session,
                &spawn_config.program,
                &spawn_config.args,
                &spawn_config.work_dir,
            )?;
            crate::tmux::rename_window(&format!("{session}:0"), slot)?;
            let _ = crate::tmux::tmux_set(&session, "remain-on-exit", "on");
        } else {
            crate::tmux::create_window(
                &session,
                slot,
                &spawn_config.program,
                &spawn_config.args,
                &spawn_config.work_dir,
            )?;
        }

        let window_target = format!("{session}:{slot}");
        let pane = crate::tmux::pane_id(&window_target)?;
        let slot_log_dir = run_log_dir.join(slot);
        std::fs::create_dir_all(&slot_log_dir)?;
        crate::tmux::setup_pipe_pane(&pane, &slot_log_dir.join("pty-output.log"))?;
        setup_parallel_log_pane(
            &window_target,
            &pane,
            &slot_log_dir.join("orchestrator.log"),
            tmux_caps.split_mode,
        )?;
        agent_panes.insert(slot.clone(), pane.clone());

        info!(
            phase = phase,
            slot = slot,
            branch = %worktree.branch,
            path = %worktree.path.display(),
            "parallel agent slot launched"
        );
    }

    if dry_run {
        return Ok(());
    }

    let _ = crate::tmux::set_status_left(
        &session,
        &format!(" [batty] {phase} | parallel {parallel} agents"),
    );
    let _ = crate::tmux::set_status_right(&session, "[running]");
    let _ = crate::tmux::select_window(&format!("{}:{}", session, slots[0]));

    println!(
        "\x1b[36m[batty]\x1b[0m started {} parallel agent windows in session '{}'",
        parallel, session
    );
    println!("\x1b[36m[batty]\x1b[0m logs: {}", run_log_dir.display());
    println!(
        "\x1b[36m[batty]\x1b[0m claim identity: {} ({})",
        claim_identity.agent,
        claim_identity.source.as_str()
    );

    if auto_attach && std::io::stdout().is_terminal() && std::io::stdin().is_terminal() {
        let session_for_attach = session.clone();
        std::thread::spawn(move || {
            let _ = crate::tmux::attach(&session_for_attach);
        });
    } else {
        println!("\x1b[36m[batty]\x1b[0m attach with: batty attach {}", phase);
    }

    let scheduler_config = crate::scheduler::SchedulerConfig::default();
    let poll_interval = scheduler_config.poll_interval;
    let mut scheduler = crate::scheduler::Scheduler::new(
        phase_dir.clone(),
        slots.clone(),
        scheduler_config,
        crate::scheduler::ShellCommandRunner,
    );
    let merge_target_branch = current_git_branch(project_root)
        .context("failed to determine merge target branch for parallel merge queue")?;
    let verify_command = project_config
        .defaults
        .dod
        .clone()
        .unwrap_or_else(|| "cargo test".to_string());
    let mut merge_queue = crate::merge_queue::MergeQueue::new(
        project_root.to_path_buf(),
        merge_target_branch.clone(),
        verify_command,
        1,
    );
    let mut active_assignments: HashMap<String, (u32, String, u64)> = HashMap::new();

    loop {
        let now = now_epoch_secs();
        let pre_states = scheduler.agent_states().clone();
        let tick = scheduler.tick(now)?;

        if !tick.dispatched.is_empty() {
            for dispatch in &tick.dispatched {
                println!(
                    "\x1b[36m[batty]\x1b[0m scheduler dispatched task #{} ({}) -> {}",
                    dispatch.task_id, dispatch.task_title, dispatch.agent
                );
                active_assignments.insert(
                    dispatch.agent.clone(),
                    (dispatch.task_id, dispatch.task_title.clone(), now),
                );
            }
        }

        if !tick.completed.is_empty() {
            println!(
                "\x1b[36m[batty]\x1b[0m scheduler observed completed tasks: {}",
                tick.completed
                    .iter()
                    .map(|id| format!("#{id}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );

            for task_id in &tick.completed {
                let completed_agent = pre_states.iter().find_map(|(agent, state)| {
                    if let crate::scheduler::AgentState::Busy { task_id: active, .. } = state
                        && active == task_id
                    {
                        Some(agent.clone())
                    } else {
                        None
                    }
                });

                if let Some(agent) = completed_agent
                    && let Some(branch) = branch_by_agent.get(&agent)
                {
                    active_assignments.remove(&agent);
                    merge_queue.enqueue(crate::merge_queue::MergeRequest {
                        task_id: *task_id,
                        agent,
                        branch: branch.clone(),
                    });
                    println!(
                        "\x1b[36m[batty]\x1b[0m merge queue depth: {}",
                        merge_queue.len()
                    );
                }
            }
        }

        let states = scheduler.agent_states().clone();
        let active_agents = states
            .values()
            .filter(|state| matches!(state, crate::scheduler::AgentState::Busy { .. }))
            .count();
        let waiting_agents = if tick.ready.is_empty() {
            states
                .values()
                .filter(|state| matches!(state, crate::scheduler::AgentState::Idle))
                .count()
        } else {
            0
        };

        let status_left = format!(
            " [{}/{} tasks] [{} agents] [{} merging]",
            tick.done_tasks,
            tick.total_tasks,
            slots.len(),
            usize::from(!merge_queue.is_empty())
        );
        let status_right = if waiting_agents > 0 {
            format!("[active {active_agents}] [waiting {waiting_agents}]")
        } else {
            format!("[active {active_agents}] [ready {}]", tick.ready.len())
        };
        let _ = crate::tmux::set_status_left(&session, &status_left);
        let _ = crate::tmux::set_status_right(&session, &status_right);

        for (agent, state) in &states {
            if let crate::scheduler::AgentState::Busy { task_id, .. } = state
                && !active_assignments.contains_key(agent)
            {
                active_assignments.insert(
                    agent.clone(),
                    (*task_id, format!("task-{task_id}"), now),
                );
            }
        }

        for agent in &slots {
            let label = if let Some((task_id, title, started_epoch)) = active_assignments.get(agent) {
                let elapsed_secs = now.saturating_sub(*started_epoch);
                let elapsed_mins = elapsed_secs / 60;
                let title = truncate_label(title, 20);
                format!("{agent} #{task_id} {title} {elapsed_mins}m")
            } else if tick.ready.is_empty() {
                format!("{agent} waiting-deps")
            } else {
                format!("{agent} idle")
            };
            if let Some(pane) = agent_panes.get(agent) {
                let _ = crate::tmux::rename_window(pane, &label);
            }
        }

        for (agent, state) in states {
            if let crate::scheduler::AgentState::Busy { .. } = state
                && let Some(pane) = agent_panes.get(&agent)
            {
                let alive = crate::tmux::pane_exists(pane)
                    && !crate::tmux::pane_dead(pane).unwrap_or(false);
                if !alive {
                    println!(
                        "\x1b[36m[batty]\x1b[0m scheduler detected crashed agent pane for {} — releasing claim",
                        agent
                    );
                    scheduler.handle_agent_crash(&agent)?;
                }
            }
        }

        if !tick.stuck.is_empty() {
            let detail = tick
                .stuck
                .iter()
                .map(|entry| {
                    format!(
                        "{} task #{} stalled {}s",
                        entry.agent, entry.task_id, entry.stalled_secs
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            bail!("scheduler detected stuck task(s): {detail}");
        }

        if tick.deadlocked {
            bail!(
                "scheduler deadlock: no ready tasks, all agents idle, and unfinished tasks remain"
            );
        }

        if !merge_queue.is_empty() {
            match merge_queue.process_next() {
                Ok(Some(merged)) => {
                    println!(
                        "\x1b[36m[batty]\x1b[0m merge queue merged task #{} from {} ({}) into {}",
                        merged.task_id, merged.agent, merged.branch, merge_target_branch
                    );
                }
                Ok(None) => {}
                Err(err) => {
                    bail!("merge queue failure: {err}");
                }
            }
        }

        if tick.all_done {
            println!(
                "\x1b[36m[batty]\x1b[0m scheduler complete: all active tasks are done"
            );
            break;
        }

        std::thread::sleep(poll_interval);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)] // Phase launch combines config and runtime toggles; keeping explicit args avoids opaque builders.
fn run_phase_with_rework(
    phase: &str,
    project_config: &ProjectConfig,
    agent_name: &str,
    policy_override: Option<&str>,
    auto_attach: bool,
    use_worktree: bool,
    force_new_worktree: bool,
    dry_run: bool,
    project_root: &Path,
    config_path: Option<&Path>,
    rework_context: Option<ReworkContext>,
    rework_attempt: u32,
) -> Result<()> {
    // 1. Validate the phase board exists before launching.
    let source_phase_dir = crate::paths::resolve_kanban_root(project_root).join(phase);
    let source_tasks_dir = source_phase_dir.join("tasks");

    if !source_tasks_dir.is_dir() {
        bail!(
            "phase board not found: {} (expected {})",
            phase,
            source_tasks_dir.display()
        );
    }

    // 2. Resolve execution workspace (isolated worktree or current branch).
    let (execution_root, log_key, phase_worktree, resumed_worktree) = if use_worktree {
        let (phase_worktree, resumed_worktree) =
            phase_worktree::resolve_phase_worktree(project_root, phase, force_new_worktree)
                .with_context(|| {
                    format!("failed to resolve isolated worktree for phase '{phase}'")
                })?;
        let execution_root = phase_worktree.path.clone();

        info!(
            phase = phase,
            branch = %phase_worktree.branch,
            base_branch = %phase_worktree.base_branch,
            worktree = %execution_root.display(),
            resumed = resumed_worktree,
            "phase worktree prepared"
        );

        (
            execution_root,
            phase_worktree.branch.clone(),
            Some(phase_worktree),
            resumed_worktree,
        )
    } else {
        let branch = current_git_branch(project_root)
            .context("failed to resolve current branch for in-place run")?;
        let execution_root = project_root.to_path_buf();

        info!(
            phase = phase,
            branch = %branch,
            workspace = %execution_root.display(),
            "phase run using current branch workspace"
        );

        (execution_root, branch, None, false)
    };

    // 3. Load tasks for context from the resolved workspace.
    let phase_dir = crate::paths::resolve_kanban_root(&execution_root).join(phase);
    let tasks_dir = phase_dir.join("tasks");
    let tasks = task::load_tasks_from_dir(&tasks_dir)
        .with_context(|| format!("failed to load tasks from {}", tasks_dir.display()))?;
    let task_dag = crate::dag::TaskDag::from_tasks_dir(&tasks_dir)?;
    let _ = task_dag.topological_sort()?;

    info!(
        phase = phase,
        task_count = tasks.len(),
        "loaded phase board"
    );

    // 4. Set up per-run logs under .batty/logs/<run-key>/
    let log_dir = project_root.join(".batty").join("logs").join(&log_key);
    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("failed to create run log dir {}", log_dir.display()))?;
    let log_path = log_dir.join("execution.jsonl");
    let execution_log = ExecutionLog::new(&log_path)
        .with_context(|| format!("failed to create execution log at {}", log_path.display()))?;

    info!(log = %log_path.display(), "execution log created");

    execution_log.log(LogEvent::SessionStarted {
        phase: phase.to_string(),
    })?;
    if let Some(phase_worktree) = phase_worktree.as_ref() {
        execution_log.log(LogEvent::PhaseWorktreeCreated {
            phase: phase.to_string(),
            path: execution_root.display().to_string(),
            branch: phase_worktree.branch.clone(),
            base_branch: phase_worktree.base_branch.clone(),
        })?;
    }

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
    let policy_tier = resolve_policy_tier(policy_override, project_config)?;

    // 6b. Resolve the persistent claim identity for this phase workspace.
    let claim_identity = resolve_claim_identity(phase, &execution_root)?;
    info!(
        phase = phase,
        claim_agent = %claim_identity.agent,
        source = claim_identity.source.as_str(),
        "resolved phase claim identity"
    );

    // 7. Compose deterministic launch context (with required file validation).
    let launch_context = compose_launch_context(
        phase,
        &tasks,
        &execution_root,
        rework_context.as_ref(),
        &claim_identity.agent,
        claim_identity.source.as_str(),
        project_config,
        policy_tier,
        adapter.as_ref(),
        config_path,
    )?;
    let context_snapshot_path = log_dir.join("launch-context.md");
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
        finalize_phase_worktree_if_present(
            phase,
            &execution_log,
            phase_worktree.as_ref(),
            RunOutcome::DryRun,
        );
        execution_log.log(LogEvent::SessionEnded {
            result: "DryRun".to_string(),
        })?;
        return Ok(());
    }

    let policy_engine = PolicyEngine::new(policy_tier, project_config.policy.auto_answer.clone());

    // 8. Get spawn config from adapter
    let mut spawn_config = adapter.spawn_config(&launch_context.prompt, &execution_root);
    let (program, args) = apply_dangerous_mode_wrapper(
        spawn_config.program,
        spawn_config.args,
        project_config.dangerous_mode.enabled,
    );
    spawn_config.program = program;
    spawn_config.args = args;

    execution_log.log(LogEvent::AgentLaunched {
        agent: adapter.name().to_string(),
        program: spawn_config.program.clone(),
        args: spawn_config.args.clone(),
        work_dir: spawn_config.work_dir.clone(),
    })?;

    // 9. Build orchestrator config
    let orch_log = log_dir.join("orchestrator.log");
    let observer = LogFileObserver::new(&orch_log)?;

    // Load project docs for Tier 2 supervisor context
    let tier2_config = if project_config.supervisor.enabled {
        let system_prompt = crate::tier2::load_project_docs(&execution_root);
        let (supervisor_program, supervisor_args) = apply_dangerous_mode_wrapper(
            project_config.supervisor.program.clone(),
            project_config.supervisor.args.clone(),
            project_config.dangerous_mode.enabled,
        );
        Some(Tier2Config {
            program: supervisor_program,
            args: supervisor_args,
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
    if let Some(phase_worktree) = phase_worktree.as_ref() {
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
    } else {
        println!(
            "\x1b[36m[batty]\x1b[0m workspace: current branch {} ({})",
            log_key,
            execution_root.display()
        );
    }
    println!(
        "\x1b[36m[batty]\x1b[0m claim identity: {} ({})",
        claim_identity.agent,
        claim_identity.source.as_str()
    );
    if !auto_attach {
        println!("\x1b[36m[batty]\x1b[0m attach with: batty attach {}", phase);
    }

    let result = match orchestrator::run(config, Box::new(observer), stop) {
        Ok(result) => result,
        Err(e) => {
            finalize_phase_worktree_if_present(
                phase,
                &execution_log,
                phase_worktree.as_ref(),
                RunOutcome::Failed,
            );
            return Err(e);
        }
    };

    // 12. Evaluate deterministic completion contract.
    let completion = match completion::evaluate_phase_completion(
        phase,
        &execution_root,
        project_config,
        &result,
    ) {
        Ok(c) => c,
        Err(e) => {
            finalize_phase_worktree_if_present(
                phase,
                &execution_log,
                phase_worktree.as_ref(),
                RunOutcome::Failed,
            );
            return Err(e);
        }
    };

    if completion.dod_executed {
        execution_log.log(LogEvent::TestExecuted {
            command: completion.dod_command.clone(),
            passed: completion.dod_passed,
            exit_code: completion.dod_exit_code,
        })?;
        execution_log.log(LogEvent::TestResult {
            attempt: 1,
            passed: completion.dod_passed,
            output_lines: completion.dod_output_lines,
        })?;
    }

    execution_log.log(LogEvent::CompletionDecision {
        phase: phase.to_string(),
        passed: completion.is_complete,
        board_all_done: completion.board_all_done,
        milestone_done: completion.milestone_done,
        summary_exists: completion.summary_exists,
        dod_passed: completion.dod_passed,
        executor_stable: completion.executor_stable,
        reasons: completion.reasons.clone(),
        summary_path: completion
            .summary_path
            .as_ref()
            .map(|p| p.display().to_string()),
        dod_command: completion.dod_command.clone(),
        dod_executed: completion.dod_executed,
        dod_exit_code: completion.dod_exit_code,
        dod_output_lines: completion.dod_output_lines,
    })?;

    let mut run_accepted = completion.is_complete;
    let mut completion_reason = completion.failure_summary();

    if completion.is_complete && phase_worktree.is_some() {
        let base_branch = phase_worktree
            .as_ref()
            .map(|worktree| worktree.base_branch.as_str())
            .unwrap_or("main");

        let review_packet = match crate::review::generate_review_packet(
            phase,
            &execution_root,
            &log_path,
            &log_key,
            base_branch,
        ) {
            Ok(packet) => packet,
            Err(e) => {
                finalize_phase_worktree_if_present(
                    phase,
                    &execution_log,
                    phase_worktree.as_ref(),
                    RunOutcome::Failed,
                );
                return Err(e);
            }
        };

        execution_log.log(LogEvent::ReviewPacketGenerated {
            phase: phase.to_string(),
            packet_path: review_packet.path.display().to_string(),
            diff_command: review_packet.diff_command.clone(),
            summary_path: review_packet
                .summary_path
                .as_ref()
                .map(|path| path.display().to_string()),
            statements_count: review_packet.statements.len(),
            execution_log_path: review_packet.execution_log_path.display().to_string(),
        })?;

        println!(
            "\x1b[36m[batty]\x1b[0m review packet generated: {}",
            review_packet.path.display()
        );

        let review_decision = match crate::review::capture_review_decision() {
            Ok(decision) => decision,
            Err(e) => {
                finalize_phase_worktree_if_present(
                    phase,
                    &execution_log,
                    phase_worktree.as_ref(),
                    RunOutcome::Failed,
                );
                return Err(e);
            }
        };

        execution_log.log(LogEvent::ReviewDecision {
            phase: phase.to_string(),
            decision: review_decision.label().to_string(),
            feedback: review_decision.feedback().map(|value| value.to_string()),
        })?;

        match review_decision {
            crate::review::ReviewDecision::Merge => {
                if let Some(worktree) = phase_worktree.as_ref()
                    && let Err(err) =
                        merge_phase_branch_and_validate(worktree, project_config, &execution_log)
                {
                    run_accepted = false;
                    completion_reason = format!("review decision: escalate ({err})");
                }
            }
            crate::review::ReviewDecision::Rework { feedback } => {
                let next_attempt = rework_attempt + 1;
                if next_attempt > project_config.defaults.max_retries {
                    run_accepted = false;
                    completion_reason = format!(
                        "review decision: rework retries exceeded (attempt {next_attempt} > max {})",
                        project_config.defaults.max_retries
                    );
                } else {
                    execution_log.log(LogEvent::ReworkCycleStarted {
                        phase: phase.to_string(),
                        attempt: next_attempt,
                        max_retries: project_config.defaults.max_retries,
                        feedback: feedback.clone(),
                    })?;
                    execution_log.log(LogEvent::RunFailed {
                        reason: format!("review decision: rework ({feedback})"),
                    })?;
                    execution_log.log(LogEvent::SessionEnded {
                        result: format!(
                            "{result:?}; completion=false; rework_cycle={next_attempt}"
                        ),
                    })?;

                    println!(
                        "\x1b[36m[batty]\x1b[0m rework requested (attempt {next_attempt}/{}), relaunching in same worktree",
                        project_config.defaults.max_retries
                    );

                    return run_phase_with_rework(
                        phase,
                        project_config,
                        agent_name,
                        policy_override,
                        auto_attach,
                        use_worktree,
                        false,
                        dry_run,
                        project_root,
                        config_path,
                        Some(ReworkContext {
                            attempt: next_attempt,
                            feedback,
                        }),
                        next_attempt,
                    );
                }
            }
            crate::review::ReviewDecision::Escalate { feedback } => {
                run_accepted = false;
                completion_reason = format!("review decision: escalate ({feedback})");
            }
        }
    }

    if run_accepted {
        execution_log.log(LogEvent::RunCompleted {
            summary: "completion contract and review gate passed".to_string(),
        })?;
        info!("session completed");
    } else {
        execution_log.log(LogEvent::RunFailed {
            reason: completion_reason.clone(),
        })?;
        warn!(reason = %completion_reason, "session failed completion contract");
    }

    let run_outcome = if run_accepted {
        RunOutcome::Completed
    } else {
        RunOutcome::Failed
    };
    finalize_phase_worktree_if_present(phase, &execution_log, phase_worktree.as_ref(), run_outcome);

    execution_log.log(LogEvent::SessionEnded {
        result: format!("{result:?}; completion={run_accepted}"),
    })?;

    if run_accepted {
        println!(
            "\n\x1b[36m[batty]\x1b[0m session complete. Log: {}",
            log_path.display()
        );
        Ok(())
    } else {
        println!(
            "\n\x1b[36m[batty]\x1b[0m session incomplete. Log: {}",
            log_path.display()
        );
        Err(anyhow!(completion_reason))
    }
}

#[allow(clippy::too_many_arguments)] // Sequencer path shares the same runtime controls as single-phase runs.
pub fn run_all_phases(
    project_config: &ProjectConfig,
    agent_name: &str,
    policy_override: Option<&str>,
    auto_attach: bool,
    use_worktree: bool,
    force_new_worktree: bool,
    dry_run: bool,
    project_root: &Path,
    config_path: Option<&Path>,
) -> Result<()> {
    let failure_policy = match std::env::var("BATTY_CONTINUE_ON_FAILURE")
        .ok()
        .as_deref()
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("1") | Some("true") | Some("yes") => {
            crate::sequencer::SequencerFailurePolicy::ContinueOnFailure
        }
        _ => crate::sequencer::SequencerFailurePolicy::StopOnFailure,
    };

    let discovery = crate::sequencer::discover_phases_for_sequencing(project_root)?;
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let log_dir = project_root
        .join(".batty")
        .join("logs")
        .join(format!("work-all-{now_secs}"));
    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("failed to create sequencer log dir {}", log_dir.display()))?;
    let execution_log_path = log_dir.join("execution.jsonl");
    let execution_log = ExecutionLog::new(&execution_log_path).with_context(|| {
        format!(
            "failed to create sequencer execution log at {}",
            execution_log_path.display()
        )
    })?;
    execution_log.log(LogEvent::SessionStarted {
        phase: "all".to_string(),
    })?;
    crate::sequencer::log_phase_selection_decisions(&execution_log, &discovery.decisions)?;

    if discovery.selected.is_empty() {
        println!("\x1b[36m[batty]\x1b[0m no incomplete numeric phases found.");
        execution_log.log(LogEvent::RunCompleted {
            summary: "no incomplete phases to run".to_string(),
        })?;
        execution_log.log(LogEvent::SessionEnded {
            result: "Completed".to_string(),
        })?;
        return Ok(());
    }

    let effective_use_worktree = if dry_run {
        use_worktree
    } else if use_worktree {
        true
    } else {
        println!(
            "\x1b[36m[batty]\x1b[0m forcing --worktree for `batty work all` to support review/merge loops."
        );
        true
    };

    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();
    ctrlc::set_handler(move || {
        stop_clone.store(true, std::sync::atomic::Ordering::Relaxed);
    })
    .ok();

    for phase in discovery.selected {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            execution_log.log(LogEvent::RunFailed {
                reason: "stopped by user interrupt".to_string(),
            })?;
            execution_log.log(LogEvent::SessionEnded {
                result: "Interrupted".to_string(),
            })?;
            bail!("stopped by user interrupt");
        }

        println!("\x1b[36m[batty]\x1b[0m running phase {}", phase.name);
        match run_phase_with_rework(
            &phase.name,
            project_config,
            agent_name,
            policy_override,
            auto_attach,
            effective_use_worktree,
            force_new_worktree,
            dry_run,
            project_root,
            config_path,
            None,
            0,
        ) {
            Ok(()) => {
                execution_log.log(LogEvent::RunCompleted {
                    summary: format!("phase {} completed", phase.name),
                })?;
                let _ = crate::sequencer::should_continue_after_phase(
                    crate::sequencer::PhaseRunOutcome::Merged,
                    failure_policy,
                );
            }
            Err(err) => {
                let reason = format!("phase {} failed: {err}", phase.name);
                execution_log.log(LogEvent::RunFailed {
                    reason: reason.clone(),
                })?;

                let continue_run = crate::sequencer::should_continue_after_phase(
                    if reason.contains("review decision: escalate") {
                        crate::sequencer::PhaseRunOutcome::Escalated
                    } else {
                        crate::sequencer::PhaseRunOutcome::Failed
                    },
                    failure_policy,
                );
                if !continue_run {
                    execution_log.log(LogEvent::SessionEnded {
                        result: "Failed".to_string(),
                    })?;
                    return Err(anyhow!(reason));
                }
            }
        }
    }

    execution_log.log(LogEvent::SessionEnded {
        result: "Completed".to_string(),
    })?;
    println!("\x1b[36m[batty]\x1b[0m all eligible phases processed.");
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

fn finalize_phase_worktree_if_present(
    phase: &str,
    execution_log: &ExecutionLog,
    phase_worktree: Option<&phase_worktree::PhaseWorktree>,
    outcome: RunOutcome,
) {
    if let Some(phase_worktree) = phase_worktree {
        handle_worktree_finalize(phase, execution_log, phase_worktree, outcome);
    }
}

#[derive(Debug)]
struct LaunchContextSnapshot {
    prompt: String,
    instructions_path: PathBuf,
    phase_doc_path: PathBuf,
    config_source: String,
}

#[derive(Debug, Clone)]
struct ClaimIdentity {
    agent: String,
    source: ClaimIdentitySource,
}

#[derive(Debug, Clone, Copy)]
enum ClaimIdentitySource {
    Persisted,
    ActivityLog,
    Generated,
}

impl ClaimIdentitySource {
    fn as_str(self) -> &'static str {
        match self {
            ClaimIdentitySource::Persisted => "persisted",
            ClaimIdentitySource::ActivityLog => "activity-log",
            ClaimIdentitySource::Generated => "generated",
        }
    }
}

#[derive(Debug, Deserialize)]
struct ActivityEntry {
    action: String,
    detail: Option<String>,
}

/// Compose launch context for agent execution.
///
/// Includes required steering docs, phase docs, board state, and effective
/// policy/default config. The resulting prompt is adapter-wrapped.
#[allow(clippy::too_many_arguments)] // Context composition intentionally receives all inputs explicitly for deterministic snapshots.
fn compose_launch_context(
    phase: &str,
    tasks: &[task::Task],
    execution_root: &Path,
    rework_context: Option<&ReworkContext>,
    claim_agent_name: &str,
    claim_agent_source: &str,
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

    let phase_doc_path = crate::paths::resolve_kanban_root(execution_root)
        .join(phase)
        .join("PHASE.md");
    if !phase_doc_path.is_file() {
        bail!(
            "missing required phase context file: {}. Add a PHASE.md in your phase directory before running `batty work {}`",
            phase_doc_path.display(),
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
        rework_context,
        claim_agent_name,
        claim_agent_source,
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
    rework_context: Option<&ReworkContext>,
    claim_agent_name: &str,
    claim_agent_source: &str,
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

    prompt.push_str("## Phase Claim Identity\n");
    prompt.push_str(&format!("claim.agent_name: {}\n", claim_agent_name));
    prompt.push_str(&format!("claim.source: {}\n", claim_agent_source));
    prompt.push_str(
        "Use this exact claim agent name for all `kanban-md ... --claim` commands in this phase workspace, including after restarts.\n",
    );
    prompt.push_str(
        "If workflow docs mention `kanban-md agent-name`, skip it and reuse `claim.agent_name`.\n\n",
    );

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

    if let Some(context) = rework_context {
        prompt.push_str("## Reviewer Feedback (Rework Loop)\n");
        prompt.push_str(&format!("rework.attempt: {}\n", context.attempt));
        prompt.push_str("Address this reviewer feedback before requesting review again:\n");
        prompt.push_str(context.feedback.trim());
        prompt.push_str("\n\n");
    }

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
        "dangerous_mode.enabled: {}\n",
        project_config.dangerous_mode.enabled
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

    prompt.push_str("## Required Completion Artifacts\n");
    prompt.push_str(
        "When this phase is complete, produce `phase-summary.md` at the repository root.\n",
    );
    prompt.push_str("`phase-summary.md` must include:\n");
    prompt.push_str("- What was done (tasks completed + outputs)\n");
    prompt.push_str("- Files changed and tests added/modified/run\n");
    prompt.push_str("- Key decisions made and why\n");
    prompt.push_str("- What was deferred or left open\n");
    prompt.push_str("- What to watch for in follow-up work\n\n");
    prompt.push_str(
        "Treat this summary plus per-task statements of work as the review packet inputs.\n\n",
    );

    prompt.push_str(
        "Follow the workflow in the active agent instructions to pick tasks, implement, test, and close them.\n",
    );
    prompt.push_str("Work through the backlog in dependency order.\n");

    prompt
}

fn resolve_claim_identity(phase: &str, execution_root: &Path) -> Result<ClaimIdentity> {
    let claim_path = claim_identity_path(execution_root);
    if let Some(agent) = read_claim_identity_file(&claim_path)? {
        return Ok(ClaimIdentity {
            agent,
            source: ClaimIdentitySource::Persisted,
        });
    }

    if let Some(agent) = latest_claim_identity_from_activity(phase, execution_root)? {
        write_claim_identity_file(&claim_path, &agent)?;
        return Ok(ClaimIdentity {
            agent,
            source: ClaimIdentitySource::ActivityLog,
        });
    }

    let agent = generate_claim_identity(execution_root)?;
    write_claim_identity_file(&claim_path, &agent)?;
    Ok(ClaimIdentity {
        agent,
        source: ClaimIdentitySource::Generated,
    })
}

fn claim_identity_path(execution_root: &Path) -> PathBuf {
    execution_root.join(".batty").join("claim-agent.txt")
}

fn read_claim_identity_file(path: &Path) -> Result<Option<String>> {
    if !path.is_file() {
        return Ok(None);
    }

    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read claim identity file {}", path.display()))?;
    Ok(normalize_claim_agent_name(&content))
}

fn write_claim_identity_file(path: &Path, agent: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create claim identity directory {}",
                parent.display()
            )
        })?;
    }

    fs::write(path, format!("{agent}\n"))
        .with_context(|| format!("failed to write claim identity file {}", path.display()))
}

fn latest_claim_identity_from_activity(
    phase: &str,
    execution_root: &Path,
) -> Result<Option<String>> {
    let activity_path = crate::paths::resolve_kanban_root(execution_root)
        .join(phase)
        .join("activity.jsonl");
    if !activity_path.is_file() {
        return Ok(None);
    }

    let file = fs::File::open(&activity_path)
        .with_context(|| format!("failed to open {}", activity_path.display()))?;
    let mut latest: Option<String> = None;
    for line in BufReader::new(file).lines() {
        let line = line.with_context(|| format!("failed reading {}", activity_path.display()))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let entry: ActivityEntry = match serde_json::from_str(trimmed) {
            Ok(entry) => entry,
            Err(err) => {
                warn!(
                    file = %activity_path.display(),
                    error = %err,
                    "skipping invalid activity log line"
                );
                continue;
            }
        };

        if entry.action != "claim" {
            continue;
        }
        let Some(detail) = entry.detail else {
            continue;
        };
        if let Some(agent) = normalize_claim_agent_name(&detail) {
            latest = Some(agent);
        }
    }

    Ok(latest)
}

fn generate_claim_identity(execution_root: &Path) -> Result<String> {
    let output = Command::new("kanban-md")
        .arg("agent-name")
        .current_dir(execution_root)
        .output()
        .with_context(|| {
            format!(
                "failed to execute `kanban-md agent-name` in {}",
                execution_root.display()
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(
            "`kanban-md agent-name` failed with status {}{}",
            output.status,
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            }
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    normalize_claim_agent_name(&stdout)
        .ok_or_else(|| anyhow!("`kanban-md agent-name` returned an empty claim identity"))
}

fn normalize_claim_agent_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_start_matches('@');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
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
#[cfg(test)]
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
    use std::path::{Path, PathBuf};

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
            None,
            "zinc-ivory",
            "persisted",
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
        assert!(snapshot.prompt.contains("claim.agent_name: zinc-ivory"));
        assert!(snapshot.prompt.contains("defaults.agent: claude"));
        assert!(snapshot.prompt.contains("effective.policy: observe"));
        assert!(snapshot.prompt.contains("dangerous_mode.enabled: false"));
        assert!(
            snapshot
                .prompt
                .contains("When this phase is complete, produce `phase-summary.md`")
        );
        assert!(snapshot.prompt.contains("What was deferred or left open"));
    }

    #[test]
    fn compose_launch_context_includes_rework_feedback_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("CLAUDE.md"), "# Steering\nUse workflow.\n").unwrap();
        fs::create_dir_all(tmp.path().join("kanban/phase-1")).unwrap();
        fs::write(
            tmp.path().join("kanban/phase-1/PHASE.md"),
            "# Phase 1\nBuild it.\n",
        )
        .unwrap();

        let adapter = crate::agent::adapter_from_name("claude").unwrap();
        let rework = ReworkContext {
            attempt: 2,
            feedback: "Fix edge-case regression in parser.".to_string(),
        };
        let snapshot = compose_launch_context(
            "phase-1",
            &[make_task(2, "fixups", "backlog", vec![])],
            tmp.path(),
            Some(&rework),
            "zinc-ivory",
            "persisted",
            &ProjectConfig::default(),
            Policy::Observe,
            adapter.as_ref(),
            None,
        )
        .unwrap();

        assert!(
            snapshot
                .prompt
                .contains("## Reviewer Feedback (Rework Loop)")
        );
        assert!(snapshot.prompt.contains("rework.attempt: 2"));
        assert!(
            snapshot
                .prompt
                .contains("Fix edge-case regression in parser.")
        );
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
            None,
            "zinc-ivory",
            "persisted",
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
            None,
            "zinc-ivory",
            "persisted",
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
            None,
            "zinc-ivory",
            "persisted",
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
    fn resolve_claim_identity_prefers_persisted_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = claim_identity_path(tmp.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "persisted-agent\n").unwrap();

        let identity = resolve_claim_identity("phase-2.5", tmp.path()).unwrap();
        assert_eq!(identity.agent, "persisted-agent");
        assert!(matches!(identity.source, ClaimIdentitySource::Persisted));
    }

    #[test]
    fn resolve_claim_identity_uses_latest_activity_claim() {
        let tmp = tempfile::tempdir().unwrap();
        let activity_path = tmp.path().join("kanban/phase-2.5/activity.jsonl");
        fs::create_dir_all(activity_path.parent().unwrap()).unwrap();
        fs::write(
            &activity_path,
            concat!(
                "{\"timestamp\":\"2026-02-21T22:32:49.146605186-05:00\",\"action\":\"claim\",\"task_id\":3,\"detail\":\"brisk-frost\"}\n",
                "{\"timestamp\":\"2026-02-21T23:13:16.788238322-05:00\",\"action\":\"claim\",\"task_id\":4,\"detail\":\"@oaken-south\"}\n"
            ),
        )
        .unwrap();

        let identity = resolve_claim_identity("phase-2.5", tmp.path()).unwrap();
        assert_eq!(identity.agent, "oaken-south");
        assert!(matches!(identity.source, ClaimIdentitySource::ActivityLog));

        let persisted = fs::read_to_string(claim_identity_path(tmp.path())).unwrap();
        assert_eq!(persisted.trim(), "oaken-south");
    }

    #[test]
    fn normalize_claim_agent_name_strips_prefix() {
        assert_eq!(
            normalize_claim_agent_name(" @zinc-ivory ").as_deref(),
            Some("zinc-ivory")
        );
        assert!(normalize_claim_agent_name("   ").is_none());
    }

    #[test]
    fn apply_dangerous_mode_wrapper_disabled_is_passthrough() {
        let (program, args) = apply_dangerous_mode_wrapper(
            "claude".to_string(),
            vec!["--prompt".to_string(), "task".to_string()],
            false,
        );
        assert_eq!(program, "claude");
        assert_eq!(args, vec!["--prompt", "task"]);
    }

    #[test]
    fn apply_dangerous_mode_wrapper_adds_claude_flag() {
        let (program, args) = apply_dangerous_mode_wrapper(
            "/usr/local/bin/claude".to_string(),
            vec!["--prompt".to_string(), "task".to_string()],
            true,
        );
        assert_eq!(program, "/usr/local/bin/claude");
        assert_eq!(
            args,
            vec![
                CLAUDE_DANGEROUS_FLAG.to_string(),
                "--prompt".to_string(),
                "task".to_string()
            ]
        );
    }

    #[test]
    fn apply_dangerous_mode_wrapper_adds_codex_flag() {
        let (program, args) = apply_dangerous_mode_wrapper(
            "codex".to_string(),
            vec!["Launch context".to_string()],
            true,
        );
        assert_eq!(program, "codex");
        assert_eq!(
            args,
            vec![
                CODEX_DANGEROUS_FLAG.to_string(),
                "Launch context".to_string()
            ]
        );
    }

    #[test]
    fn apply_dangerous_mode_wrapper_does_not_duplicate_flag() {
        let (program, args) = apply_dangerous_mode_wrapper(
            "codex".to_string(),
            vec![
                CODEX_DANGEROUS_FLAG.to_string(),
                "Launch context".to_string(),
            ],
            true,
        );
        assert_eq!(program, "codex");
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], CODEX_DANGEROUS_FLAG);
    }

    #[test]
    fn apply_dangerous_mode_wrapper_ignores_unknown_program() {
        let (program, args) =
            apply_dangerous_mode_wrapper("aider".to_string(), vec!["task".to_string()], true);
        assert_eq!(program, "aider");
        assert_eq!(args, vec!["task"]);
    }

    #[test]
    fn parallel_agent_slot_names_are_stable_and_sequential() {
        assert_eq!(
            parallel_agent_slot_names(3),
            vec![
                "agent-1".to_string(),
                "agent-2".to_string(),
                "agent-3".to_string()
            ]
        );
    }

    #[test]
    fn run_phase_parallel_one_delegates_without_parallel_side_effects() {
        let tmp = tempfile::tempdir().unwrap();
        let err = run_phase_parallel(
            "phase-9",
            1,
            &ProjectConfig::default(),
            "claude",
            None,
            false,
            false,
            false,
            true,
            tmp.path(),
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("phase board not found"));
    }

    #[test]
    fn run_phase_parallel_fails_fast_on_dependency_cycle() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("kanban").join("phase-x").join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();

        fs::write(
            tasks_dir.join("001-a.md"),
            "---\nid: 1\ntitle: a\nstatus: backlog\npriority: high\ntags: []\ndepends_on:\n  - 2\nclass: standard\n---\n\nA\n",
        )
        .unwrap();
        fs::write(
            tasks_dir.join("002-b.md"),
            "---\nid: 2\ntitle: b\nstatus: backlog\npriority: high\ntags: []\ndepends_on:\n  - 1\nclass: standard\n---\n\nB\n",
        )
        .unwrap();

        let err = run_phase_parallel(
            "phase-x",
            2,
            &ProjectConfig::default(),
            "claude",
            None,
            false,
            false,
            false,
            true,
            tmp.path(),
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("dependency cycle detected"));
    }

    #[test]
    fn resolve_policy_tier_uses_override_or_default() {
        let config = ProjectConfig::default();
        assert_eq!(
            resolve_policy_tier(Some("observe"), &config).unwrap(),
            Policy::Observe
        );
        assert_eq!(
            resolve_policy_tier(Some("suggest"), &config).unwrap(),
            Policy::Suggest
        );
        assert_eq!(
            resolve_policy_tier(Some("act"), &config).unwrap(),
            Policy::Act
        );
        assert_eq!(
            resolve_policy_tier(None, &config).unwrap(),
            config.defaults.policy
        );
    }

    #[test]
    fn resolve_policy_tier_rejects_invalid_values() {
        let err = resolve_policy_tier(Some("invalid"), &ProjectConfig::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown policy"));
    }

    #[test]
    fn infer_agent_from_execution_log_reads_latest_launch_event() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("execution.jsonl");
        fs::write(
            &log,
            concat!(
                "{\"timestamp\":\"1\",\"event\":\"session_started\",\"data\":{\"phase\":\"phase-2.5\"}}\n",
                "{\"timestamp\":\"2\",\"event\":\"agent_launched\",\"data\":{\"agent\":\"claude-code\",\"program\":\"claude\",\"work_dir\":\"/tmp\"}}\n",
                "{\"timestamp\":\"3\",\"event\":\"agent_launched\",\"data\":{\"agent\":\"codex-cli\",\"program\":\"codex\",\"work_dir\":\"/tmp\"}}\n"
            ),
        )
        .unwrap();

        let agent = infer_agent_from_execution_log(&log);
        assert_eq!(agent.as_deref(), Some("codex-cli"));
    }

    #[test]
    fn infer_phase_for_session_matches_phase_directory() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("kanban").join("phase-2.5")).unwrap();

        let phase = infer_phase_for_session(tmp.path(), "batty-phase-2-5").unwrap();
        assert_eq!(phase, "phase-2.5");
    }

    #[test]
    fn terminal_size_returns_valid_dimensions() {
        let size = terminal_size();
        assert!(size.rows > 0);
        assert!(size.cols > 0);
    }

    fn git(repo: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .current_dir(repo)
            .args(args)
            .output()
            .unwrap()
    }

    fn init_git_repo() -> Option<(tempfile::TempDir, String)> {
        let version = std::process::Command::new("git")
            .arg("--version")
            .output()
            .ok()?;
        if !version.status.success() {
            return None;
        }

        let tmp = tempfile::tempdir().ok()?;
        let init = git(tmp.path(), &["init", "-q"]);
        if !init.status.success() {
            return None;
        }

        let _ = git(
            tmp.path(),
            &["config", "user.email", "batty-test@example.com"],
        );
        let _ = git(tmp.path(), &["config", "user.name", "Batty Test"]);

        fs::write(tmp.path().join("README.md"), "base\n").ok()?;
        let add = git(tmp.path(), &["add", "README.md"]);
        if !add.status.success() {
            return None;
        }
        let commit = git(tmp.path(), &["commit", "-q", "-m", "init"]);
        if !commit.status.success() {
            return None;
        }

        let branch =
            String::from_utf8_lossy(&git(tmp.path(), &["branch", "--show-current"]).stdout)
                .trim()
                .to_string();
        if branch.is_empty() {
            return None;
        }
        Some((tmp, branch))
    }

    #[test]
    fn merge_phase_branch_and_validate_merges_clean_branch() {
        let Some((tmp, base_branch)) = init_git_repo() else {
            return;
        };

        let create = git(tmp.path(), &["switch", "-c", "phase-3-run-001"]);
        assert!(create.status.success());
        fs::write(tmp.path().join("feature.txt"), "feature\n").unwrap();
        assert!(git(tmp.path(), &["add", "feature.txt"]).status.success());
        assert!(
            git(tmp.path(), &["commit", "-q", "-m", "feature"])
                .status
                .success(),
            "{}",
            String::from_utf8_lossy(&git(tmp.path(), &["status"]).stderr)
        );
        assert!(git(tmp.path(), &["switch", &base_branch]).status.success());

        let start_commit = String::from_utf8_lossy(&git(tmp.path(), &["rev-parse", "HEAD"]).stdout)
            .trim()
            .to_string();
        let phase_worktree = phase_worktree::PhaseWorktree {
            repo_root: tmp.path().to_path_buf(),
            base_branch: base_branch.clone(),
            start_commit,
            branch: "phase-3-run-001".to_string(),
            path: tmp.path().to_path_buf(),
        };

        let mut config = ProjectConfig::default();
        config.defaults.dod = Some("true".to_string());
        let log_path = tmp.path().join("execution.jsonl");
        let log = ExecutionLog::new(&log_path).unwrap();

        merge_phase_branch_and_validate(&phase_worktree, &config, &log).unwrap();

        let merged = git(
            tmp.path(),
            &[
                "merge-base",
                "--is-ancestor",
                "phase-3-run-001",
                &base_branch,
            ],
        );
        assert!(merged.status.success());

        let log_body = fs::read_to_string(log_path).unwrap();
        assert!(log_body.contains("\"event\":\"merge\""));
        assert!(log_body.contains("\"event\":\"test_executed\""));
    }

    #[test]
    fn merge_phase_branch_and_validate_escalates_on_unresolved_conflict() {
        let Some((tmp, base_branch)) = init_git_repo() else {
            return;
        };

        fs::write(tmp.path().join("shared.txt"), "line\n").unwrap();
        assert!(git(tmp.path(), &["add", "shared.txt"]).status.success());
        assert!(
            git(tmp.path(), &["commit", "-q", "-m", "shared base"])
                .status
                .success()
        );

        assert!(
            git(tmp.path(), &["switch", "-c", "phase-3-run-002"])
                .status
                .success()
        );
        fs::write(tmp.path().join("shared.txt"), "line from branch\n").unwrap();
        assert!(git(tmp.path(), &["add", "shared.txt"]).status.success());
        assert!(
            git(tmp.path(), &["commit", "-q", "-m", "branch change"])
                .status
                .success()
        );

        assert!(git(tmp.path(), &["switch", &base_branch]).status.success());
        fs::write(tmp.path().join("shared.txt"), "line from base\n").unwrap();
        assert!(git(tmp.path(), &["add", "shared.txt"]).status.success());
        assert!(
            git(tmp.path(), &["commit", "-q", "-m", "base change"])
                .status
                .success()
        );

        let start_commit = String::from_utf8_lossy(&git(tmp.path(), &["rev-parse", "HEAD"]).stdout)
            .trim()
            .to_string();
        let phase_worktree = phase_worktree::PhaseWorktree {
            repo_root: tmp.path().to_path_buf(),
            base_branch: base_branch.clone(),
            start_commit,
            branch: "phase-3-run-002".to_string(),
            path: tmp.path().to_path_buf(),
        };

        let mut config = ProjectConfig::default();
        config.defaults.dod = Some("true".to_string());
        let log = ExecutionLog::new(&tmp.path().join("execution.jsonl")).unwrap();

        let err = merge_phase_branch_and_validate(&phase_worktree, &config, &log)
            .unwrap_err()
            .to_string();
        assert!(err.contains("merge conflict unresolved"));
    }

    fn write_phase_task(project_root: &Path, phase: &str, id: u32, status: &str) {
        let tasks_dir = project_root
            .join(".batty")
            .join("kanban")
            .join(phase)
            .join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        let file = tasks_dir.join(format!("{id:03}-task-{id}.md"));
        let content = format!(
            "---\nid: {id}\ntitle: task-{id}\nstatus: {status}\npriority: high\ntags: []\ndepends_on: []\nclass: standard\n---\n\nTask {id}\n"
        );
        fs::write(file, content).unwrap();
    }

    fn write_phase_doc(project_root: &Path, phase: &str) {
        let phase_dir = project_root.join(".batty").join("kanban").join(phase);
        fs::create_dir_all(&phase_dir).unwrap();
        fs::write(phase_dir.join("PHASE.md"), format!("# {phase}\n")).unwrap();
    }

    #[test]
    fn run_all_phases_returns_when_every_phase_is_complete() {
        let tmp = tempfile::tempdir().unwrap();
        write_phase_task(tmp.path(), "phase-1", 1, "done");
        write_phase_task(tmp.path(), "phase-2", 1, "done");

        let result = run_all_phases(
            &ProjectConfig::default(),
            "claude",
            None,
            false,
            false,
            false,
            true,
            tmp.path(),
            None,
        );
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn run_all_phases_executes_discovered_phases_in_dry_run_mode() {
        let Some((tmp, _base_branch)) = init_git_repo() else {
            return;
        };
        fs::write(tmp.path().join("CLAUDE.md"), "# Steering\n").unwrap();
        write_phase_doc(tmp.path(), "phase-1");
        write_phase_doc(tmp.path(), "phase-2");
        write_phase_task(tmp.path(), "phase-1", 1, "backlog");
        write_phase_task(tmp.path(), "phase-2", 1, "backlog");
        write_phase_task(tmp.path(), "phase-9", 1, "done");

        let result = run_all_phases(
            &ProjectConfig::default(),
            "claude",
            None,
            false,
            false,
            false,
            true,
            tmp.path(),
            None,
        );
        assert!(result.is_ok(), "{result:?}");
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
