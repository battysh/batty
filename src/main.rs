mod agent;
mod cli;
mod config;
mod detector;
mod dod;
mod events;
mod log;
mod orchestrator;
mod policy;
mod prompt;
mod supervisor;
mod task;
mod tier2;
mod tmux;
mod work;

use anyhow::Result;
use clap::Parser;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tracing::{info, warn};

use cli::{Cli, Command};
use config::ProjectConfig;

fn resolve_board_dir(project_root: &Path, phase: &str) -> Result<PathBuf> {
    let session = tmux::session_name(phase);
    if tmux::session_exists(&session) {
        let session_root = tmux::session_path(&session)?;
        let active_board = PathBuf::from(session_root).join("kanban").join(phase);
        if active_board.is_dir() {
            return Ok(active_board);
        }
        warn!(
            session = %session,
            board = %active_board.display(),
            "active tmux session found but board directory missing; falling back to repo board"
        );
    }

    let fallback = project_root.join("kanban").join(phase);
    if fallback.is_dir() {
        return Ok(fallback);
    }

    anyhow::bail!(
        "phase board not found for '{}': checked active run and fallback path {}",
        phase,
        fallback.display()
    );
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let filter = match cli.verbose {
        0 => "batty=info",
        1 => "batty=debug",
        _ => "batty=trace",
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    let cwd = std::env::current_dir()?;
    let (config, config_path) = ProjectConfig::load(&cwd)?;

    match config_path {
        Some(ref p) => info!("loaded config from {}", p.display()),
        None => info!("no .batty/config.toml found, using defaults"),
    }

    match cli.command {
        Command::Work {
            target,
            parallel,
            agent,
            policy,
            attach,
            new,
            foreground,
        } => {
            // Detached mode: spawn a background batty worker and return immediately.
            // The worker runs with --foreground to avoid recursive spawning.
            if !attach && !foreground {
                let tasks_dir = cwd.join("kanban").join(&target).join("tasks");
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

            work::run_phase(&target, &config, agent_name, policy_str, attach, new, &cwd)?;
        }
        Command::Attach { target } => {
            let session = tmux::session_name(&target);
            tmux::attach(&session)?;
        }
        Command::Config => {
            println!("Project config:");
            println!("  agent:       {}", config.defaults.agent);
            println!(
                "  policy:      {}",
                match config.defaults.policy {
                    config::Policy::Observe => "observe",
                    config::Policy::Suggest => "suggest",
                    config::Policy::Act => "act",
                }
            );
            println!(
                "  dod:         {}",
                config.defaults.dod.as_deref().unwrap_or("(none)")
            );
            println!("  max_retries: {}", config.defaults.max_retries);
            println!("Supervisor config:");
            println!("  enabled:     {}", config.supervisor.enabled);
            println!("  program:     {}", config.supervisor.program);
            println!("  args:        {:?}", config.supervisor.args);
            println!("  timeout_sec: {}", config.supervisor.timeout_secs);
            println!("  trace_io:    {}", config.supervisor.trace_io);
            println!("Detector config:");
            println!("  silence_sec: {}", config.detector.silence_timeout_secs);
            println!("  cooldown_ms: {}", config.detector.answer_cooldown_millis);
            println!(
                "  unknown_fallback: {}",
                config.detector.unknown_request_fallback
            );
            println!(
                "  idle_input_fallback: {}",
                config.detector.idle_input_fallback
            );
            if let Some(ref p) = config_path {
                println!("  source:      {}", p.display());
            } else {
                println!("  source:      (defaults — no .batty/config.toml found)");
            }
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
