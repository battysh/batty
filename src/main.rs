mod agent;
mod cli;
mod config;
mod events;
mod log;
mod paths;
mod prompt;
mod task;
mod team;
mod tmux;
mod worktree;

use anyhow::{Context, Result, bail};
use clap::Parser;
use std::collections::HashMap;
use std::path::PathBuf;
use tracing::debug;

use cli::{Cli, Command, ReviewDispositionArg, TaskCommand, TaskStateArg};

/// Resolve the project root directory.
///
/// If running inside a git worktree, resolves to the main repository root
/// so that all `.batty/` operations (inboxes, team config, kanban board,
/// daemon PID, events) use the shared project directory.
fn project_root() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // Use git to find the main repo root (handles worktrees)
    if let Ok(output) = std::process::Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(&cwd)
        .output()
    {
        if output.status.success() {
            let git_common = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let git_path = if std::path::Path::new(&git_common).is_absolute() {
                PathBuf::from(&git_common)
            } else {
                cwd.join(&git_common)
            };
            // .git dir's parent is the repo root
            if let Some(repo_root) = git_path.parent() {
                if let Ok(canonical) = repo_root.canonicalize() {
                    return canonical;
                }
            }
        }
    }

    cwd
}

fn setup_tracing(verbose: u8) {
    let filter = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter)),
        )
        .with_writer(std::io::stderr)
        .init();
}

fn task_state_arg_name(state: TaskStateArg) -> &'static str {
    match state {
        TaskStateArg::Backlog => "backlog",
        TaskStateArg::Todo => "todo",
        TaskStateArg::InProgress => "in-progress",
        TaskStateArg::Review => "review",
        TaskStateArg::Blocked => "blocked",
        TaskStateArg::Done => "done",
        TaskStateArg::Archived => "archived",
    }
}

fn review_disposition_arg_name(disposition: ReviewDispositionArg) -> &'static str {
    match disposition {
        ReviewDispositionArg::Approved => "approved",
        ReviewDispositionArg::ChangesRequested => "changes_requested",
        ReviewDispositionArg::Rejected => "rejected",
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    setup_tracing(cli.verbose);

    let root = project_root();
    debug!(root = %root.display(), "project root");

    match cli.command {
        Command::Init { template } => {
            let template_name = match template {
                cli::InitTemplate::Solo => "solo",
                cli::InitTemplate::Pair => "pair",
                cli::InitTemplate::Simple => "simple",
                cli::InitTemplate::Squad => "squad",
                cli::InitTemplate::Large => "large",
                cli::InitTemplate::Research => "research",
                cli::InitTemplate::Software => "software",
                cli::InitTemplate::Batty => "batty",
            };
            let created = team::init_team(&root, template_name)?;
            println!("Initialized team config ({} files):", created.len());
            for path in &created {
                println!("  {}", path.display());
            }
            println!();
            println!("Edit .batty/team_config/team.yaml to configure your team.");
            println!("Then run: batty start");
        }

        Command::Start { attach } => {
            let session = team::start_team(&root, attach)?;
            if !attach {
                println!("Team session started: {session}");
                println!("Run `batty attach` to connect.");
            }
        }

        Command::Stop => {
            team::stop_team(&root)?;
            println!("Team session stopped.");
        }

        Command::Attach => {
            team::attach_team(&root)?;
        }

        Command::Status { json } => {
            team::team_status(&root, json)?;
        }

        Command::Send { role, message } => {
            team::send_message(&root, &role, &message)?;
            println!("Message queued for {role}.");
        }

        Command::Assign { engineer, task } => {
            let id = team::assign_task(&root, &engineer, &task)?;
            println!(
                "Task queued for {engineer}. Inbox message id: {id}. Delivery result will be reported by Batty."
            );
            match team::wait_for_assignment_result(&root, &id, std::time::Duration::from_secs(8))? {
                Some(result) => eprintln!("{}", team::format_assignment_result(&result)),
                None => eprintln!(
                    "Assignment is still queued or pending delivery. No daemon result was available yet for {id}."
                ),
            }
        }

        Command::Validate => {
            team::validate_team(&root)?;
        }

        Command::Config { json } => {
            let config_path = team::team_config_path(&root);
            if !config_path.exists() {
                println!("No team config found. Run `batty init` first.");
                return Ok(());
            }

            let team_config = team::config::TeamConfig::load(&config_path)?;
            if json {
                // Serialize the config back to JSON for inspection
                let members = team::hierarchy::resolve_hierarchy(&team_config)?;
                let output = serde_json::json!({
                    "config_path": config_path.display().to_string(),
                    "team": team_config.name,
                    "roles": team_config.roles.len(),
                    "members": members.len(),
                    "board": {
                        "rotation_threshold": team_config.board.rotation_threshold,
                        "auto_dispatch": team_config.board.auto_dispatch,
                    },
                    "standup": {
                        "interval_secs": team_config.standup.interval_secs,
                        "output_lines": team_config.standup.output_lines,
                    },
                    "automation": {
                        "timeout_nudges": team_config.automation.timeout_nudges,
                        "standups": team_config.automation.standups,
                        "triage_interventions": team_config.automation.triage_interventions,
                        "review_interventions": team_config.automation.review_interventions,
                        "owned_task_interventions": team_config.automation.owned_task_interventions,
                        "manager_dispatch_interventions": team_config.automation.manager_dispatch_interventions,
                        "architect_utilization_interventions": team_config.automation.architect_utilization_interventions,
                    },
                    "workflow": {
                        "mode": team_config.workflow_mode.as_str(),
                        "orchestrator_pane": team_config.orchestrator_pane,
                    },
                });
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                println!("Config: {}", config_path.display());
                println!("Team: {}", team_config.name);
                println!("Roles: {}", team_config.roles.len());
                let members = team::hierarchy::resolve_hierarchy(&team_config)?;
                println!("Total members: {}", members.len());
                println!(
                    "Board rotation threshold: {}",
                    team_config.board.rotation_threshold
                );
                println!("Board auto-dispatch: {}", team_config.board.auto_dispatch);
                println!("Standup interval: {}s", team_config.standup.interval_secs);
                println!(
                    "Automation: timeout_nudges={}, standups={}, triage={}, review={}, owned_tasks={}, manager_dispatch={}, architect_utilization={}",
                    team_config.automation.timeout_nudges,
                    team_config.automation.standups,
                    team_config.automation.triage_interventions,
                    team_config.automation.review_interventions,
                    team_config.automation.owned_task_interventions,
                    team_config.automation.manager_dispatch_interventions,
                    team_config.automation.architect_utilization_interventions,
                );
                println!(
                    "Workflow: mode={}, orchestrator_pane={}",
                    team_config.workflow_mode.as_str(),
                    team_config.orchestrator_pane
                );
            }
        }

        Command::Board => {
            let board_dir = root.join(".batty").join("team_config").join("board");
            if board_dir.is_dir() {
                let status = std::process::Command::new("kanban-md")
                    .args(["tui", "--dir", &board_dir.to_string_lossy()])
                    .status()
                    .context("failed to run kanban-md — is it installed?")?;
                if !status.success() {
                    bail!("kanban-md tui failed");
                }
            } else {
                bail!(
                    "no board found at {}; run `batty init` first",
                    board_dir.display()
                );
            }
        }

        Command::Inbox { member, limit, all } => {
            let limit = if all { None } else { Some(limit) };
            team::list_inbox(&root, &member, limit)?;
        }

        Command::Read { member, id } => {
            team::read_message(&root, &member, &id)?;
        }

        Command::Ack { member, id } => {
            team::ack_message(&root, &member, &id)?;
            println!("Message {id} acknowledged for {member}.");
        }

        Command::Merge { engineer } => {
            team::merge_worktree(&root, &engineer)?;
        }

        Command::Task { command } => {
            let board_dir = team::team_config_dir(&root).join("board");
            match command {
                TaskCommand::Transition {
                    task_id,
                    target_state,
                } => team::task_cmd::cmd_transition(
                    &board_dir,
                    task_id,
                    task_state_arg_name(target_state),
                )?,
                TaskCommand::Assign {
                    task_id,
                    execution_owner,
                    review_owner,
                } => team::task_cmd::cmd_assign(
                    &board_dir,
                    task_id,
                    execution_owner.as_deref(),
                    review_owner.as_deref(),
                )?,
                TaskCommand::Review {
                    task_id,
                    disposition,
                } => team::task_cmd::cmd_review(
                    &board_dir,
                    task_id,
                    review_disposition_arg_name(disposition),
                )?,
                TaskCommand::Update {
                    task_id,
                    branch,
                    commit,
                    blocked_on,
                    clear_blocked,
                } => {
                    let mut fields = HashMap::new();
                    if let Some(branch) = branch {
                        fields.insert("branch".to_string(), branch);
                    }
                    if let Some(commit) = commit {
                        fields.insert("commit".to_string(), commit);
                    }
                    if let Some(blocked_on) = blocked_on {
                        fields.insert("blocked_on".to_string(), blocked_on);
                    }
                    if clear_blocked {
                        fields.insert("clear_blocked".to_string(), "true".to_string());
                    }
                    team::task_cmd::cmd_update(&board_dir, task_id, fields)?;
                }
            }
        }

        Command::Daemon {
            project_root,
            resume,
        } => {
            let root = std::path::PathBuf::from(project_root);
            team::run_daemon(&root, resume)?;
        }

        Command::Completions { shell } => {
            use clap::CommandFactory;
            let shell = match shell {
                cli::CompletionShell::Bash => clap_complete::Shell::Bash,
                cli::CompletionShell::Zsh => clap_complete::Shell::Zsh,
                cli::CompletionShell::Fish => clap_complete::Shell::Fish,
            };
            clap_complete::generate(shell, &mut Cli::command(), "batty", &mut std::io::stdout());
        }

        Command::Pause => {
            team::pause_team(&root)?;
            println!("Nudges and standups paused. Run `batty resume` to resume.");
        }

        Command::Resume => {
            team::resume_team(&root)?;
            println!("Nudges and standups resumed.");
        }

        Command::Load => {
            team::show_load(&root)?;
        }

        Command::Telegram => {
            team::setup_telegram(&root)?;
        }
    }

    Ok(())
}
