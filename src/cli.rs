use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "batty",
    about = "Hierarchical agent team system for software development",
    version = concat!(env!("CARGO_PKG_VERSION"), "\nhttps://github.com/battysh/batty")
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Verbosity level (-v, -vv, -vvv)
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Scaffold .batty/team_config/ with default team.yaml and prompt templates
    #[command(alias = "install")]
    Init {
        /// Template to use for scaffolding
        #[arg(long, value_enum, conflicts_with = "from")]
        template: Option<InitTemplate>,
        /// Copy team config from $HOME/.batty/templates/<name>/
        #[arg(long, conflicts_with = "template")]
        from: Option<String>,
        /// Overwrite existing team config files
        #[arg(long)]
        force: bool,
        /// Default agent backend for all roles (claude, codex, kiro)
        #[arg(long)]
        agent: Option<String>,
    },

    /// Export the current team config as a reusable template
    ExportTemplate {
        /// Template name
        name: String,
    },

    /// Export run state for debugging
    ExportRun,

    /// Generate a run retrospective
    Retro {
        /// Path to events.jsonl (default: .batty/team_config/events.jsonl)
        #[arg(long)]
        events: Option<PathBuf>,
    },

    /// Start the team daemon and tmux session
    Start {
        /// Auto-attach to the tmux session after startup
        #[arg(long, default_value_t = false)]
        attach: bool,
    },

    /// Stop the team daemon and kill the tmux session
    Stop,

    /// Attach to the running team tmux session
    Attach,

    /// Show all team members and their states
    Status {
        /// Emit machine-readable JSON output
        #[arg(long, default_value_t = false)]
        json: bool,
    },

    /// Send a message to an agent role (human → agent injection)
    Send {
        /// Explicit sender override (hidden; used by pane bridge and automation)
        #[arg(long, hide = true)]
        from: Option<String>,
        /// Target role name (e.g., "architect", "manager-1")
        role: String,
        /// Message to inject
        message: String,
    },

    /// Assign a task to an engineer (used by manager agent)
    Assign {
        /// Target engineer instance (e.g., "eng-1-1")
        engineer: String,
        /// Task description
        task: String,
    },

    /// Validate team config without launching
    Validate {
        /// Show all individual checks with pass/fail status
        #[arg(long, default_value_t = false)]
        show_checks: bool,
    },

    /// Show resolved team configuration
    Config {
        /// Emit machine-readable JSON output
        #[arg(long, default_value_t = false)]
        json: bool,
    },

    /// Show the kanban board
    Board {
        #[command(subcommand)]
        command: Option<BoardCommand>,
    },

    /// List inbox messages for a team member, or purge delivered inbox messages
    #[command(args_conflicts_with_subcommands = true)]
    Inbox {
        #[command(subcommand)]
        command: Option<InboxCommand>,
        /// Member name (e.g., "architect", "manager-1", "eng-1-1")
        member: Option<String>,
        /// Maximum number of recent messages to show
        #[arg(
            short = 'n',
            long = "limit",
            default_value_t = 20,
            conflicts_with = "all"
        )]
        limit: usize,
        /// Show all messages
        #[arg(long, default_value_t = false)]
        all: bool,
    },

    /// Read a specific message from a member's inbox
    Read {
        /// Member name
        member: String,
        /// Message REF, ID, or ID prefix from `batty inbox` output
        id: String,
    },

    /// Acknowledge (mark delivered) a message in a member's inbox
    Ack {
        /// Member name
        member: String,
        /// Message REF, ID, or ID prefix from `batty inbox` output
        id: String,
    },

    /// Merge an engineer's worktree branch into main
    Merge {
        /// Engineer instance name (e.g., "eng-1-1")
        engineer: String,
    },

    /// Manage workflow task state and metadata
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },

    /// Record a structured review disposition for a task
    Review {
        /// Task id
        task_id: u32,
        /// Review disposition
        #[arg(value_enum)]
        disposition: ReviewAction,
        /// Feedback text
        feedback: Option<String>,
        /// Reviewer name (default: human)
        #[arg(long, default_value = "human")]
        reviewer: String,
    },

    /// Generate shell completions
    Completions {
        /// Shell to generate completion script for
        #[arg(value_enum)]
        shell: CompletionShell,
    },

    /// Per-intervention runtime toggles
    Nudge {
        #[command(subcommand)]
        command: NudgeCommand,
    },

    /// Pause nudges and standups
    Pause,

    /// Resume nudges and standups
    Resume,

    /// Manage Grafana monitoring (setup, status, open)
    Grafana {
        #[command(subcommand)]
        command: GrafanaCommand,
    },

    /// Set up Telegram bot for human communication
    Telegram,

    /// Estimate team load and show recent load history
    Load,

    /// Show pending dispatch queue entries
    Queue,

    /// Estimate current run cost from agent session files
    Cost,

    /// Dynamically scale team topology (add/remove agents)
    Scale {
        #[command(subcommand)]
        command: ScaleCommand,
    },

    /// Dump diagnostic state from Batty state files
    Doctor {
        /// Remove orphan branches and worktrees after confirmation
        #[arg(long, default_value_t = false)]
        fix: bool,
        /// Skip the cleanup confirmation prompt
        #[arg(long, default_value_t = false, requires = "fix")]
        yes: bool,
    },

    /// Show consolidated telemetry dashboard (tasks, cycle time, rates, agents)
    Metrics,

    /// Query the telemetry database for agent and task metrics
    Telemetry {
        #[command(subcommand)]
        command: TelemetryCommand,
    },

    /// Interactive chat with an agent via the shim protocol
    Chat {
        /// Agent type: claude, codex, kiro, generic
        #[arg(long, default_value = "generic")]
        agent_type: String,

        /// Shell command to launch the agent CLI (auto-detected from agent type if omitted)
        #[arg(long)]
        cmd: Option<String>,

        /// Working directory for the agent
        #[arg(long, default_value = ".")]
        cwd: String,

        /// Use SDK mode (NDJSON stdin/stdout) instead of PTY screen-scraping
        #[arg(long, default_value_t = false)]
        sdk_mode: bool,
    },

    /// Internal: run a shim process (spawned by `batty chat` or orchestrator)
    #[command(hide = true)]
    Shim {
        /// Unique agent identifier
        #[arg(long)]
        id: String,

        /// Agent type: claude, codex, kiro, generic
        #[arg(long)]
        agent_type: String,

        /// Shell command to launch the agent CLI
        #[arg(long)]
        cmd: String,

        /// Working directory for the agent
        #[arg(long)]
        cwd: String,

        /// Terminal rows
        #[arg(long, default_value = "50")]
        rows: u16,

        /// Terminal columns
        #[arg(long, default_value = "220")]
        cols: u16,

        /// Path to write raw PTY output for tmux display panes
        #[arg(long)]
        pty_log_path: Option<String>,

        /// Use SDK mode (NDJSON stdin/stdout) instead of PTY screen-scraping
        #[arg(long, default_value_t = false)]
        sdk_mode: bool,
    },

    /// Internal: interactive shim pane bridge for tmux
    #[command(hide = true)]
    ConsolePane {
        /// Project root directory
        #[arg(long)]
        project_root: String,

        /// Member/agent id
        #[arg(long)]
        member: String,

        /// Path to the shim event log
        #[arg(long)]
        events_log_path: String,

        /// Path to the shim PTY log
        #[arg(long)]
        pty_log_path: String,
    },

    /// Internal: run the daemon loop (spawned by `batty start`)
    #[command(hide = true)]
    Daemon {
        /// Project root directory
        #[arg(long)]
        project_root: String,
        /// Resume agent sessions from a previous run
        #[arg(long)]
        resume: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum TelemetryCommand {
    /// Show session summaries
    Summary,
    /// Show per-agent performance metrics
    Agents,
    /// Show per-task lifecycle metrics
    Tasks,
    /// Show review pipeline metrics (auto-merge rate, rework, latency)
    Reviews,
    /// Show recent events from the telemetry database
    Events {
        /// Maximum number of events to show
        #[arg(short = 'n', long = "limit", default_value_t = 50)]
        limit: usize,
    },
}

#[derive(Subcommand, Debug)]
pub enum GrafanaCommand {
    /// Install Grafana and the SQLite datasource plugin, then start the service
    Setup,
    /// Check whether the Grafana server is reachable
    Status,
    /// Open the Grafana dashboard in the default browser
    Open,
}

#[derive(Subcommand, Debug)]
pub enum InboxCommand {
    /// Purge delivered messages from inbox cur/ directories
    Purge {
        /// Role/member name to purge
        #[arg(required_unless_present = "all_roles")]
        role: Option<String>,
        /// Purge delivered messages for every inbox
        #[arg(long, default_value_t = false)]
        all_roles: bool,
        /// Purge delivered messages older than this unix timestamp
        #[arg(long, conflicts_with_all = ["all", "older_than"])]
        before: Option<u64>,
        /// Purge delivered messages older than this duration (e.g. 24h, 7d, 2w)
        #[arg(long, conflicts_with_all = ["all", "before"])]
        older_than: Option<String>,
        /// Purge all delivered messages
        #[arg(long, default_value_t = false, conflicts_with_all = ["before", "older_than"])]
        all: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum BoardCommand {
    /// List board tasks in a non-interactive table
    List {
        /// Filter tasks by status
        #[arg(long)]
        status: Option<String>,
    },
    /// Show per-status task counts
    Summary,
    /// Show dependency graph
    Deps {
        /// Output format: tree (default), flat, or dot
        #[arg(long, value_enum, default_value_t = DepsFormatArg::Tree)]
        format: DepsFormatArg,
    },
    /// Move done tasks to archive directory
    Archive {
        /// Only archive tasks older than this (e.g. "7d", "24h", "2w", or ISO date)
        #[arg(long, default_value = "0s")]
        older_than: String,

        /// Show what would be archived without moving files
        #[arg(long)]
        dry_run: bool,
    },
    /// Show board health dashboard
    Health,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DepsFormatArg {
    Tree,
    Flat,
    Dot,
}

#[derive(Subcommand, Debug)]
pub enum TaskCommand {
    /// Transition a task to a new workflow state
    Transition {
        /// Task id
        task_id: u32,
        /// Target state
        #[arg(value_enum)]
        target_state: TaskStateArg,
    },

    /// Assign execution and/or review ownership
    Assign {
        /// Task id
        task_id: u32,
        /// Execution owner
        #[arg(long = "execution-owner")]
        execution_owner: Option<String>,
        /// Review owner
        #[arg(long = "review-owner")]
        review_owner: Option<String>,
    },

    /// Record a review disposition for a task
    Review {
        /// Task id
        task_id: u32,
        /// Review disposition
        #[arg(long, value_enum)]
        disposition: ReviewDispositionArg,
        /// Feedback text (stored and delivered for changes_requested)
        #[arg(long)]
        feedback: Option<String>,
    },

    /// Update workflow metadata fields
    Update {
        /// Task id
        task_id: u32,
        /// Worktree branch
        #[arg(long)]
        branch: Option<String>,
        /// Commit sha
        #[arg(long)]
        commit: Option<String>,
        /// Blocking reason
        #[arg(long = "blocked-on")]
        blocked_on: Option<String>,
        /// Clear blocking fields
        #[arg(long = "clear-blocked", default_value_t = false)]
        clear_blocked: bool,
    },

    /// Set per-task auto-merge override
    #[command(name = "auto-merge")]
    AutoMerge {
        /// Task id
        task_id: u32,
        /// Enable or disable auto-merge for this task
        #[arg(value_enum)]
        action: AutoMergeAction,
    },

    /// Set scheduled_for and/or cron_schedule on a task
    Schedule {
        /// Task id
        task_id: u32,
        /// Scheduled datetime in RFC3339 format (e.g. 2026-03-25T09:00:00-04:00)
        #[arg(long = "at")]
        at: Option<String>,
        /// Cron expression (e.g. '0 9 * * *')
        #[arg(long = "cron")]
        cron: Option<String>,
        /// Clear both scheduled_for and cron_schedule
        #[arg(long, default_value_t = false)]
        clear: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum InitTemplate {
    /// Single agent, no hierarchy (1 pane)
    Solo,
    /// Architect + 1 engineer pair (2 panes)
    Pair,
    /// 1 architect + 1 manager + 3 engineers (5 panes)
    Simple,
    /// 1 architect + 1 manager + 5 engineers with layout (7 panes)
    Squad,
    /// Human + architect + 3 managers + 15 engineers with Telegram (19 panes)
    Large,
    /// PI + 3 sub-leads + 6 researchers — research lab style (10 panes)
    Research,
    /// Human + tech lead + 2 eng managers + 8 developers — full product team (11 panes)
    Software,
    /// Clean-room workflow: decompiler + spec-writer + test-writer + implementer (4 panes)
    Cleanroom,
    /// Batty self-development: human + architect + manager + 4 Rust engineers (6 panes)
    Batty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CompletionShell {
    Bash,
    Zsh,
    Fish,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TaskStateArg {
    Backlog,
    Todo,
    #[value(name = "in-progress")]
    InProgress,
    Review,
    Blocked,
    Done,
    Archived,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ReviewDispositionArg {
    Approved,
    #[value(name = "changes_requested")]
    ChangesRequested,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ReviewAction {
    Approve,
    #[value(name = "request-changes")]
    RequestChanges,
    Reject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AutoMergeAction {
    Enable,
    Disable,
}

#[derive(Subcommand, Debug)]
pub enum ScaleCommand {
    /// Set engineer instance count (scales up or down)
    Engineers {
        /// Target number of engineers per manager
        count: u32,
    },
    /// Add a new manager role
    AddManager {
        /// Name for the new manager role
        name: String,
    },
    /// Remove a manager role
    RemoveManager {
        /// Name of the manager role to remove
        name: String,
    },
    /// Show current topology (instance counts)
    Status,
}

#[derive(Subcommand, Debug)]
pub enum NudgeCommand {
    /// Disable an intervention at runtime
    Disable {
        /// Intervention name
        #[arg(value_enum)]
        name: NudgeIntervention,
    },
    /// Re-enable a disabled intervention
    Enable {
        /// Intervention name
        #[arg(value_enum)]
        name: NudgeIntervention,
    },
    /// Show status of all interventions
    Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum NudgeIntervention {
    Replenish,
    Triage,
    Review,
    Dispatch,
    Utilization,
    #[value(name = "owned-task")]
    OwnedTask,
}

impl NudgeIntervention {
    /// Return the marker file suffix for this intervention.
    #[allow(dead_code)]
    pub fn marker_name(self) -> &'static str {
        match self {
            Self::Replenish => "replenish",
            Self::Triage => "triage",
            Self::Review => "review",
            Self::Dispatch => "dispatch",
            Self::Utilization => "utilization",
            Self::OwnedTask => "owned-task",
        }
    }

    /// All known interventions.
    #[allow(dead_code)]
    pub const ALL: [NudgeIntervention; 6] = [
        Self::Replenish,
        Self::Triage,
        Self::Review,
        Self::Dispatch,
        Self::Utilization,
        Self::OwnedTask,
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn board_command_defaults_to_tui() {
        let cli = Cli::parse_from(["batty", "board"]);
        match cli.command {
            Command::Board { command } => assert!(command.is_none()),
            other => panic!("expected board command, got {other:?}"),
        }
    }

    #[test]
    fn board_list_subcommand_parses() {
        let cli = Cli::parse_from(["batty", "board", "list"]);
        match cli.command {
            Command::Board {
                command: Some(BoardCommand::List { status }),
            } => assert_eq!(status, None),
            other => panic!("expected board list command, got {other:?}"),
        }
    }

    #[test]
    fn board_list_subcommand_parses_status_filter() {
        let cli = Cli::parse_from(["batty", "board", "list", "--status", "review"]);
        match cli.command {
            Command::Board {
                command: Some(BoardCommand::List { status }),
            } => assert_eq!(status.as_deref(), Some("review")),
            other => panic!("expected board list command, got {other:?}"),
        }
    }

    #[test]
    fn board_summary_subcommand_parses() {
        let cli = Cli::parse_from(["batty", "board", "summary"]);
        match cli.command {
            Command::Board {
                command: Some(BoardCommand::Summary),
            } => {}
            other => panic!("expected board summary command, got {other:?}"),
        }
    }

    #[test]
    fn board_deps_subcommand_defaults_to_tree() {
        let cli = Cli::parse_from(["batty", "board", "deps"]);
        match cli.command {
            Command::Board {
                command: Some(BoardCommand::Deps { format }),
            } => assert_eq!(format, DepsFormatArg::Tree),
            other => panic!("expected board deps command, got {other:?}"),
        }
    }

    #[test]
    fn board_deps_subcommand_parses_format_flag() {
        for (arg, expected) in [
            ("tree", DepsFormatArg::Tree),
            ("flat", DepsFormatArg::Flat),
            ("dot", DepsFormatArg::Dot),
        ] {
            let cli = Cli::parse_from(["batty", "board", "deps", "--format", arg]);
            match cli.command {
                Command::Board {
                    command: Some(BoardCommand::Deps { format }),
                } => assert_eq!(format, expected, "format arg={arg}"),
                other => panic!("expected board deps command for {arg}, got {other:?}"),
            }
        }
    }

    #[test]
    fn board_archive_subcommand_parses() {
        let cli = Cli::parse_from(["batty", "board", "archive"]);
        match cli.command {
            Command::Board {
                command:
                    Some(BoardCommand::Archive {
                        older_than,
                        dry_run,
                    }),
            } => {
                assert_eq!(older_than, "0s");
                assert!(!dry_run);
            }
            other => panic!("expected board archive command, got {other:?}"),
        }
    }

    #[test]
    fn board_archive_subcommand_parses_older_than() {
        let cli = Cli::parse_from(["batty", "board", "archive", "--older-than", "7d"]);
        match cli.command {
            Command::Board {
                command:
                    Some(BoardCommand::Archive {
                        older_than,
                        dry_run,
                    }),
            } => {
                assert_eq!(older_than, "7d");
                assert!(!dry_run);
            }
            other => panic!("expected board archive command with older_than, got {other:?}"),
        }
    }

    #[test]
    fn board_archive_subcommand_parses_dry_run() {
        let cli = Cli::parse_from(["batty", "board", "archive", "--dry-run"]);
        match cli.command {
            Command::Board {
                command:
                    Some(BoardCommand::Archive {
                        older_than,
                        dry_run,
                    }),
            } => {
                assert_eq!(older_than, "0s");
                assert!(dry_run);
            }
            other => panic!("expected board archive command with dry_run, got {other:?}"),
        }
    }

    #[test]
    fn board_health_subcommand_parses() {
        let cli = Cli::parse_from(["batty", "board", "health"]);
        match cli.command {
            Command::Board {
                command: Some(BoardCommand::Health),
            } => {}
            other => panic!("expected board health command, got {other:?}"),
        }
    }

    #[test]
    fn init_subcommand_defaults_to_simple() {
        let cli = Cli::parse_from(["batty", "init"]);
        match cli.command {
            Command::Init {
                template,
                from,
                agent,
                ..
            } => {
                assert_eq!(template, None);
                assert_eq!(from, None);
                assert_eq!(agent, None);
            }
            other => panic!("expected init command, got {other:?}"),
        }
    }

    #[test]
    fn init_subcommand_accepts_large_template() {
        let cli = Cli::parse_from(["batty", "init", "--template", "large"]);
        match cli.command {
            Command::Init { template, from, .. } => {
                assert_eq!(template, Some(InitTemplate::Large));
                assert_eq!(from, None);
            }
            other => panic!("expected init command, got {other:?}"),
        }
    }

    #[test]
    fn init_subcommand_accepts_from_template_name() {
        let cli = Cli::parse_from(["batty", "init", "--from", "custom-team"]);
        match cli.command {
            Command::Init { template, from, .. } => {
                assert_eq!(template, None);
                assert_eq!(from.as_deref(), Some("custom-team"));
            }
            other => panic!("expected init command, got {other:?}"),
        }
    }

    #[test]
    fn init_subcommand_rejects_from_with_template() {
        let result = Cli::try_parse_from(["batty", "init", "--template", "large", "--from", "x"]);
        assert!(result.is_err());
    }

    #[test]
    fn init_agent_flag_parses() {
        let cli = Cli::parse_from(["batty", "init", "--agent", "codex"]);
        match cli.command {
            Command::Init { agent, .. } => {
                assert_eq!(agent.as_deref(), Some("codex"));
            }
            other => panic!("expected init command, got {other:?}"),
        }
    }

    #[test]
    fn install_alias_maps_to_init() {
        let cli = Cli::parse_from(["batty", "install"]);
        match cli.command {
            Command::Init {
                template,
                from,
                agent,
                ..
            } => {
                assert_eq!(template, None);
                assert_eq!(from, None);
                assert_eq!(agent, None);
            }
            other => panic!("expected init command via install alias, got {other:?}"),
        }
    }

    #[test]
    fn install_alias_with_agent_flag() {
        let cli = Cli::parse_from(["batty", "install", "--agent", "kiro"]);
        match cli.command {
            Command::Init { agent, .. } => {
                assert_eq!(agent.as_deref(), Some("kiro"));
            }
            other => panic!("expected init command via install alias, got {other:?}"),
        }
    }

    #[test]
    fn export_template_subcommand_parses() {
        let cli = Cli::parse_from(["batty", "export-template", "myteam"]);
        match cli.command {
            Command::ExportTemplate { name } => assert_eq!(name, "myteam"),
            other => panic!("expected export-template command, got {other:?}"),
        }
    }

    #[test]
    fn export_run_subcommand_parses() {
        let cli = Cli::parse_from(["batty", "export-run"]);
        match cli.command {
            Command::ExportRun => {}
            other => panic!("expected export-run command, got {other:?}"),
        }
    }

    #[test]
    fn retro_subcommand_parses() {
        let cli = Cli::parse_from(["batty", "retro"]);
        match cli.command {
            Command::Retro { events } => assert!(events.is_none()),
            other => panic!("expected retro command, got {other:?}"),
        }
    }

    #[test]
    fn retro_subcommand_parses_with_events_path() {
        let cli = Cli::parse_from(["batty", "retro", "--events", "/tmp/events.jsonl"]);
        match cli.command {
            Command::Retro { events } => {
                assert_eq!(events, Some(PathBuf::from("/tmp/events.jsonl")));
            }
            other => panic!("expected retro command, got {other:?}"),
        }
    }

    #[test]
    fn start_subcommand_defaults() {
        let cli = Cli::parse_from(["batty", "start"]);
        match cli.command {
            Command::Start { attach } => assert!(!attach),
            other => panic!("expected start command, got {other:?}"),
        }
    }

    #[test]
    fn start_subcommand_with_attach() {
        let cli = Cli::parse_from(["batty", "start", "--attach"]);
        match cli.command {
            Command::Start { attach } => assert!(attach),
            other => panic!("expected start command, got {other:?}"),
        }
    }

    #[test]
    fn stop_subcommand_parses() {
        let cli = Cli::parse_from(["batty", "stop"]);
        assert!(matches!(cli.command, Command::Stop));
    }

    #[test]
    fn attach_subcommand_parses() {
        let cli = Cli::parse_from(["batty", "attach"]);
        assert!(matches!(cli.command, Command::Attach));
    }

    #[test]
    fn status_subcommand_defaults() {
        let cli = Cli::parse_from(["batty", "status"]);
        match cli.command {
            Command::Status { json } => assert!(!json),
            other => panic!("expected status command, got {other:?}"),
        }
    }

    #[test]
    fn status_subcommand_json_flag() {
        let cli = Cli::parse_from(["batty", "status", "--json"]);
        match cli.command {
            Command::Status { json } => assert!(json),
            other => panic!("expected status command, got {other:?}"),
        }
    }

    #[test]
    fn send_subcommand_parses_role_and_message() {
        let cli = Cli::parse_from(["batty", "send", "architect", "hello world"]);
        match cli.command {
            Command::Send {
                from,
                role,
                message,
            } => {
                assert!(from.is_none());
                assert_eq!(role, "architect");
                assert_eq!(message, "hello world");
            }
            other => panic!("expected send command, got {other:?}"),
        }
    }

    #[test]
    fn assign_subcommand_parses_engineer_and_task() {
        let cli = Cli::parse_from(["batty", "assign", "eng-1-1", "fix auth bug"]);
        match cli.command {
            Command::Assign { engineer, task } => {
                assert_eq!(engineer, "eng-1-1");
                assert_eq!(task, "fix auth bug");
            }
            other => panic!("expected assign command, got {other:?}"),
        }
    }

    #[test]
    fn validate_subcommand_parses() {
        let cli = Cli::parse_from(["batty", "validate"]);
        match cli.command {
            Command::Validate { show_checks } => assert!(!show_checks),
            other => panic!("expected validate command, got {other:?}"),
        }
    }

    #[test]
    fn validate_subcommand_show_checks_flag() {
        let cli = Cli::parse_from(["batty", "validate", "--show-checks"]);
        match cli.command {
            Command::Validate { show_checks } => assert!(show_checks),
            other => panic!("expected validate command with show_checks, got {other:?}"),
        }
    }

    #[test]
    fn config_subcommand_json_flag() {
        let cli = Cli::parse_from(["batty", "config", "--json"]);
        match cli.command {
            Command::Config { json } => assert!(json),
            other => panic!("expected config command, got {other:?}"),
        }
    }

    #[test]
    fn merge_subcommand_parses_engineer() {
        let cli = Cli::parse_from(["batty", "merge", "eng-1-1"]);
        match cli.command {
            Command::Merge { engineer } => assert_eq!(engineer, "eng-1-1"),
            other => panic!("expected merge command, got {other:?}"),
        }
    }

    #[test]
    fn completions_subcommand_parses_shell() {
        let cli = Cli::parse_from(["batty", "completions", "zsh"]);
        match cli.command {
            Command::Completions { shell } => assert_eq!(shell, CompletionShell::Zsh),
            other => panic!("expected completions command, got {other:?}"),
        }
    }

    #[test]
    fn inbox_subcommand_parses_defaults() {
        let cli = Cli::parse_from(["batty", "inbox", "architect"]);
        match cli.command {
            Command::Inbox {
                command,
                member,
                limit,
                all,
            } => {
                assert!(command.is_none());
                assert_eq!(member.as_deref(), Some("architect"));
                assert_eq!(limit, 20);
                assert!(!all);
            }
            other => panic!("expected inbox command, got {other:?}"),
        }
    }

    #[test]
    fn inbox_subcommand_parses_limit_flag() {
        let cli = Cli::parse_from(["batty", "inbox", "architect", "-n", "50"]);
        match cli.command {
            Command::Inbox {
                command,
                member,
                limit,
                all,
            } => {
                assert!(command.is_none());
                assert_eq!(member.as_deref(), Some("architect"));
                assert_eq!(limit, 50);
                assert!(!all);
            }
            other => panic!("expected inbox command, got {other:?}"),
        }
    }

    #[test]
    fn inbox_subcommand_parses_all_flag() {
        let cli = Cli::parse_from(["batty", "inbox", "architect", "--all"]);
        match cli.command {
            Command::Inbox {
                command,
                member,
                limit,
                all,
            } => {
                assert!(command.is_none());
                assert_eq!(member.as_deref(), Some("architect"));
                assert_eq!(limit, 20);
                assert!(all);
            }
            other => panic!("expected inbox command, got {other:?}"),
        }
    }

    #[test]
    fn inbox_purge_subcommand_parses_role_and_before() {
        let cli = Cli::parse_from(["batty", "inbox", "purge", "architect", "--before", "123"]);
        match cli.command {
            Command::Inbox {
                command:
                    Some(InboxCommand::Purge {
                        role,
                        all_roles,
                        before,
                        older_than,
                        all,
                    }),
                member,
                ..
            } => {
                assert!(member.is_none());
                assert_eq!(role.as_deref(), Some("architect"));
                assert!(!all_roles);
                assert_eq!(before, Some(123));
                assert!(older_than.is_none());
                assert!(!all);
            }
            other => panic!("expected inbox purge command, got {other:?}"),
        }
    }

    #[test]
    fn inbox_purge_subcommand_parses_all_roles_and_all() {
        let cli = Cli::parse_from(["batty", "inbox", "purge", "--all-roles", "--all"]);
        match cli.command {
            Command::Inbox {
                command:
                    Some(InboxCommand::Purge {
                        role,
                        all_roles,
                        before,
                        older_than,
                        all,
                    }),
                member,
                ..
            } => {
                assert!(member.is_none());
                assert!(role.is_none());
                assert!(all_roles);
                assert_eq!(before, None);
                assert!(older_than.is_none());
                assert!(all);
            }
            other => panic!("expected inbox purge command, got {other:?}"),
        }
    }

    #[test]
    fn inbox_purge_subcommand_parses_older_than() {
        let cli = Cli::parse_from(["batty", "inbox", "purge", "eng-1", "--older-than", "24h"]);
        match cli.command {
            Command::Inbox {
                command:
                    Some(InboxCommand::Purge {
                        role,
                        all_roles,
                        before,
                        older_than,
                        all,
                    }),
                ..
            } => {
                assert_eq!(role.as_deref(), Some("eng-1"));
                assert!(!all_roles);
                assert_eq!(before, None);
                assert_eq!(older_than.as_deref(), Some("24h"));
                assert!(!all);
            }
            other => panic!("expected inbox purge command, got {other:?}"),
        }
    }

    #[test]
    fn inbox_purge_rejects_older_than_with_before() {
        let result = Cli::try_parse_from([
            "batty",
            "inbox",
            "purge",
            "eng-1",
            "--older-than",
            "24h",
            "--before",
            "100",
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn inbox_purge_rejects_older_than_with_all() {
        let result = Cli::try_parse_from([
            "batty",
            "inbox",
            "purge",
            "eng-1",
            "--older-than",
            "24h",
            "--all",
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn read_subcommand_parses_member_and_id() {
        let cli = Cli::parse_from(["batty", "read", "architect", "abc123"]);
        match cli.command {
            Command::Read { member, id } => {
                assert_eq!(member, "architect");
                assert_eq!(id, "abc123");
            }
            other => panic!("expected read command, got {other:?}"),
        }
    }

    #[test]
    fn ack_subcommand_parses_member_and_id() {
        let cli = Cli::parse_from(["batty", "ack", "eng-1-1", "abc123"]);
        match cli.command {
            Command::Ack { member, id } => {
                assert_eq!(member, "eng-1-1");
                assert_eq!(id, "abc123");
            }
            other => panic!("expected ack command, got {other:?}"),
        }
    }

    #[test]
    fn pause_subcommand_parses() {
        let cli = Cli::parse_from(["batty", "pause"]);
        assert!(matches!(cli.command, Command::Pause));
    }

    #[test]
    fn resume_subcommand_parses() {
        let cli = Cli::parse_from(["batty", "resume"]);
        assert!(matches!(cli.command, Command::Resume));
    }

    #[test]
    fn telegram_subcommand_parses() {
        let cli = Cli::parse_from(["batty", "telegram"]);
        assert!(matches!(cli.command, Command::Telegram));
    }

    #[test]
    fn doctor_subcommand_parses() {
        let cli = Cli::parse_from(["batty", "doctor"]);
        assert!(matches!(
            cli.command,
            Command::Doctor {
                fix: false,
                yes: false
            }
        ));
    }

    #[test]
    fn doctor_subcommand_parses_fix_flag() {
        let cli = Cli::parse_from(["batty", "doctor", "--fix"]);
        assert!(matches!(
            cli.command,
            Command::Doctor {
                fix: true,
                yes: false
            }
        ));
    }

    #[test]
    fn doctor_subcommand_parses_fix_yes_flags() {
        let cli = Cli::parse_from(["batty", "doctor", "--fix", "--yes"]);
        assert!(matches!(
            cli.command,
            Command::Doctor {
                fix: true,
                yes: true
            }
        ));
    }

    #[test]
    fn load_subcommand_parses() {
        let cli = Cli::parse_from(["batty", "load"]);
        assert!(matches!(cli.command, Command::Load));
    }

    #[test]
    fn queue_subcommand_parses() {
        let cli = Cli::parse_from(["batty", "queue"]);
        assert!(matches!(cli.command, Command::Queue));
    }

    #[test]
    fn cost_subcommand_parses() {
        let cli = Cli::parse_from(["batty", "cost"]);
        assert!(matches!(cli.command, Command::Cost));
    }

    #[test]
    fn verbose_flag_is_global() {
        let cli = Cli::parse_from(["batty", "-vv", "status"]);
        assert_eq!(cli.verbose, 2);
    }

    #[test]
    fn task_transition_subcommand_parses() {
        let cli = Cli::parse_from(["batty", "task", "transition", "24", "in-progress"]);
        match cli.command {
            Command::Task {
                command:
                    TaskCommand::Transition {
                        task_id,
                        target_state,
                    },
            } => {
                assert_eq!(task_id, 24);
                assert_eq!(target_state, TaskStateArg::InProgress);
            }
            other => panic!("expected task transition command, got {other:?}"),
        }
    }

    #[test]
    fn task_assign_subcommand_parses() {
        let cli = Cli::parse_from([
            "batty",
            "task",
            "assign",
            "24",
            "--execution-owner",
            "eng-1-2",
            "--review-owner",
            "manager-1",
        ]);
        match cli.command {
            Command::Task {
                command:
                    TaskCommand::Assign {
                        task_id,
                        execution_owner,
                        review_owner,
                    },
            } => {
                assert_eq!(task_id, 24);
                assert_eq!(execution_owner.as_deref(), Some("eng-1-2"));
                assert_eq!(review_owner.as_deref(), Some("manager-1"));
            }
            other => panic!("expected task assign command, got {other:?}"),
        }
    }

    #[test]
    fn task_review_subcommand_parses() {
        let cli = Cli::parse_from([
            "batty",
            "task",
            "review",
            "24",
            "--disposition",
            "changes_requested",
        ]);
        match cli.command {
            Command::Task {
                command:
                    TaskCommand::Review {
                        task_id,
                        disposition,
                        feedback,
                    },
            } => {
                assert_eq!(task_id, 24);
                assert_eq!(disposition, ReviewDispositionArg::ChangesRequested);
                assert!(feedback.is_none());
            }
            other => panic!("expected task review command, got {other:?}"),
        }
    }

    #[test]
    fn task_update_subcommand_parses() {
        let cli = Cli::parse_from([
            "batty",
            "task",
            "update",
            "24",
            "--branch",
            "eng-1-2/task-24",
            "--commit",
            "abc1234",
            "--blocked-on",
            "waiting for review",
            "--clear-blocked",
        ]);
        match cli.command {
            Command::Task {
                command:
                    TaskCommand::Update {
                        task_id,
                        branch,
                        commit,
                        blocked_on,
                        clear_blocked,
                    },
            } => {
                assert_eq!(task_id, 24);
                assert_eq!(branch.as_deref(), Some("eng-1-2/task-24"));
                assert_eq!(commit.as_deref(), Some("abc1234"));
                assert_eq!(blocked_on.as_deref(), Some("waiting for review"));
                assert!(clear_blocked);
            }
            other => panic!("expected task update command, got {other:?}"),
        }
    }

    #[test]
    fn nudge_disable_parses() {
        let cli = Cli::parse_from(["batty", "nudge", "disable", "triage"]);
        match cli.command {
            Command::Nudge {
                command: NudgeCommand::Disable { name },
            } => assert_eq!(name, NudgeIntervention::Triage),
            other => panic!("expected nudge disable, got {other:?}"),
        }
    }

    #[test]
    fn nudge_enable_parses() {
        let cli = Cli::parse_from(["batty", "nudge", "enable", "replenish"]);
        match cli.command {
            Command::Nudge {
                command: NudgeCommand::Enable { name },
            } => assert_eq!(name, NudgeIntervention::Replenish),
            other => panic!("expected nudge enable, got {other:?}"),
        }
    }

    #[test]
    fn nudge_status_parses() {
        let cli = Cli::parse_from(["batty", "nudge", "status"]);
        match cli.command {
            Command::Nudge {
                command: NudgeCommand::Status,
            } => {}
            other => panic!("expected nudge status, got {other:?}"),
        }
    }

    #[test]
    fn nudge_disable_owned_task_parses() {
        let cli = Cli::parse_from(["batty", "nudge", "disable", "owned-task"]);
        match cli.command {
            Command::Nudge {
                command: NudgeCommand::Disable { name },
            } => assert_eq!(name, NudgeIntervention::OwnedTask),
            other => panic!("expected nudge disable owned-task, got {other:?}"),
        }
    }

    #[test]
    fn nudge_rejects_unknown_intervention() {
        let result = Cli::try_parse_from(["batty", "nudge", "disable", "unknown"]);
        assert!(result.is_err());
    }

    #[test]
    fn nudge_intervention_marker_names() {
        assert_eq!(NudgeIntervention::Replenish.marker_name(), "replenish");
        assert_eq!(NudgeIntervention::Triage.marker_name(), "triage");
        assert_eq!(NudgeIntervention::Review.marker_name(), "review");
        assert_eq!(NudgeIntervention::Dispatch.marker_name(), "dispatch");
        assert_eq!(NudgeIntervention::Utilization.marker_name(), "utilization");
        assert_eq!(NudgeIntervention::OwnedTask.marker_name(), "owned-task");
    }

    #[test]
    fn parse_task_schedule_at() {
        let cli = Cli::parse_from([
            "batty",
            "task",
            "schedule",
            "50",
            "--at",
            "2026-03-25T09:00:00-04:00",
        ]);
        match cli.command {
            Command::Task {
                command:
                    TaskCommand::Schedule {
                        task_id,
                        at,
                        cron,
                        clear,
                    },
            } => {
                assert_eq!(task_id, 50);
                assert_eq!(at.as_deref(), Some("2026-03-25T09:00:00-04:00"));
                assert!(cron.is_none());
                assert!(!clear);
            }
            other => panic!("expected task schedule command, got {other:?}"),
        }
    }

    #[test]
    fn parse_task_schedule_cron() {
        let cli = Cli::parse_from(["batty", "task", "schedule", "51", "--cron", "0 9 * * *"]);
        match cli.command {
            Command::Task {
                command:
                    TaskCommand::Schedule {
                        task_id,
                        at,
                        cron,
                        clear,
                    },
            } => {
                assert_eq!(task_id, 51);
                assert!(at.is_none());
                assert_eq!(cron.as_deref(), Some("0 9 * * *"));
                assert!(!clear);
            }
            other => panic!("expected task schedule command, got {other:?}"),
        }
    }

    #[test]
    fn parse_task_schedule_clear() {
        let cli = Cli::parse_from(["batty", "task", "schedule", "52", "--clear"]);
        match cli.command {
            Command::Task {
                command:
                    TaskCommand::Schedule {
                        task_id,
                        at,
                        cron,
                        clear,
                    },
            } => {
                assert_eq!(task_id, 52);
                assert!(at.is_none());
                assert!(cron.is_none());
                assert!(clear);
            }
            other => panic!("expected task schedule command, got {other:?}"),
        }
    }

    #[test]
    fn parse_task_schedule_both() {
        let cli = Cli::parse_from([
            "batty",
            "task",
            "schedule",
            "53",
            "--at",
            "2026-04-01T00:00:00Z",
            "--cron",
            "0 9 * * 1",
        ]);
        match cli.command {
            Command::Task {
                command:
                    TaskCommand::Schedule {
                        task_id,
                        at,
                        cron,
                        clear,
                    },
            } => {
                assert_eq!(task_id, 53);
                assert_eq!(at.as_deref(), Some("2026-04-01T00:00:00Z"));
                assert_eq!(cron.as_deref(), Some("0 9 * * 1"));
                assert!(!clear);
            }
            other => panic!("expected task schedule command, got {other:?}"),
        }
    }

    #[test]
    fn review_approve_parses() {
        let cli = Cli::parse_from(["batty", "review", "42", "approve"]);
        match cli.command {
            Command::Review {
                task_id,
                disposition,
                feedback,
                reviewer,
            } => {
                assert_eq!(task_id, 42);
                assert_eq!(disposition, ReviewAction::Approve);
                assert!(feedback.is_none());
                assert_eq!(reviewer, "human");
            }
            other => panic!("expected review command, got {other:?}"),
        }
    }

    #[test]
    fn review_request_changes_with_feedback_parses() {
        let cli = Cli::parse_from([
            "batty",
            "review",
            "99",
            "request-changes",
            "fix the error handling",
        ]);
        match cli.command {
            Command::Review {
                task_id,
                disposition,
                feedback,
                reviewer,
            } => {
                assert_eq!(task_id, 99);
                assert_eq!(disposition, ReviewAction::RequestChanges);
                assert_eq!(feedback.as_deref(), Some("fix the error handling"));
                assert_eq!(reviewer, "human");
            }
            other => panic!("expected review command, got {other:?}"),
        }
    }

    #[test]
    fn review_reject_with_reviewer_flag_parses() {
        let cli = Cli::parse_from([
            "batty",
            "review",
            "7",
            "reject",
            "does not meet requirements",
            "--reviewer",
            "manager-1",
        ]);
        match cli.command {
            Command::Review {
                task_id,
                disposition,
                feedback,
                reviewer,
            } => {
                assert_eq!(task_id, 7);
                assert_eq!(disposition, ReviewAction::Reject);
                assert_eq!(feedback.as_deref(), Some("does not meet requirements"));
                assert_eq!(reviewer, "manager-1");
            }
            other => panic!("expected review command, got {other:?}"),
        }
    }

    #[test]
    fn review_rejects_invalid_disposition() {
        let result = Cli::try_parse_from(["batty", "review", "42", "maybe"]);
        assert!(result.is_err());
    }

    // --- send: missing required args ---

    #[test]
    fn send_rejects_missing_role() {
        let result = Cli::try_parse_from(["batty", "send"]);
        assert!(result.is_err());
    }

    #[test]
    fn send_rejects_missing_message() {
        let result = Cli::try_parse_from(["batty", "send", "architect"]);
        assert!(result.is_err());
    }

    // --- assign: missing required args ---

    #[test]
    fn assign_rejects_missing_engineer() {
        let result = Cli::try_parse_from(["batty", "assign"]);
        assert!(result.is_err());
    }

    #[test]
    fn assign_rejects_missing_task() {
        let result = Cli::try_parse_from(["batty", "assign", "eng-1-1"]);
        assert!(result.is_err());
    }

    // --- review: missing required args ---

    #[test]
    fn review_rejects_missing_task_id() {
        let result = Cli::try_parse_from(["batty", "review"]);
        assert!(result.is_err());
    }

    #[test]
    fn review_rejects_missing_disposition() {
        let result = Cli::try_parse_from(["batty", "review", "42"]);
        assert!(result.is_err());
    }

    // --- merge: missing required args ---

    #[test]
    fn merge_rejects_missing_engineer() {
        let result = Cli::try_parse_from(["batty", "merge"]);
        assert!(result.is_err());
    }

    // --- read/ack: missing required args ---

    #[test]
    fn read_rejects_missing_member() {
        let result = Cli::try_parse_from(["batty", "read"]);
        assert!(result.is_err());
    }

    #[test]
    fn read_rejects_missing_id() {
        let result = Cli::try_parse_from(["batty", "read", "architect"]);
        assert!(result.is_err());
    }

    #[test]
    fn ack_rejects_missing_args() {
        let result = Cli::try_parse_from(["batty", "ack"]);
        assert!(result.is_err());
    }

    // --- telemetry subcommands ---

    #[test]
    fn telemetry_summary_parses() {
        let cli = Cli::parse_from(["batty", "telemetry", "summary"]);
        match cli.command {
            Command::Telemetry {
                command: TelemetryCommand::Summary,
            } => {}
            other => panic!("expected telemetry summary, got {other:?}"),
        }
    }

    #[test]
    fn telemetry_agents_parses() {
        let cli = Cli::parse_from(["batty", "telemetry", "agents"]);
        match cli.command {
            Command::Telemetry {
                command: TelemetryCommand::Agents,
            } => {}
            other => panic!("expected telemetry agents, got {other:?}"),
        }
    }

    #[test]
    fn telemetry_tasks_parses() {
        let cli = Cli::parse_from(["batty", "telemetry", "tasks"]);
        match cli.command {
            Command::Telemetry {
                command: TelemetryCommand::Tasks,
            } => {}
            other => panic!("expected telemetry tasks, got {other:?}"),
        }
    }

    #[test]
    fn telemetry_reviews_parses() {
        let cli = Cli::parse_from(["batty", "telemetry", "reviews"]);
        match cli.command {
            Command::Telemetry {
                command: TelemetryCommand::Reviews,
            } => {}
            other => panic!("expected telemetry reviews, got {other:?}"),
        }
    }

    #[test]
    fn telemetry_events_default_limit() {
        let cli = Cli::parse_from(["batty", "telemetry", "events"]);
        match cli.command {
            Command::Telemetry {
                command: TelemetryCommand::Events { limit },
            } => assert_eq!(limit, 50),
            other => panic!("expected telemetry events, got {other:?}"),
        }
    }

    #[test]
    fn telemetry_events_custom_limit() {
        let cli = Cli::parse_from(["batty", "telemetry", "events", "-n", "10"]);
        match cli.command {
            Command::Telemetry {
                command: TelemetryCommand::Events { limit },
            } => assert_eq!(limit, 10),
            other => panic!("expected telemetry events with limit, got {other:?}"),
        }
    }

    #[test]
    fn telemetry_rejects_missing_subcommand() {
        let result = Cli::try_parse_from(["batty", "telemetry"]);
        assert!(result.is_err());
    }

    // --- grafana ---

    #[test]
    fn grafana_setup_parses() {
        let cli = Cli::parse_from(["batty", "grafana", "setup"]);
        assert!(matches!(
            cli.command,
            Command::Grafana {
                command: GrafanaCommand::Setup
            }
        ));
    }

    #[test]
    fn grafana_status_parses() {
        let cli = Cli::parse_from(["batty", "grafana", "status"]);
        assert!(matches!(
            cli.command,
            Command::Grafana {
                command: GrafanaCommand::Status
            }
        ));
    }

    #[test]
    fn grafana_open_parses() {
        let cli = Cli::parse_from(["batty", "grafana", "open"]);
        assert!(matches!(
            cli.command,
            Command::Grafana {
                command: GrafanaCommand::Open
            }
        ));
    }

    #[test]
    fn grafana_rejects_missing_subcommand() {
        let result = Cli::try_parse_from(["batty", "grafana"]);
        assert!(result.is_err());
    }

    // --- task auto-merge ---

    #[test]
    fn task_auto_merge_enable_parses() {
        let cli = Cli::parse_from(["batty", "task", "auto-merge", "30", "enable"]);
        match cli.command {
            Command::Task {
                command: TaskCommand::AutoMerge { task_id, action },
            } => {
                assert_eq!(task_id, 30);
                assert_eq!(action, AutoMergeAction::Enable);
            }
            other => panic!("expected task auto-merge enable, got {other:?}"),
        }
    }

    #[test]
    fn task_auto_merge_disable_parses() {
        let cli = Cli::parse_from(["batty", "task", "auto-merge", "31", "disable"]);
        match cli.command {
            Command::Task {
                command: TaskCommand::AutoMerge { task_id, action },
            } => {
                assert_eq!(task_id, 31);
                assert_eq!(action, AutoMergeAction::Disable);
            }
            other => panic!("expected task auto-merge disable, got {other:?}"),
        }
    }

    #[test]
    fn task_auto_merge_rejects_invalid_action() {
        let result = Cli::try_parse_from(["batty", "task", "auto-merge", "30", "toggle"]);
        assert!(result.is_err());
    }

    // --- task assign with partial owners ---

    #[test]
    fn task_assign_execution_owner_only() {
        let cli = Cli::parse_from([
            "batty",
            "task",
            "assign",
            "10",
            "--execution-owner",
            "eng-1-3",
        ]);
        match cli.command {
            Command::Task {
                command:
                    TaskCommand::Assign {
                        task_id,
                        execution_owner,
                        review_owner,
                    },
            } => {
                assert_eq!(task_id, 10);
                assert_eq!(execution_owner.as_deref(), Some("eng-1-3"));
                assert!(review_owner.is_none());
            }
            other => panic!("expected task assign command, got {other:?}"),
        }
    }

    // --- task rejects missing subcommand ---

    #[test]
    fn task_rejects_missing_subcommand() {
        let result = Cli::try_parse_from(["batty", "task"]);
        assert!(result.is_err());
    }

    // --- doctor: --yes requires --fix ---

    #[test]
    fn doctor_rejects_yes_without_fix() {
        let result = Cli::try_parse_from(["batty", "doctor", "--yes"]);
        assert!(result.is_err());
    }

    // --- daemon hidden subcommand ---

    #[test]
    fn daemon_subcommand_parses() {
        let cli = Cli::parse_from(["batty", "daemon", "--project-root", "/tmp/project"]);
        match cli.command {
            Command::Daemon {
                project_root,
                resume,
            } => {
                assert_eq!(project_root, "/tmp/project");
                assert!(!resume);
            }
            other => panic!("expected daemon command, got {other:?}"),
        }
    }

    #[test]
    fn daemon_subcommand_parses_resume_flag() {
        let cli = Cli::parse_from([
            "batty",
            "daemon",
            "--project-root",
            "/tmp/project",
            "--resume",
        ]);
        match cli.command {
            Command::Daemon {
                project_root,
                resume,
            } => {
                assert_eq!(project_root, "/tmp/project");
                assert!(resume);
            }
            other => panic!("expected daemon command with resume, got {other:?}"),
        }
    }

    // --- completions: all shell variants ---

    #[test]
    fn completions_all_shells_parse() {
        for (arg, expected) in [
            ("bash", CompletionShell::Bash),
            ("zsh", CompletionShell::Zsh),
            ("fish", CompletionShell::Fish),
        ] {
            let cli = Cli::parse_from(["batty", "completions", arg]);
            match cli.command {
                Command::Completions { shell } => assert_eq!(shell, expected, "shell arg={arg}"),
                other => panic!("expected completions command for {arg}, got {other:?}"),
            }
        }
    }

    #[test]
    fn completions_rejects_unknown_shell() {
        let result = Cli::try_parse_from(["batty", "completions", "powershell"]);
        assert!(result.is_err());
    }

    // --- init: all template variants ---

    #[test]
    fn init_all_template_variants() {
        for (arg, expected) in [
            ("solo", InitTemplate::Solo),
            ("pair", InitTemplate::Pair),
            ("simple", InitTemplate::Simple),
            ("squad", InitTemplate::Squad),
            ("large", InitTemplate::Large),
            ("research", InitTemplate::Research),
            ("software", InitTemplate::Software),
            ("cleanroom", InitTemplate::Cleanroom),
            ("batty", InitTemplate::Batty),
        ] {
            let cli = Cli::parse_from(["batty", "init", "--template", arg]);
            match cli.command {
                Command::Init { template, from, .. } => {
                    assert_eq!(template, Some(expected), "template arg={arg}");
                    assert!(from.is_none());
                }
                other => panic!("expected init command for template {arg}, got {other:?}"),
            }
        }
    }

    // --- task review with feedback ---

    #[test]
    fn task_review_with_feedback_parses() {
        let cli = Cli::parse_from([
            "batty",
            "task",
            "review",
            "15",
            "--disposition",
            "changes_requested",
            "--feedback",
            "please fix tests",
        ]);
        match cli.command {
            Command::Task {
                command:
                    TaskCommand::Review {
                        task_id,
                        disposition,
                        feedback,
                    },
            } => {
                assert_eq!(task_id, 15);
                assert_eq!(disposition, ReviewDispositionArg::ChangesRequested);
                assert_eq!(feedback.as_deref(), Some("please fix tests"));
            }
            other => panic!("expected task review command, got {other:?}"),
        }
    }

    // --- task transition: all states ---

    #[test]
    fn task_transition_all_states() {
        for (arg, expected) in [
            ("backlog", TaskStateArg::Backlog),
            ("todo", TaskStateArg::Todo),
            ("in-progress", TaskStateArg::InProgress),
            ("review", TaskStateArg::Review),
            ("blocked", TaskStateArg::Blocked),
            ("done", TaskStateArg::Done),
            ("archived", TaskStateArg::Archived),
        ] {
            let cli = Cli::parse_from(["batty", "task", "transition", "1", arg]);
            match cli.command {
                Command::Task {
                    command:
                        TaskCommand::Transition {
                            task_id,
                            target_state,
                        },
                } => {
                    assert_eq!(task_id, 1);
                    assert_eq!(target_state, expected, "state arg={arg}");
                }
                other => panic!("expected task transition for {arg}, got {other:?}"),
            }
        }
    }

    #[test]
    fn task_transition_rejects_invalid_state() {
        let result = Cli::try_parse_from(["batty", "task", "transition", "1", "cancelled"]);
        assert!(result.is_err());
    }

    // --- unknown subcommand ---

    #[test]
    fn rejects_unknown_subcommand() {
        let result = Cli::try_parse_from(["batty", "foobar"]);
        assert!(result.is_err());
    }

    // --- no args ---

    #[test]
    fn rejects_no_subcommand() {
        let result = Cli::try_parse_from(["batty"]);
        assert!(result.is_err());
    }

    // --- inbox purge requires role or all-roles ---

    #[test]
    fn inbox_purge_rejects_missing_role_and_all_roles() {
        let result = Cli::try_parse_from(["batty", "inbox", "purge", "--all"]);
        assert!(result.is_err());
    }

    // --- nudge: all intervention variants ---

    #[test]
    fn nudge_enable_all_interventions() {
        for (arg, expected) in [
            ("replenish", NudgeIntervention::Replenish),
            ("triage", NudgeIntervention::Triage),
            ("review", NudgeIntervention::Review),
            ("dispatch", NudgeIntervention::Dispatch),
            ("utilization", NudgeIntervention::Utilization),
            ("owned-task", NudgeIntervention::OwnedTask),
        ] {
            let cli = Cli::parse_from(["batty", "nudge", "enable", arg]);
            match cli.command {
                Command::Nudge {
                    command: NudgeCommand::Enable { name },
                } => assert_eq!(name, expected, "nudge enable arg={arg}"),
                other => panic!("expected nudge enable for {arg}, got {other:?}"),
            }
        }
    }

    // --- config: default (no --json) ---

    #[test]
    fn config_subcommand_defaults_no_json() {
        let cli = Cli::parse_from(["batty", "config"]);
        match cli.command {
            Command::Config { json } => assert!(!json),
            other => panic!("expected config command, got {other:?}"),
        }
    }

    // --- completion generation tests ---

    /// Helper: generate completion script for a shell into a String.
    fn generate_completions(shell: clap_complete::Shell) -> String {
        use clap::CommandFactory;
        let mut buf = Vec::new();
        clap_complete::generate(shell, &mut Cli::command(), "batty", &mut buf);
        String::from_utf8(buf).expect("completions should be valid UTF-8")
    }

    #[test]
    fn completions_bash_generates() {
        let output = generate_completions(clap_complete::Shell::Bash);
        assert!(!output.is_empty(), "bash completions should not be empty");
        assert!(
            output.contains("_batty"),
            "bash completions should define _batty function"
        );
    }

    #[test]
    fn completions_zsh_generates() {
        let output = generate_completions(clap_complete::Shell::Zsh);
        assert!(!output.is_empty(), "zsh completions should not be empty");
        assert!(
            output.contains("#compdef batty"),
            "zsh completions should start with #compdef"
        );
    }

    #[test]
    fn completions_fish_generates() {
        let output = generate_completions(clap_complete::Shell::Fish);
        assert!(!output.is_empty(), "fish completions should not be empty");
        assert!(
            output.contains("complete -c batty"),
            "fish completions should contain complete -c batty"
        );
    }

    #[test]
    fn completions_include_grafana_subcommands() {
        let output = generate_completions(clap_complete::Shell::Fish);
        // Top-level grafana command
        assert!(
            output.contains("grafana"),
            "completions should include grafana command"
        );
        // Grafana subcommands
        assert!(
            output.contains("setup"),
            "completions should include grafana setup"
        );
        assert!(
            output.contains("status"),
            "completions should include grafana status"
        );
        assert!(
            output.contains("open"),
            "completions should include grafana open"
        );
    }

    #[test]
    fn completions_include_all_recent_commands() {
        let output = generate_completions(clap_complete::Shell::Fish);
        let expected_commands = [
            "task",
            "metrics",
            "grafana",
            "telemetry",
            "nudge",
            "load",
            "queue",
            "cost",
            "doctor",
            "pause",
            "resume",
        ];
        for cmd in &expected_commands {
            assert!(
                output.contains(cmd),
                "completions should include '{cmd}' command"
            );
        }
    }
}
