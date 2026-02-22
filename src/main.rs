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

    let cwd = std::env::current_dir().context("failed to get current directory (was it deleted?)")?;
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
            if !attach && !foreground && !dry_run {
                let tasks_dir = paths::resolve_kanban_root(&cwd)
                    .join(&target)
                    .join("tasks");
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
                println!("  kanban-md skills: skipped (kanban-md not available or skills not present)");
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
}
