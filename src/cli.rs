use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "batty",
    about = "Hierarchical agent team system for software development",
    version
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
    Init {
        /// Template to use for scaffolding
        #[arg(long, value_enum, conflicts_with = "from")]
        template: Option<InitTemplate>,
        /// Copy team config from $HOME/.batty/templates/<name>/
        #[arg(long, conflicts_with = "template")]
        from: Option<String>,
    },

    /// Export the current team config as a reusable template
    ExportTemplate {
        /// Template name
        name: String,
    },

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
    Validate,

    /// Show resolved team configuration
    Config {
        /// Emit machine-readable JSON output
        #[arg(long, default_value_t = false)]
        json: bool,
    },

    /// Show the kanban board
    Board,

    /// List inbox messages for a team member
    Inbox {
        /// Member name (e.g., "architect", "manager-1", "eng-1-1")
        member: String,
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

    /// Generate shell completions
    Completions {
        /// Shell to generate completion script for
        #[arg(value_enum)]
        shell: CompletionShell,
    },

    /// Pause nudges and standups
    Pause,

    /// Resume nudges and standups
    Resume,

    /// Set up Telegram bot for human communication
    Telegram,

    /// Estimate team load and show recent load history
    Load,

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_subcommand_defaults_to_simple() {
        let cli = Cli::parse_from(["batty", "init"]);
        match cli.command {
            Command::Init { template, from } => {
                assert_eq!(template, None);
                assert_eq!(from, None);
            }
            other => panic!("expected init command, got {other:?}"),
        }
    }

    #[test]
    fn init_subcommand_accepts_large_template() {
        let cli = Cli::parse_from(["batty", "init", "--template", "large"]);
        match cli.command {
            Command::Init { template, from } => {
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
            Command::Init { template, from } => {
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
    fn export_template_subcommand_parses() {
        let cli = Cli::parse_from(["batty", "export-template", "myteam"]);
        match cli.command {
            Command::ExportTemplate { name } => assert_eq!(name, "myteam"),
            other => panic!("expected export-template command, got {other:?}"),
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
            Command::Send { role, message } => {
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
        assert!(matches!(cli.command, Command::Validate));
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
            Command::Inbox { member, limit, all } => {
                assert_eq!(member, "architect");
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
            Command::Inbox { member, limit, all } => {
                assert_eq!(member, "architect");
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
            Command::Inbox { member, limit, all } => {
                assert_eq!(member, "architect");
                assert_eq!(limit, 20);
                assert!(all);
            }
            other => panic!("expected inbox command, got {other:?}"),
        }
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
    fn load_subcommand_parses() {
        let cli = Cli::parse_from(["batty", "load"]);
        assert!(matches!(cli.command, Command::Load));
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
                    },
            } => {
                assert_eq!(task_id, 24);
                assert_eq!(disposition, ReviewDispositionArg::ChangesRequested);
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
}
