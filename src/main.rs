use anyhow::{Context, Result, bail};
use batty_cli::{
    agent,
    cli::{
        self, AutoMergeAction, BoardCommand, Cli, Command, DepsFormatArg, GrafanaCommand,
        InboxCommand, NudgeCommand, ReviewDispositionArg, TaskCommand, TaskStateArg,
    },
    team,
};
use clap::Parser;
use dialoguer::{Confirm, Input, Select};
use std::collections::HashMap;
use std::path::PathBuf;
use tracing::debug;

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

fn format_ts(unix_secs: i64) -> String {
    use std::time::{Duration, UNIX_EPOCH};
    let dt = UNIX_EPOCH + Duration::from_secs(unix_secs as u64);
    let datetime: chrono::DateTime<chrono::Local> = dt.into();
    datetime.format("%Y-%m-%d %H:%M:%S").to_string()
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

fn board_summary_counts(board_dir: &std::path::Path) -> Result<Vec<(&'static str, usize)>> {
    const STATUSES: [&str; 7] = [
        "backlog",
        "todo",
        "in-progress",
        "review",
        "blocked",
        "done",
        "archived",
    ];

    STATUSES
        .into_iter()
        .map(|status| {
            let output = team::board_cmd::list_tasks(board_dir, Some(status))?;
            Ok((status, count_board_list_rows(&output)))
        })
        .collect()
}

fn count_board_list_rows(output: &str) -> usize {
    output
        .lines()
        .filter(|line| {
            line.split_whitespace()
                .next()
                .is_some_and(|token| token.chars().all(|ch| ch.is_ascii_digit()))
        })
        .count()
}

fn confirm(prompt: &str, default: bool) -> Result<bool> {
    Ok(Confirm::new()
        .with_prompt(prompt)
        .default(default)
        .interact()?)
}

fn input_u64(prompt: &str, default: u64) -> Result<u64> {
    let s: String = Input::new()
        .with_prompt(prompt)
        .default(default.to_string())
        .interact_text()?;
    Ok(s.parse::<u64>().unwrap_or(default))
}

fn collect_init_overrides() -> Result<team::InitOverrides> {
    let mut ov = team::InitOverrides::default();

    println!();
    println!("── Orchestrator ──────────────────────────────────────────");
    println!("The orchestrator is a dedicated tmux pane that runs automated");
    println!("triage, review routing, dispatch-gap recovery, and standups.");
    ov.orchestrator_pane = Some(confirm(
        "Enable orchestrator pane?",
        true,
    )?);

    println!();
    println!("── Board & Dispatch ──────────────────────────────────────");
    println!("Auto-dispatch automatically assigns todo tasks from the board");
    println!("to idle engineers without manual intervention.");
    ov.auto_dispatch = Some(confirm(
        "Enable auto-dispatch of board tasks?",
        true,
    )?);

    println!();
    println!("── Engineer Worktrees ────────────────────────────────────");
    println!("When enabled, each engineer gets an isolated git worktree.");
    println!("New task assignments create a fresh branch in that worktree,");
    println!("keeping engineers from stepping on each other's changes.");
    ov.use_worktrees = Some(confirm(
        "Enable git worktrees for engineers?",
        true,
    )?);

    println!();
    println!("── Automation: Nudges & Standups ─────────────────────────");
    println!("Timeout nudges ping agents that appear stuck or idle for too long.");
    ov.timeout_nudges = Some(confirm(
        "Enable timeout nudges?",
        true,
    )?);

    println!("Standups periodically ask agents to report their status.");
    ov.standups = Some(confirm(
        "Enable periodic standups?",
        true,
    )?);

    println!();
    println!("── Automation: Interventions ──────────────────────────────");
    println!("These are daemon-driven actions that keep the team moving.");
    println!();

    println!("Triage: auto-routes new tasks to the right manager/engineer.");
    ov.triage_interventions = Some(confirm(
        "Enable triage interventions?",
        true,
    )?);

    println!("Review: nudges reviewers and escalates stale reviews.");
    ov.review_interventions = Some(confirm(
        "Enable review interventions?",
        true,
    )?);

    println!("Owned-task recovery: re-dispatches tasks stuck on a dead agent.");
    ov.owned_task_interventions = Some(confirm(
        "Enable owned-task recovery?",
        true,
    )?);

    println!("Manager dispatch: nudges managers when todo tasks pile up.");
    ov.manager_dispatch_interventions = Some(confirm(
        "Enable manager dispatch interventions?",
        true,
    )?);

    println!("Architect utilization: nudges the architect when engineers are idle.");
    ov.architect_utilization_interventions = Some(confirm(
        "Enable architect utilization interventions?",
        true,
    )?);

    println!();
    println!("── Auto-Merge ───────────────────────────────────────────");
    println!("When enabled, completed engineer branches that pass tests and");
    println!("score above a confidence threshold are merged automatically.");
    ov.auto_merge_enabled = Some(confirm(
        "Enable auto-merge?",
        false,
    )?);

    println!();
    println!("── Timing (seconds) ─────────────────────────────────────");
    println!("These control how often the daemon checks on agents and reviews.");
    println!();

    ov.standup_interval_secs = Some(input_u64(
        "Standup interval (secs, how often agents report status)",
        600,
    )?);

    ov.nudge_interval_secs = Some(input_u64(
        "Architect nudge interval (secs, idle ping for architect)",
        900,
    )?);

    ov.stall_threshold_secs = Some(input_u64(
        "Stall threshold (secs, agent considered stuck after this)",
        300,
    )?);

    ov.review_nudge_threshold_secs = Some(input_u64(
        "Review nudge threshold (secs, reviewer gets a reminder)",
        1800,
    )?);

    ov.review_timeout_secs = Some(input_u64(
        "Review timeout (secs, stale review gets escalated)",
        7200,
    )?);

    println!();

    Ok(ov)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    setup_tracing(cli.verbose);

    let root = project_root();
    debug!(root = %root.display(), "project root");

    match cli.command {
        Command::Init {
            template,
            from,
            force,
            agent,
        } => {
            // Validate agent name if provided
            if let Some(ref name) = agent {
                if agent::adapter_from_name(name).is_none() {
                    bail!(
                        "unknown agent backend '{name}'. Supported: {}",
                        agent::KNOWN_AGENT_NAMES.join(", ")
                    );
                }
            }
            let created = if let Some(template_name) = from.as_deref() {
                team::init_from_template(&root, template_name)?
            } else {
                let template_name = match template.unwrap_or(cli::InitTemplate::Simple) {
                    cli::InitTemplate::Solo => "solo",
                    cli::InitTemplate::Pair => "pair",
                    cli::InitTemplate::Simple => "simple",
                    cli::InitTemplate::Squad => "squad",
                    cli::InitTemplate::Large => "large",
                    cli::InitTemplate::Research => "research",
                    cli::InitTemplate::Software => "software",
                    cli::InitTemplate::Batty => "batty",
                };
                // Interactive prompts for project name and agent
                let default_name = root
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "my-project".to_string());
                let project_name: String = Input::new()
                    .with_prompt("Project name")
                    .default(default_name)
                    .interact_text()?;

                let selected_agent = if let Some(agent_name) = agent.as_deref() {
                    agent_name
                } else {
                    let agents = agent::KNOWN_AGENT_NAMES;
                    let default_idx = agents.iter().position(|&a| a == "claude").unwrap_or(0);
                    let agent_idx = Select::new()
                        .with_prompt("Agent backend")
                        .items(agents)
                        .default(default_idx)
                        .interact()?;
                    agents[agent_idx]
                };

                let overrides = collect_init_overrides()?;

                team::init_team_with_overrides(
                    &root,
                    template_name,
                    Some(&project_name),
                    Some(selected_agent),
                    force,
                    Some(&overrides),
                )?
            };
            println!("Initialized team config ({} files):", created.len());
            for path in &created {
                println!("  {}", path.display());
            }
            println!();
            println!("Edit .batty/team_config/team.yaml to configure your team.");
            println!("Then run: batty start");
        }

        Command::ExportTemplate { name } => {
            let count = team::export_template(&root, &name)?;
            println!("Exported template '{name}' ({count} files)");
        }

        Command::ExportRun => {
            let path = team::export_run(&root)?;
            println!("Run export written to {}", path.display());
        }

        Command::Retro { events } => {
            let events_path = events
                .unwrap_or_else(|| root.join(".batty").join("team_config").join("events.jsonl"));
            let stats = team::retrospective::analyze_event_log(&events_path)?;
            match stats {
                Some(stats) => {
                    let path = team::retrospective::generate_retrospective(&root, &stats)?;
                    println!("Retrospective written to {}", path.display());
                }
                None => println!("No run data found in event log."),
            }
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

        Command::Validate { show_checks } => {
            team::validate_team(&root, show_checks)?;
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
                        "failure_pattern_detection": team_config.automation.failure_pattern_detection,
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
                    "Automation: timeout_nudges={}, standups={}, failure_patterns={}, triage={}, review={}, owned_tasks={}, manager_dispatch={}, architect_utilization={}",
                    team_config.automation.timeout_nudges,
                    team_config.automation.standups,
                    team_config.automation.failure_pattern_detection,
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

        Command::Board { command } => {
            let board_dir = root.join(".batty").join("team_config").join("board");
            if !board_dir.is_dir() {
                bail!(
                    "no board found at {}; run `batty init` first",
                    board_dir.display()
                );
            }

            match command {
                Some(BoardCommand::List { status }) => {
                    print!(
                        "{}",
                        team::board_cmd::list_tasks(&board_dir, status.as_deref())?
                    );
                }
                Some(BoardCommand::Summary) => {
                    for (status, count) in board_summary_counts(&board_dir)? {
                        println!("{status:<11} {count}");
                    }
                }
                Some(BoardCommand::Deps { format }) => {
                    let fmt = match format {
                        DepsFormatArg::Tree => team::deps::DepsFormat::Tree,
                        DepsFormatArg::Flat => team::deps::DepsFormat::Flat,
                        DepsFormatArg::Dot => team::deps::DepsFormat::Dot,
                    };
                    print!("{}", team::deps::render_deps(&board_dir, fmt)?);
                }
                Some(BoardCommand::Archive {
                    older_than,
                    dry_run,
                }) => {
                    let max_age = team::board::parse_age_threshold(&older_than)?;
                    let tasks = team::board::done_tasks_older_than(&board_dir, max_age)?;
                    if dry_run {
                        println!("[dry-run] Would archive {} task(s):", tasks.len());
                    }
                    let summary = team::board::archive_tasks(&board_dir, &tasks, dry_run)?;
                    if !dry_run {
                        println!(
                            "Archived {} task(s) to {}",
                            summary.archived_count,
                            summary.archive_dir.display()
                        );
                    }
                }
                Some(BoardCommand::Health) => {
                    let events_path = root.join(".batty").join("team_config").join("events.jsonl");
                    let health = team::board_health::compute_health(&board_dir, &events_path)?;
                    print!("{}", team::board_health::format_health(&health));
                }
                None => {
                    let status = std::process::Command::new("kanban-md")
                        .args(["tui", "--dir", &board_dir.to_string_lossy()])
                        .status()
                        .context("failed to run kanban-md — is it installed?")?;
                    if !status.success() {
                        bail!("kanban-md tui failed");
                    }
                }
            }
        }

        Command::Inbox {
            command,
            member,
            limit,
            all,
        } => match command {
            Some(InboxCommand::Purge {
                role,
                all_roles,
                before,
                older_than,
                all,
            }) => {
                let before = match (before, older_than) {
                    (Some(ts), _) => Some(ts),
                    (_, Some(dur)) => {
                        let age = team::board::parse_age_threshold(&dur)?;
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_secs();
                        Some(now.saturating_sub(age.as_secs()))
                    }
                    _ => None,
                };
                let summary = team::purge_inbox(&root, role.as_deref(), all_roles, before, all)?;
                if all_roles {
                    println!(
                        "Purged {} delivered message(s) across {} inbox(es).",
                        summary.messages, summary.roles
                    );
                } else {
                    let role = role.unwrap_or_default();
                    println!(
                        "Purged {} delivered message(s) from {role}.",
                        summary.messages
                    );
                }
            }
            None => {
                let member =
                    member.context("member is required unless using `batty inbox purge`")?;
                let limit = if all { None } else { Some(limit) };
                team::list_inbox(&root, &member, limit)?;
            }
        },

        Command::Read { member, id } => {
            team::read_message(&root, &member, &id)?;
        }

        Command::Ack { member, id } => {
            team::ack_message(&root, &member, &id)?;
            println!("Message {id} acknowledged for {member}.");
        }

        Command::Review {
            task_id,
            disposition,
            feedback,
            reviewer,
        } => {
            let board_dir = team::team_config_dir(&root).join("board");
            let disposition_str = match disposition {
                cli::ReviewAction::Approve => "approve",
                cli::ReviewAction::RequestChanges => "request-changes",
                cli::ReviewAction::Reject => "reject",
            };
            team::task_cmd::cmd_review_structured(
                &board_dir,
                task_id,
                disposition_str,
                feedback.as_deref(),
                &reviewer,
            )?;
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
                    feedback,
                } => team::task_cmd::cmd_review(
                    &board_dir,
                    task_id,
                    review_disposition_arg_name(disposition),
                    feedback.as_deref(),
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
                TaskCommand::AutoMerge { task_id, action } => {
                    let enabled = match action {
                        AutoMergeAction::Enable => true,
                        AutoMergeAction::Disable => false,
                    };
                    team::task_cmd::cmd_auto_merge(task_id, enabled, &root)?;
                }
                TaskCommand::Schedule {
                    task_id,
                    at,
                    cron,
                    clear,
                } => team::task_cmd::cmd_schedule(
                    &board_dir,
                    task_id,
                    at.as_deref(),
                    cron.as_deref(),
                    clear,
                )?,
            }
        }

        Command::Metrics => {
            team::metrics_cmd::run(&root)?;
        }

        Command::Telemetry { command } => {
            let conn =
                team::telemetry_db::open(&root).context("failed to open telemetry database")?;
            match command {
                cli::TelemetryCommand::Summary => {
                    let rows = team::telemetry_db::query_session_summaries(&conn)?;
                    if rows.is_empty() {
                        println!("No session summaries recorded yet.");
                    } else {
                        println!(
                            "{:<24} {:<20} {:<20} {:>10} {:>8} {:>8}",
                            "SESSION", "STARTED", "ENDED", "COMPLETED", "MERGES", "EVENTS"
                        );
                        for row in &rows {
                            let started = format_ts(row.started_at);
                            let ended = row
                                .ended_at
                                .map(format_ts)
                                .unwrap_or_else(|| "running".to_string());
                            println!(
                                "{:<24} {:<20} {:<20} {:>10} {:>8} {:>8}",
                                row.session_id,
                                started,
                                ended,
                                row.tasks_completed,
                                row.total_merges,
                                row.total_events
                            );
                        }
                    }
                }
                cli::TelemetryCommand::Agents => {
                    let rows = team::telemetry_db::query_agent_metrics(&conn)?;
                    if rows.is_empty() {
                        println!("No agent metrics recorded yet.");
                    } else {
                        println!(
                            "{:<16} {:>11} {:>8} {:>8} {:>12} {:>8}",
                            "ROLE", "COMPLETIONS", "FAILURES", "RESTARTS", "CYCLE_SECS", "IDLE_PCT"
                        );
                        for row in &rows {
                            let total_polls = row.idle_polls + row.working_polls;
                            let idle_pct = if total_polls > 0 {
                                format!(
                                    "{:.0}%",
                                    row.idle_polls as f64 / total_polls as f64 * 100.0
                                )
                            } else {
                                "-".to_string()
                            };
                            println!(
                                "{:<16} {:>11} {:>8} {:>8} {:>12} {:>8}",
                                row.role,
                                row.completions,
                                row.failures,
                                row.restarts,
                                row.total_cycle_secs,
                                idle_pct
                            );
                        }
                    }
                }
                cli::TelemetryCommand::Tasks => {
                    let rows = team::telemetry_db::query_task_metrics(&conn)?;
                    if rows.is_empty() {
                        println!("No task metrics recorded yet.");
                    } else {
                        println!(
                            "{:<8} {:<20} {:<20} {:>7} {:>11} {:>10} {:>10}",
                            "TASK",
                            "STARTED",
                            "COMPLETED",
                            "RETRIES",
                            "ESCALATIONS",
                            "MERGE_SECS",
                            "CONFIDENCE"
                        );
                        for row in &rows {
                            let started = row
                                .started_at
                                .map(format_ts)
                                .unwrap_or_else(|| "-".to_string());
                            let completed = row
                                .completed_at
                                .map(format_ts)
                                .unwrap_or_else(|| "-".to_string());
                            let merge = row
                                .merge_time_secs
                                .map(|s| s.to_string())
                                .unwrap_or_else(|| "-".to_string());
                            let confidence = row
                                .confidence_score
                                .map(|c| format!("{:.2}", c))
                                .unwrap_or_else(|| "-".to_string());
                            println!(
                                "{:<8} {:<20} {:<20} {:>7} {:>11} {:>10} {:>10}",
                                row.task_id,
                                started,
                                completed,
                                row.retries,
                                row.escalations,
                                merge,
                                confidence
                            );
                        }
                    }
                }
                cli::TelemetryCommand::Reviews => {
                    let row = team::telemetry_db::query_review_metrics(&conn)?;
                    let total_merges = row.auto_merge_count + row.manual_merge_count;
                    let auto_rate = if total_merges > 0 {
                        format!(
                            "{:.0}%",
                            row.auto_merge_count as f64 / total_merges as f64 * 100.0
                        )
                    } else {
                        "-".to_string()
                    };
                    let total_reviewed = total_merges + row.rework_count;
                    let rework_rate = if total_reviewed > 0 {
                        format!(
                            "{:.0}%",
                            row.rework_count as f64 / total_reviewed as f64 * 100.0
                        )
                    } else {
                        "-".to_string()
                    };
                    let avg_latency = row
                        .avg_review_latency_secs
                        .map(|s| format!("{:.0}s", s))
                        .unwrap_or_else(|| "-".to_string());
                    println!("Review Pipeline (all sessions)");
                    println!(
                        "Auto-merge Rate: {} | Rework Rate: {}",
                        auto_rate, rework_rate
                    );
                    println!(
                        "Auto: {} | Manual: {} | Rework: {} | Nudges: {} | Escalations: {}",
                        row.auto_merge_count,
                        row.manual_merge_count,
                        row.rework_count,
                        row.review_nudge_count,
                        row.review_escalation_count
                    );
                    println!("Avg Review Latency: {}", avg_latency);
                }
                cli::TelemetryCommand::Events { limit } => {
                    let rows = team::telemetry_db::query_recent_events(&conn, limit)?;
                    if rows.is_empty() {
                        println!("No events recorded yet.");
                    } else {
                        println!(
                            "{:<20} {:<24} {:<16} {:<8}",
                            "TIMESTAMP", "EVENT", "ROLE", "TASK"
                        );
                        for row in &rows {
                            let ts = format_ts(row.timestamp);
                            let role = row.role.as_deref().unwrap_or("-");
                            let task = row.task_id.as_deref().unwrap_or("-");
                            println!("{:<20} {:<24} {:<16} {:<8}", ts, row.event_type, role, task);
                        }
                    }
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

        Command::Nudge { command } => match command {
            NudgeCommand::Disable { name } => {
                team::disable_nudge(&root, name.marker_name())?;
                println!("Intervention '{}' disabled.", name.marker_name());
            }
            NudgeCommand::Enable { name } => {
                team::enable_nudge(&root, name.marker_name())?;
                println!("Intervention '{}' re-enabled.", name.marker_name());
            }
            NudgeCommand::Status => {
                team::nudge_status(&root)?;
            }
        },

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

        Command::Queue => {
            let entries = team::daemon::load_dispatch_queue_snapshot(&root);
            if entries.is_empty() {
                println!("Dispatch queue is empty.");
            } else {
                println!(
                    "{:<20} {:<8} {:<36} {:>8}  LAST FAILURE",
                    "ENGINEER", "TASK", "TITLE", "FAILURES"
                );
                println!("{}", "-".repeat(100));
                for entry in entries {
                    println!(
                        "{:<20} {:<8} {:<36} {:>8}  {}",
                        entry.engineer,
                        entry.task_id,
                        entry.task_title,
                        entry.validation_failures,
                        entry.last_failure.unwrap_or_else(|| "-".to_string())
                    );
                }
            }
        }

        Command::Cost => {
            team::cost::show_cost(&root)?;
        }

        Command::Doctor { fix, yes } => {
            print!("{}", team::doctor::run(&root, fix, yes)?);
        }

        Command::Grafana { command } => {
            let config_path = team::team_config_path(&root);
            let port = if config_path.exists() {
                team::config::TeamConfig::load(&config_path)?.grafana.port
            } else {
                team::grafana::DEFAULT_PORT
            };
            match command {
                GrafanaCommand::Setup => team::grafana::setup(port)?,
                GrafanaCommand::Status => team::grafana::status(port)?,
                GrafanaCommand::Open => team::grafana::open(port)?,
            }
        }

        Command::Telegram => {
            team::setup_telegram(&root)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn board_list_counts_data_rows_only() {
        let output = "\
ID    STATUS        PRIORITY   TITLE    CLAIMED    TAGS    DUE
1     todo          high       One      --         --      --
12    review        medium     Two      @eng-1     dx      --
";

        assert_eq!(count_board_list_rows(output), 2);
    }

    #[test]
    fn board_list_ignores_headers_and_empty_output() {
        let output = "\
ID    STATUS        PRIORITY   TITLE    CLAIMED    TAGS    DUE
";

        assert_eq!(count_board_list_rows(output), 0);
        assert_eq!(count_board_list_rows(""), 0);
    }
}
