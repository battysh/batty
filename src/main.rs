mod agent;
mod cli;
mod completion;
mod config;
mod detector;
mod dod;
mod events;
mod install;
mod log;
mod orchestrator;
mod paths;
mod policy;
mod prompt;
mod review;
mod sequencer;
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
use tracing::{info, warn};

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
    let digits: String = after_phase.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() {
        return digits.parse::<f64>().ok();
    }
    None
}

/// Derive phase status from task statuses.
///
/// - All tasks done → "Done"
/// - Any task in-progress → "In Progress"
/// - All tasks in backlog/todo (none done, none in-progress) → "Not Started"
/// - Mix of done and backlog (none in-progress) → "In Progress"
/// - No tasks → "Empty"
fn derive_phase_status(tasks: &[task::Task]) -> &'static str {
    if tasks.is_empty() {
        return "Empty";
    }

    let total = tasks.len();
    let done = tasks.iter().filter(|t| t.status == "done").count();
    let in_progress = tasks
        .iter()
        .filter(|t| t.status == "in-progress")
        .count();

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

/// Read the git branch name for a worktree directory, if available.
fn worktree_branch(worktree_dir: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["-C", &worktree_dir.to_string_lossy(), "branch", "--show-current"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() { None } else { Some(branch) }
}

/// Read the last commit summary (short hash + subject) for a worktree directory.
fn worktree_last_commit(worktree_dir: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args([
            "-C",
            &worktree_dir.to_string_lossy(),
            "log",
            "--oneline",
            "-1",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if line.is_empty() { None } else { Some(line) }
}

/// Resolve the worktree root directory for a phase (the parent of .batty/kanban/<phase>).
///
/// Given a board dir like `.batty/worktrees/phase-3-run-001/.batty/kanban/phase-3`,
/// walk up to find the worktree root (the dir containing `.batty/`).
fn worktree_root_from_board(board_dir: &Path) -> Option<PathBuf> {
    // board_dir = <worktree>/.batty/kanban/<phase> or <worktree>/kanban/<phase>
    let mut ancestor = board_dir.parent()?; // kanban dir
    ancestor = ancestor.parent()?; // .batty or worktree root
    if ancestor.file_name().is_some_and(|n| n == ".batty") {
        ancestor = ancestor.parent()?; // worktree root
    }
    Some(ancestor.to_path_buf())
}

struct TaskCounts {
    done: usize,
    total: usize,
}

impl TaskCounts {
    fn from_dir(tasks_dir: &Path) -> Self {
        if tasks_dir.is_dir() {
            match task::load_tasks_from_dir(tasks_dir) {
                Ok(tasks) => {
                    let done = tasks.iter().filter(|t| t.status == "done").count();
                    Self { done, total: tasks.len() }
                }
                Err(_) => Self { done: 0, total: 0 },
            }
        } else {
            Self { done: 0, total: 0 }
        }
    }
}

struct WorktreeInfo {
    branch: String,
    last_commit: Option<String>,
    counts: TaskCounts,
    status: String,
}

struct BoardInfo {
    name: String,
    /// Status derived from the repo (main branch) tasks.
    repo_status: String,
    repo_counts: TaskCounts,
    /// Present when a worktree with a board for this phase exists.
    worktree: Option<WorktreeInfo>,
}

impl BoardInfo {
    /// Effective status: worktree if present, otherwise repo.
    fn effective_status(&self) -> &str {
        self.worktree
            .as_ref()
            .map(|w| w.status.as_str())
            .unwrap_or(&self.repo_status)
    }

    /// Effective task counts: worktree if present, otherwise repo.
    fn effective_counts(&self) -> &TaskCounts {
        self.worktree
            .as_ref()
            .map(|w| &w.counts)
            .unwrap_or(&self.repo_counts)
    }
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
        let repo_counts = TaskCounts::from_dir(&path.join("tasks"));
        let repo_tasks = if path.join("tasks").is_dir() {
            task::load_tasks_from_dir(&path.join("tasks")).unwrap_or_default()
        } else {
            Vec::new()
        };
        let repo_status = derive_phase_status(&repo_tasks).to_string();

        // Worktree board (if any)
        let wt_info = match resolve_latest_worktree_board_dir(project_root, &name) {
            Ok(Some(wt_board)) => {
                let wt_counts = TaskCounts::from_dir(&wt_board.join("tasks"));
                let wt_tasks = if wt_board.join("tasks").is_dir() {
                    task::load_tasks_from_dir(&wt_board.join("tasks")).unwrap_or_default()
                } else {
                    Vec::new()
                };
                let wt_status = derive_phase_status(&wt_tasks).to_string();

                let wt_root = worktree_root_from_board(&wt_board);
                let branch = wt_root
                    .as_ref()
                    .and_then(|r| worktree_branch(r))
                    .unwrap_or_else(|| "???".to_string());
                let last_commit = wt_root.as_ref().and_then(|r| worktree_last_commit(r));

                Some(WorktreeInfo {
                    branch,
                    last_commit,
                    counts: wt_counts,
                    status: wt_status,
                })
            }
            _ => None,
        };

        boards.push(BoardInfo {
            name,
            repo_status,
            repo_counts,
            worktree: wt_info,
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

    let mut output = String::new();

    // Table header
    output.push_str(&format!(
        "{:<name_width$}  {:<status_width$}  Tasks\n",
        "Phase", "Status"
    ));

    for board in &boards {
        let tasks_str = format!("{}/{}", board.repo_counts.done, board.repo_counts.total);
        output.push_str(&format!(
            "{:<name_width$}  {:<status_width$}  {:>5}\n",
            board.name, board.repo_status, tasks_str
        ));

        // Worktree detail line, indented
        if let Some(ref wt) = board.worktree {
            let wt_tasks_str = format!("{}/{}", wt.counts.done, wt.counts.total);
            let merged = wt.status == board.repo_status
                && wt.counts.done == board.repo_counts.done
                && wt.counts.total == board.repo_counts.total;
            let merged_label = if merged { " (merged)" } else { "" };
            let commit_label = wt
                .last_commit
                .as_ref()
                .map(|c| format!("  {c}"))
                .unwrap_or_default();
            output.push_str(&format!(
                "  worktree:  {:<status_width$}  {:>5}  {}{}{}\n",
                wt.status, wt_tasks_str, wt.branch, merged_label, commit_label,
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
    let cli = Cli::parse();
    let is_config_command = matches!(&cli.command, Command::Config { .. });

    let filter = match cli.verbose {
        0 if is_config_command => "batty=warn",
        0 => "batty=info",
        1 => "batty=debug",
        _ => "batty=trace",
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    let cwd =
        std::env::current_dir().context("failed to get current directory (was it deleted?)")?;
    let (config, config_path) = ProjectConfig::load(&cwd)?;

    if !is_config_command || cli.verbose > 0 {
        match config_path {
            Some(ref p) => info!("loaded config from {}", p.display()),
            None => info!("no .batty/config.toml found, using defaults"),
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
        Command::BoardList => {
            let output = list_boards(&cwd)?;
            print!("{output}");
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
    fn worktree_root_from_board_extracts_root() {
        let board = PathBuf::from("/project/.batty/worktrees/run-001/.batty/kanban/phase-3");
        let root = worktree_root_from_board(&board).unwrap();
        assert_eq!(root, PathBuf::from("/project/.batty/worktrees/run-001"));
    }

    #[test]
    fn worktree_root_from_board_handles_legacy_kanban() {
        let board = PathBuf::from("/project/.batty/worktrees/run-001/kanban/phase-3");
        let root = worktree_root_from_board(&board).unwrap();
        assert_eq!(root, PathBuf::from("/project/.batty/worktrees/run-001"));
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

        // Table rows
        let lines: Vec<&str> = output.lines().collect();
        assert!(lines[0].contains("Phase"));
        assert!(lines[0].contains("Status"));

        assert!(lines[1].starts_with("phase-1"));
        assert!(lines[1].contains("Done"));
        assert!(lines[1].contains("1/1"));

        assert!(lines[2].starts_with("phase-2"));
        assert!(lines[2].contains("In Progress"));
        assert!(lines[2].contains("1/2"));

        assert!(lines[3].starts_with("docs-update"));
        assert!(lines[3].contains("Not Started"));
        assert!(lines[3].contains("0/1"));

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
        assert!(lines[1].contains("1/1"));

        // phase-3 repo line: shows Not Started 0/2
        assert!(lines[2].starts_with("phase-3"));
        assert!(lines[2].contains("Not Started"));
        assert!(lines[2].contains("0/2"));

        // phase-3 worktree line: indented, shows Done 2/2
        let wt_line = lines[3];
        assert!(wt_line.contains("Done"), "expected worktree Done, got: {wt_line}");
        assert!(wt_line.contains("2/2"), "expected worktree 2/2, got: {wt_line}");

        // Summary uses effective (worktree) status: both boards "Done"
        assert!(output.contains("2 done"));
        assert!(output.contains("0 in progress"));
    }

    #[test]
    fn list_boards_shows_merged_when_repo_matches_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path();
        let kanban = project.join(".batty").join("kanban");

        // Repo: phase-2 all done (already merged)
        create_test_phase(&kanban, "phase-2", &["done", "done"]);

        // Worktree: phase-2 also all done (same state)
        let wt_kanban = project
            .join(".batty")
            .join("worktrees")
            .join("phase-2-run-001")
            .join(".batty")
            .join("kanban");
        create_test_phase(&wt_kanban, "phase-2", &["done", "done"]);

        let output = list_boards(project).unwrap();

        // The worktree line should contain "(merged)"
        let wt_line = output
            .lines()
            .find(|l| l.contains("(merged)"))
            .expect("expected a (merged) line in output");
        assert!(wt_line.contains("Done"));
        assert!(wt_line.contains("2/2"));
    }
}
