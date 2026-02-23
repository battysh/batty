mod agent;
mod cli;
mod completion;
mod config;
mod dag;
mod detector;
mod dod;
mod events;
mod install;
mod log;
mod merge_queue;
mod orchestrator;
mod paths;
mod policy;
mod prompt;
mod review;
mod scheduler;
mod sequencer;
mod shell_completion;
mod supervisor;
mod task;
mod tier2;
mod tmux;
mod work;

use anyhow::{Context, Result};
use clap::Parser;
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tracing::{debug, warn};

use cli::{Cli, Command, InstallTarget};
use config::ProjectConfig;

fn sanitize_phase_for_worktree_prefix(phase: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;

    for c in phase.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }

    let slug = out.trim_matches('-').to_string();
    if slug.is_empty() {
        "phase".to_string()
    } else {
        slug
    }
}

fn parse_run_number(name: &str, prefix: &str) -> Option<u32> {
    let suffix = name.strip_prefix(prefix)?;
    if suffix.len() < 3 || !suffix.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    suffix.parse().ok()
}

fn resolve_latest_worktree_board_dir(project_root: &Path, phase: &str) -> Result<Option<PathBuf>> {
    let worktrees_root = project_root.join(".batty").join("worktrees");
    if !worktrees_root.is_dir() {
        return Ok(None);
    }

    let phase_slug = sanitize_phase_for_worktree_prefix(phase);
    let prefix = format!("{phase_slug}-run-");
    let mut best: Option<(u32, PathBuf)> = None;

    for entry in std::fs::read_dir(&worktrees_root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let name = entry.file_name().to_string_lossy().to_string();
        let Some(run) = parse_run_number(&name, &prefix) else {
            continue;
        };
        let board_dir = paths::resolve_kanban_root(&path).join(phase);
        if !board_dir.is_dir() {
            continue;
        }

        match &best {
            Some((best_run, _)) if run <= *best_run => {}
            _ => best = Some((run, board_dir)),
        }
    }

    Ok(best.map(|(_, dir)| dir))
}

fn resolve_board_dir(project_root: &Path, phase: &str) -> Result<PathBuf> {
    let session = tmux::session_name(phase);
    if tmux::session_exists(&session) {
        let session_root = tmux::session_path(&session)?;
        let session_root_path = PathBuf::from(session_root);
        let active_board = paths::resolve_kanban_root(&session_root_path).join(phase);
        if active_board.is_dir() {
            return Ok(active_board);
        }
        warn!(
            session = %session,
            board = %active_board.display(),
            "active tmux session found but board directory missing; falling back to repo board"
        );
    }

    if let Some(worktree_board) = resolve_latest_worktree_board_dir(project_root, phase)? {
        return Ok(worktree_board);
    }

    let fallback = paths::resolve_kanban_root(project_root).join(phase);
    if fallback.is_dir() {
        return Ok(fallback);
    }

    anyhow::bail!(
        "phase board not found for '{}': checked active tmux run, latest worktree run, and fallback path {}",
        phase,
        fallback.display()
    );
}

fn policy_label(policy: config::Policy) -> &'static str {
    match policy {
        config::Policy::Observe => "observe",
        config::Policy::Suggest => "suggest",
        config::Policy::Act => "act",
    }
}

fn config_source_label(config_path: Option<&Path>) -> String {
    config_path
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(defaults — no .batty/config.toml found)".to_string())
}

fn push_kv(output: &mut String, key: &str, value: impl std::fmt::Display) {
    output.push_str(&format!("  {key:<20} {value}\n"));
}

fn render_config_human(config: &ProjectConfig, config_path: Option<&Path>) -> String {
    let mut output = String::new();
    output.push_str("Defaults\n");
    push_kv(&mut output, "agent", &config.defaults.agent);
    push_kv(&mut output, "policy", policy_label(config.defaults.policy));
    push_kv(
        &mut output,
        "dod",
        config.defaults.dod.as_deref().unwrap_or("(none)"),
    );
    push_kv(&mut output, "max_retries", config.defaults.max_retries);
    output.push('\n');

    output.push_str("Supervisor\n");
    push_kv(&mut output, "enabled", config.supervisor.enabled);
    push_kv(&mut output, "program", &config.supervisor.program);
    if config.supervisor.args.is_empty() {
        push_kv(&mut output, "args", "(none)");
    } else {
        push_kv(&mut output, "args", config.supervisor.args.join(", "));
    }
    push_kv(&mut output, "timeout_secs", config.supervisor.timeout_secs);
    push_kv(&mut output, "trace_io", config.supervisor.trace_io);
    output.push('\n');

    output.push_str("Detector\n");
    push_kv(
        &mut output,
        "silence_timeout",
        format!("{}s", config.detector.silence_timeout_secs),
    );
    push_kv(
        &mut output,
        "answer_cooldown",
        format!("{}ms", config.detector.answer_cooldown_millis),
    );
    push_kv(
        &mut output,
        "unknown_fallback",
        config.detector.unknown_request_fallback,
    );
    push_kv(
        &mut output,
        "idle_input_fallback",
        config.detector.idle_input_fallback,
    );
    output.push('\n');

    output.push_str("Dangerous Mode\n");
    push_kv(&mut output, "enabled", config.dangerous_mode.enabled);
    output.push('\n');

    output.push_str("Policy Auto Answers\n");
    let mut auto_answers: Vec<_> = config.policy.auto_answer.iter().collect();
    auto_answers.sort_by(|a, b| a.0.cmp(b.0));
    if auto_answers.is_empty() {
        push_kv(&mut output, "entries", "(none)");
    } else {
        for (prompt, answer) in auto_answers {
            output.push_str(&format!("  - {prompt} => {answer}\n"));
        }
    }
    output.push('\n');

    output.push_str("Source Path\n");
    push_kv(&mut output, "path", config_source_label(config_path));

    output
}

fn render_config_json(config: &ProjectConfig, config_path: Option<&Path>) -> Result<String> {
    let auto_answer: BTreeMap<String, String> = config
        .policy
        .auto_answer
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let payload = serde_json::json!({
        "defaults": {
            "agent": &config.defaults.agent,
            "policy": policy_label(config.defaults.policy),
            "dod": config.defaults.dod.clone(),
            "max_retries": config.defaults.max_retries
        },
        "supervisor": {
            "enabled": config.supervisor.enabled,
            "program": &config.supervisor.program,
            "args": &config.supervisor.args,
            "timeout_secs": config.supervisor.timeout_secs,
            "trace_io": config.supervisor.trace_io
        },
        "detector": {
            "silence_timeout_secs": config.detector.silence_timeout_secs,
            "answer_cooldown_millis": config.detector.answer_cooldown_millis,
            "unknown_request_fallback": config.detector.unknown_request_fallback,
            "idle_input_fallback": config.detector.idle_input_fallback
        },
        "dangerous_mode": {
            "enabled": config.dangerous_mode.enabled
        },
        "policy": {
            "auto_answer": auto_answer
        },
        "source_path": config_source_label(config_path)
    });

    serde_json::to_string_pretty(&payload).context("failed to serialize config to JSON")
}

/// Extract a sortable numeric key from a phase directory name.
///
/// Handles names like "phase-2.5" → Some(2.5), "phase-3b" → Some(3.0).
/// Returns `None` for non-numeric names like "docs-update".
fn phase_sort_key(name: &str) -> Option<f64> {
    let after_phase = name.strip_prefix("phase-")?;
    // Try parsing the whole suffix as a float (e.g. "2.5")
    if let Ok(n) = after_phase.parse::<f64>() {
        return Some(n);
    }
    // Try parsing just the leading digits (e.g. "3b" → 3.0)
    let digits: String = after_phase
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if !digits.is_empty() {
        return digits.parse::<f64>().ok();
    }
    None
}

/// Statuses that are excluded from board counts (not active work).
fn is_excluded_status(status: &str) -> bool {
    matches!(status, "archived" | "cancelled" | "wontfix")
}

/// Derive phase status from task statuses.
///
/// - All tasks done → "Done"
/// - Any task in-progress → "In Progress"
/// - All tasks in backlog/todo (none done, none in-progress) → "Not Started"
/// - Mix of done and backlog (none in-progress) → "In Progress"
/// - No tasks → "Empty"
fn derive_phase_status(tasks: &[task::Task]) -> &'static str {
    let active: Vec<_> = tasks
        .iter()
        .filter(|t| !is_excluded_status(&t.status))
        .collect();

    if active.is_empty() {
        return if tasks.is_empty() { "Empty" } else { "Done" };
    }

    let total = active.len();
    let done = active.iter().filter(|t| t.status == "done").count();
    let in_progress = active.iter().filter(|t| t.status == "in-progress").count();

    if done == total {
        "Done"
    } else if in_progress > 0 {
        "In Progress"
    } else if done > 0 {
        // Some done, some not, but nobody actively working — still "In Progress"
        "In Progress"
    } else {
        "Not Started"
    }
}

struct TaskCounts {
    todo: usize,
    in_progress: usize,
    done: usize,
    total: usize,
}

impl TaskCounts {
    fn from_tasks(tasks: &[task::Task]) -> Self {
        let active: Vec<_> = tasks
            .iter()
            .filter(|t| !is_excluded_status(&t.status))
            .collect();
        let done = active.iter().filter(|t| t.status == "done").count();
        let in_progress = active
            .iter()
            .filter(|t| t.status == "in-progress")
            .count();
        let total = active.len();
        let todo = total - done - in_progress;
        Self {
            todo,
            in_progress,
            done,
            total,
        }
    }

}

struct WorktreeInfo {
    /// Run name, e.g. "run-001".
    run_name: String,
    /// Whether a tmux session is active for this worktree.
    active: bool,
    counts: TaskCounts,
    status: String,
}

struct BoardInfo {
    name: String,
    /// Status derived from the repo (main branch) tasks.
    repo_status: String,
    repo_counts: TaskCounts,
    /// All worktrees with a board for this phase, sorted by run number.
    worktrees: Vec<WorktreeInfo>,
}

impl BoardInfo {
    /// Effective status: prefer active worktree, then latest, then repo.
    fn effective_status(&self) -> &str {
        self.best_worktree()
            .map(|w| w.status.as_str())
            .unwrap_or(&self.repo_status)
    }

    /// Effective task counts: prefer active worktree, then latest, then repo.
    fn effective_counts(&self) -> &TaskCounts {
        self.best_worktree()
            .map(|w| &w.counts)
            .unwrap_or(&self.repo_counts)
    }

    /// Best worktree: active one if any, otherwise the last (highest run number).
    fn best_worktree(&self) -> Option<&WorktreeInfo> {
        self.worktrees
            .iter()
            .find(|w| w.active)
            .or(self.worktrees.last())
    }
}

/// Format a task count cell: show "n/total" when n > 0, otherwise "-".
fn fmt_count(n: usize, total: usize) -> String {
    if n == 0 {
        "-".to_string()
    } else {
        format!("{n}/{total}")
    }
}

/// Collect all worktrees for a phase, sorted by run number ascending.
fn collect_worktrees(project_root: &Path, phase: &str) -> Vec<WorktreeInfo> {
    let worktrees_root = project_root.join(".batty").join("worktrees");
    if !worktrees_root.is_dir() {
        return Vec::new();
    }

    let phase_slug = sanitize_phase_for_worktree_prefix(phase);
    let prefix = format!("{phase_slug}-run-");
    let mut runs: Vec<(u32, WorktreeInfo)> = Vec::new();

    let entries = match std::fs::read_dir(&worktrees_root) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let dir_name = entry.file_name().to_string_lossy().to_string();
        let Some(run_num) = parse_run_number(&dir_name, &prefix) else {
            continue;
        };

        let board_dir = paths::resolve_kanban_root(&path).join(phase);
        if !board_dir.is_dir() {
            continue;
        }

        let tasks = if board_dir.join("tasks").is_dir() {
            task::load_tasks_from_dir(&board_dir.join("tasks")).unwrap_or_default()
        } else {
            Vec::new()
        };
        let counts = TaskCounts::from_tasks(&tasks);
        let status = derive_phase_status(&tasks).to_string();

        // Check if there's an active tmux session for this worktree
        let session = format!("batty-{}", dir_name);
        let active = tmux::session_exists(&session);

        let run_label = format!("run-{run_num:03}");
        runs.push((
            run_num,
            WorktreeInfo {
                run_name: run_label,
                active,
                counts,
                status,
            },
        ));
    }

    runs.sort_by_key(|(n, _)| *n);
    runs.into_iter().map(|(_, info)| info).collect()
}

/// Build a formatted listing of all boards, showing both repo and worktree state.
fn list_boards(project_root: &Path) -> Result<String> {
    let kanban_root = paths::resolve_kanban_root(project_root);
    let entries = std::fs::read_dir(&kanban_root)
        .with_context(|| format!("failed to read kanban root: {}", kanban_root.display()))?;

    let mut boards: Vec<BoardInfo> = Vec::new();

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let phase_md = path.join("PHASE.md");
        if !phase_md.exists() {
            continue;
        }

        let name = entry.file_name().to_string_lossy().to_string();

        // Repo (main) board
        let repo_tasks = if path.join("tasks").is_dir() {
            task::load_tasks_from_dir(&path.join("tasks")).unwrap_or_default()
        } else {
            Vec::new()
        };
        let repo_counts = TaskCounts::from_tasks(&repo_tasks);
        let repo_status = derive_phase_status(&repo_tasks).to_string();

        // Collect all worktrees for this phase
        let wt_infos = collect_worktrees(project_root, &name);

        boards.push(BoardInfo {
            name,
            repo_status,
            repo_counts,
            worktrees: wt_infos,
        });
    }

    // Sort: numeric phases first (by number), then alphabetic phases.
    boards.sort_by(|a, b| {
        let ka = phase_sort_key(&a.name);
        let kb = phase_sort_key(&b.name);
        match (ka, kb) {
            (Some(na), Some(nb)) => na
                .partial_cmp(&nb)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.name.cmp(&b.name)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.name.cmp(&b.name),
        }
    });

    // Effective status: use worktree status when available (for summary counts).
    let done_count = boards
        .iter()
        .filter(|b| b.effective_status() == "Done")
        .count();
    let in_progress_count = boards
        .iter()
        .filter(|b| b.effective_status() == "In Progress")
        .count();
    let not_started_count = boards
        .iter()
        .filter(|b| {
            let s = b.effective_status();
            s == "Not Started" || s == "Empty"
        })
        .count();
    let total_tasks: usize = boards.iter().map(|b| b.effective_counts().total).sum();
    let total_done: usize = boards.iter().map(|b| b.effective_counts().done).sum();

    // Column widths
    let name_width = boards
        .iter()
        .map(|b| b.name.len())
        .max()
        .unwrap_or(5)
        .max(5);
    let status_width = boards
        .iter()
        .map(|b| b.repo_status.len())
        .max()
        .unwrap_or(6)
        .max(6);

    // Width for indented worktree run labels (name_width minus 2-space indent)
    let label_width = name_width.saturating_sub(2);

    let mut output = String::new();

    // Table header
    output.push_str(&format!(
        "{:<name_width$}  {:<status_width$}  {:>5}  {:>5}  {:>5}\n",
        "Phase", "Status", "Todo", "WIP", "Done"
    ));

    for board in &boards {
        let c = &board.repo_counts;
        output.push_str(&format!(
            "{:<name_width$}  {:<status_width$}  {:>5}  {:>5}  {:>5}\n",
            board.name,
            board.repo_status,
            fmt_count(c.todo, c.total),
            fmt_count(c.in_progress, c.total),
            fmt_count(c.done, c.total),
        ));

        // Worktree detail lines, indented
        for wt in &board.worktrees {
            let wc = &wt.counts;
            let active_label = if wt.active { " *" } else { "" };
            output.push_str(&format!(
                "  {:<label_width$}  {:<status_width$}  {:>5}  {:>5}  {:>5}{}\n",
                wt.run_name,
                wt.status,
                fmt_count(wc.todo, wc.total),
                fmt_count(wc.in_progress, wc.total),
                fmt_count(wc.done, wc.total),
                active_label,
            ));
        }
    }

    // Summary line (uses effective/best-known status)
    output.push_str(&format!(
        "\n{} boards: {} done, {} in progress, {} not started ({}/{} tasks)\n",
        boards.len(),
        done_count,
        in_progress_count,
        not_started_count,
        total_done,
        total_tasks,
    ));

    Ok(output)
}

#[tokio::main]
async fn main() -> Result<()> {
    let first_arg = std::env::args().nth(1);
    let cli = Cli::parse();
    let is_quiet_meta_command = matches!(
        &cli.command,
        Command::Config { .. } | Command::Completions { .. }
    ) || first_arg.as_deref() == Some("completions");

    let filter = match cli.verbose {
        0 if is_quiet_meta_command => "batty=warn",
        0 => "batty=info",
        1 => "batty=debug",
        _ => "batty=trace",
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    if let Command::Completions { shell } = &cli.command {
        shell_completion::print(*shell)?;
        return Ok(());
    }

    let cwd =
        std::env::current_dir().context("failed to get current directory (was it deleted?)")?;
    let (config, config_path) = ProjectConfig::load(&cwd)?;

    if !is_quiet_meta_command || cli.verbose > 0 {
        match config_path {
            Some(ref p) => debug!("loaded config from {}", p.display()),
            None => debug!("no .batty/config.toml found, using defaults"),
        }
    }

    match cli.command {
        Command::Work {
            target,
            parallel,
            agent,
            policy,
            attach,
            worktree,
            new,
            dry_run,
            foreground,
        } => {
            // Detached mode: spawn a background batty worker and return immediately.
            // The worker runs with --foreground to avoid recursive spawning.
            if target != "all" && !attach && !foreground && !dry_run {
                let tasks_dir = paths::resolve_kanban_root(&cwd).join(&target).join("tasks");
                if !tasks_dir.is_dir() {
                    anyhow::bail!(
                        "phase board not found: {} (expected {})",
                        target,
                        tasks_dir.display()
                    );
                }

                let exe = std::env::current_exe()?;
                let mut cmd = std::process::Command::new(exe);
                cmd.arg("work").arg(&target).arg("--foreground");

                if parallel != 1 {
                    cmd.arg("--parallel").arg(parallel.to_string());
                }
                if let Some(ref a) = agent {
                    cmd.arg("--agent").arg(a);
                }
                if let Some(ref p) = policy {
                    cmd.arg("--policy").arg(p);
                }
                if worktree {
                    cmd.arg("--worktree");
                }
                if new {
                    cmd.arg("--new");
                }

                let log_dir = cwd.join(".batty").join("logs");
                std::fs::create_dir_all(&log_dir)?;
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let detached_log = log_dir.join(format!("detached-{target}-{ts}.log"));
                let stdout_log = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&detached_log)?;
                let stderr_log = stdout_log.try_clone()?;

                let child = cmd
                    .stdin(Stdio::null())
                    .stdout(Stdio::from(stdout_log))
                    .stderr(Stdio::from(stderr_log))
                    .spawn()?;

                println!(
                    "[batty] started detached in background (pid: {})",
                    child.id()
                );
                println!("[batty] attach with: batty attach {}", target);
                println!("[batty] detached log: {}", detached_log.display());
                return Ok(());
            }

            let agent_name = agent.as_deref().unwrap_or(&config.defaults.agent);
            let policy_str = policy.as_deref();

            if target == "all" {
                if parallel != 1 {
                    anyhow::bail!(
                        "`batty work all --parallel` is planned for phase 4; use --parallel 1 for now"
                    );
                }

                work::run_all_phases(
                    &config,
                    agent_name,
                    policy_str,
                    attach,
                    worktree,
                    new,
                    dry_run,
                    &cwd,
                    config_path.as_deref(),
                )?;
            } else {
                if parallel > 1 {
                    work::run_phase_parallel(
                        &target,
                        parallel,
                        &config,
                        agent_name,
                        policy_str,
                        attach,
                        worktree,
                        new,
                        dry_run,
                        &cwd,
                        config_path.as_deref(),
                    )?;
                } else {
                    work::run_phase(
                        &target,
                        &config,
                        agent_name,
                        policy_str,
                        attach,
                        worktree,
                        new,
                        dry_run,
                        &cwd,
                        config_path.as_deref(),
                    )?;
                }
            }
        }
        Command::Attach { target } => {
            let session = tmux::session_name(&target);
            tmux::attach(&session)?;
        }
        Command::Resume { target } => {
            work::resume_phase(&target, &config, config.defaults.agent.as_str(), &cwd)?;
        }
        Command::Config { json } => {
            if json {
                println!("{}", render_config_json(&config, config_path.as_deref())?);
            } else {
                print!("{}", render_config_human(&config, config_path.as_deref()));
            }
        }
        Command::Completions { shell } => {
            shell_completion::print(shell)?;
        }
        Command::Install { target, dir } => {
            let destination = PathBuf::from(dir);
            let prereqs = install::ensure_prerequisites()?;

            let install_target = match target {
                InstallTarget::Both => install::InstallTarget::Both,
                InstallTarget::Claude => install::InstallTarget::Claude,
                InstallTarget::Codex => install::InstallTarget::Codex,
            };
            let summary = install::install_assets(&destination, install_target)?;

            println!("Checked external prerequisites:");
            for tool in &prereqs.present {
                println!("  present:   {}", tool);
            }
            for tool in &prereqs.installed {
                println!("  installed: {}", tool);
            }

            println!(
                "Installed Batty project assets in {}",
                destination.display()
            );
            for path in &summary.created_or_updated {
                println!("  updated:   {}", path.display());
            }
            for path in &summary.unchanged {
                println!("  unchanged: {}", path.display());
            }

            if summary.kanban_skills_installed {
                println!("  kanban-md skills: installed");
            } else {
                println!("  kanban-md skills: skipped (kanban-md not available)");
            }

            if summary.gitignore_entries_added.is_empty() {
                println!("  .gitignore: already up to date");
            } else {
                for entry in &summary.gitignore_entries_added {
                    println!("  .gitignore: added {}", entry);
                }
            }
        }
        Command::Remove { target, dir } => {
            let destination = PathBuf::from(dir);
            let remove_target = match target {
                InstallTarget::Both => install::InstallTarget::Both,
                InstallTarget::Claude => install::InstallTarget::Claude,
                InstallTarget::Codex => install::InstallTarget::Codex,
            };
            let summary = install::remove_assets(&destination, remove_target)?;

            println!(
                "Removed Batty project assets from {}",
                destination.display()
            );
            for path in &summary.removed {
                println!("  removed:   {}", path.display());
            }
            for path in &summary.not_found {
                println!("  not found: {}", path.display());
            }

            if summary.kanban_skills_removed {
                println!("  kanban-md skills: removed");
            } else {
                println!(
                    "  kanban-md skills: skipped (kanban-md not available or skills not present)"
                );
            }

            if summary.gitignore_entries_removed.is_empty() {
                println!("  .gitignore: no batty entries found");
            } else {
                for entry in &summary.gitignore_entries_removed {
                    println!("  .gitignore: removed {}", entry);
                }
            }

            println!();
            println!("To fully remove Batty, also run: rm -rf .batty");
            println!("(worktrees under .batty/worktrees/ may contain local branches)");
        }
        Command::Board { target, print_dir } => {
            let board_dir = resolve_board_dir(&cwd, &target)?;
            if print_dir {
                println!("{}", board_dir.display());
                return Ok(());
            }

            let status = std::process::Command::new("kanban-md")
                .arg("tui")
                .arg("--dir")
                .arg(&board_dir)
                .status()
                .map_err(|e| anyhow::anyhow!("failed to launch kanban-md: {e}"))?;

            if !status.success() {
                anyhow::bail!("kanban-md tui exited with non-zero status");
            }
        }
        Command::List { watch, interval } => {
            if watch {
                loop {
                    // Clear screen and move cursor to top-left
                    print!("\x1b[2J\x1b[H");
                    let output = list_boards(&cwd)?;
                    print!("{output}");
                    std::io::Write::flush(&mut std::io::stdout())?;
                    std::thread::sleep(std::time::Duration::from_secs(interval));
                }
            } else {
                let output = list_boards(&cwd)?;
                print!("{output}");
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_phase_for_worktree_prefix_matches_convention() {
        assert_eq!(sanitize_phase_for_worktree_prefix("phase-2.5"), "phase-2-5");
        assert_eq!(sanitize_phase_for_worktree_prefix("Phase 7"), "phase-7");
        assert_eq!(sanitize_phase_for_worktree_prefix("///"), "phase");
    }

    #[test]
    fn resolve_latest_worktree_board_dir_prefers_highest_run() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        std::fs::create_dir_all(
            root.join(".batty")
                .join("worktrees")
                .join("phase-2-5-run-001")
                .join("kanban")
                .join("phase-2.5"),
        )
        .unwrap();
        std::fs::create_dir_all(
            root.join(".batty")
                .join("worktrees")
                .join("phase-2-5-run-003")
                .join("kanban")
                .join("phase-2.5"),
        )
        .unwrap();
        std::fs::create_dir_all(
            root.join(".batty")
                .join("worktrees")
                .join("phase-2-5-run-002"),
        )
        .unwrap();

        let resolved = resolve_latest_worktree_board_dir(root, "phase-2.5")
            .unwrap()
            .unwrap();
        assert!(resolved.ends_with("phase-2-5-run-003/kanban/phase-2.5"));
    }

    #[test]
    fn resolve_latest_worktree_board_dir_returns_none_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let resolved = resolve_latest_worktree_board_dir(tmp.path(), "phase-2.5").unwrap();
        assert!(resolved.is_none());
    }

    #[test]
    fn render_config_human_groups_sections_and_formats_arrays() {
        let config = ProjectConfig::default();
        let rendered = render_config_human(&config, None);

        assert!(rendered.contains("Defaults"));
        assert!(rendered.contains("Supervisor"));
        assert!(rendered.contains("Dangerous Mode"));
        assert!(rendered.contains("Source Path"));
        assert!(rendered.contains("args"));
        assert!(rendered.contains("-p, --output-format, text"));
        assert!(rendered.contains("(defaults — no .batty/config.toml found)"));
    }

    #[test]
    fn render_config_json_is_valid_and_contains_expected_fields() {
        let config = ProjectConfig::default();
        let json = render_config_json(&config, None).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(value["defaults"]["agent"], "claude");
        assert_eq!(value["defaults"]["policy"], "observe");
        assert!(value["supervisor"]["args"].is_array());
        assert_eq!(value["dangerous_mode"]["enabled"], false);
        assert_eq!(
            value["source_path"],
            "(defaults — no .batty/config.toml found)"
        );
    }

    #[test]
    fn render_config_json_sorts_auto_answer_keys() {
        let mut config = ProjectConfig::default();
        config
            .policy
            .auto_answer
            .insert("z-prompt".into(), "z".into());
        config
            .policy
            .auto_answer
            .insert("a-prompt".into(), "a".into());

        let json = render_config_json(&config, None).unwrap();
        let first = json.find("\"a-prompt\"").unwrap();
        let second = json.find("\"z-prompt\"").unwrap();
        assert!(first < second, "expected sorted JSON map keys");
    }

    #[test]
    fn phase_sort_key_parses_numeric_phases() {
        assert_eq!(phase_sort_key("phase-1"), Some(1.0));
        assert_eq!(phase_sort_key("phase-2.5"), Some(2.5));
        assert_eq!(phase_sort_key("phase-3b"), Some(3.0));
        assert_eq!(phase_sort_key("docs-update"), None);
    }

    fn make_task(status: &str) -> task::Task {
        task::Task {
            id: 1,
            title: String::new(),
            status: status.to_string(),
            priority: String::new(),
            tags: Vec::new(),
            depends_on: Vec::new(),
            description: String::new(),
            batty_config: None,
            source_path: PathBuf::new(),
        }
    }

    #[test]
    fn derive_phase_status_all_done() {
        let tasks = vec![make_task("done"), make_task("done")];
        assert_eq!(derive_phase_status(&tasks), "Done");
    }

    #[test]
    fn derive_phase_status_in_progress() {
        let tasks = vec![make_task("done"), make_task("in-progress")];
        assert_eq!(derive_phase_status(&tasks), "In Progress");
    }

    #[test]
    fn derive_phase_status_some_done_some_backlog() {
        let tasks = vec![make_task("done"), make_task("backlog")];
        assert_eq!(derive_phase_status(&tasks), "In Progress");
    }

    #[test]
    fn derive_phase_status_all_backlog() {
        let tasks = vec![make_task("backlog"), make_task("backlog")];
        assert_eq!(derive_phase_status(&tasks), "Not Started");
    }

    #[test]
    fn derive_phase_status_empty() {
        let tasks: Vec<task::Task> = Vec::new();
        assert_eq!(derive_phase_status(&tasks), "Empty");
    }

    /// Helper: create a kanban phase dir with PHASE.md and optional task files.
    fn create_test_phase(kanban_root: &Path, name: &str, task_statuses: &[&str]) {
        let phase_dir = kanban_root.join(name);
        std::fs::create_dir_all(&phase_dir).unwrap();
        std::fs::write(phase_dir.join("PHASE.md"), format!("# {name}\n")).unwrap();

        if !task_statuses.is_empty() {
            let tasks_dir = phase_dir.join("tasks");
            std::fs::create_dir_all(&tasks_dir).unwrap();
            for (i, status) in task_statuses.iter().enumerate() {
                let id = i + 1;
                std::fs::write(
                    tasks_dir.join(format!("{id:03}-t.md")),
                    format!("---\nid: {id}\ntitle: task {id}\nstatus: {status}\n---\nBody.\n"),
                )
                .unwrap();
            }
        }
    }

    #[test]
    fn list_boards_renders_sorted_table_with_summary() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path();
        let kanban = project.join(".batty").join("kanban");

        create_test_phase(&kanban, "phase-2", &["done", "backlog"]);
        create_test_phase(&kanban, "phase-1", &["done"]);
        create_test_phase(&kanban, "docs-update", &["backlog"]);

        let output = list_boards(project).unwrap();

        // Table header
        let lines: Vec<&str> = output.lines().collect();
        assert!(lines[0].contains("Phase"));
        assert!(lines[0].contains("Status"));
        assert!(lines[0].contains("Todo"));
        assert!(lines[0].contains("WIP"));
        assert!(lines[0].contains("Done"));

        // phase-1: 1 done, 0 todo, 0 in-progress
        assert!(lines[1].starts_with("phase-1"));
        assert!(lines[1].contains("Done"));

        // phase-2: 1 done, 1 todo (backlog), 0 in-progress
        assert!(lines[2].starts_with("phase-2"));
        assert!(lines[2].contains("In Progress"));

        // docs-update: 0 done, 1 todo, 0 in-progress
        assert!(lines[3].starts_with("docs-update"));
        assert!(lines[3].contains("Not Started"));

        // Summary line
        assert!(output.contains("3 boards"));
        assert!(output.contains("1 done"));
        assert!(output.contains("1 in progress"));
        assert!(output.contains("1 not started"));
        assert!(output.contains("2/4 tasks"));
    }

    #[test]
    fn list_boards_skips_dirs_without_phase_md() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path();
        let kanban = project.join(".batty").join("kanban");

        // Dir without PHASE.md — should be skipped
        std::fs::create_dir_all(kanban.join("no-phase")).unwrap();

        create_test_phase(&kanban, "phase-1", &[]);

        let output = list_boards(project).unwrap();
        assert!(output.contains("phase-1"));
        assert!(!output.contains("no-phase"));
    }

    #[test]
    fn list_boards_shows_repo_and_worktree_state() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path();
        let kanban = project.join(".batty").join("kanban");

        // Repo: phase-3 all backlog, phase-1 all done
        create_test_phase(&kanban, "phase-3", &["backlog", "backlog"]);
        create_test_phase(&kanban, "phase-1", &["done"]);

        // Worktree: phase-3 all done (diverged from repo)
        let wt_kanban = project
            .join(".batty")
            .join("worktrees")
            .join("phase-3-run-001")
            .join(".batty")
            .join("kanban");
        create_test_phase(&wt_kanban, "phase-3", &["done", "done"]);

        let output = list_boards(project).unwrap();
        let lines: Vec<&str> = output.lines().collect();

        // phase-1: repo only, no worktree line
        assert!(lines[1].starts_with("phase-1"));
        assert!(lines[1].contains("Done"));

        // phase-3 repo line: shows Not Started with 2 todo
        assert!(lines[2].starts_with("phase-3"));
        assert!(lines[2].contains("Not Started"));

        // phase-3 worktree line: indented, shows run-001 and Done
        let wt_line = lines[3];
        assert!(
            wt_line.contains("run-001"),
            "expected run-001, got: {wt_line}"
        );
        assert!(
            wt_line.contains("Done"),
            "expected worktree Done, got: {wt_line}"
        );

        // Summary uses effective (worktree) status: both boards "Done"
        assert!(output.contains("2 done"));
        assert!(output.contains("0 in progress"));
    }

    #[test]
    fn list_boards_shows_multiple_worktrees() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path();
        let kanban = project.join(".batty").join("kanban");

        create_test_phase(&kanban, "phase-2", &["backlog", "backlog"]);

        // Two worktrees for phase-2
        let wt1 = project
            .join(".batty")
            .join("worktrees")
            .join("phase-2-run-001")
            .join(".batty")
            .join("kanban");
        create_test_phase(&wt1, "phase-2", &["done", "done"]);

        let wt2 = project
            .join(".batty")
            .join("worktrees")
            .join("phase-2-run-002")
            .join(".batty")
            .join("kanban");
        create_test_phase(&wt2, "phase-2", &["done", "backlog"]);

        let output = list_boards(project).unwrap();

        // Both worktrees should appear
        assert!(output.contains("run-001"), "missing run-001 in:\n{output}");
        assert!(output.contains("run-002"), "missing run-002 in:\n{output}");

        // run-001 before run-002
        let pos1 = output.find("run-001").unwrap();
        let pos2 = output.find("run-002").unwrap();
        assert!(pos1 < pos2, "run-001 should appear before run-002");
    }
}
