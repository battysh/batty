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
        #[arg(long, value_enum, default_value_t = InitTemplate::Simple)]
        template: InitTemplate,
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
    },

    /// Read a specific message from a member's inbox
    Read {
        /// Member name
        member: String,
        /// Message ID (or prefix) from `batty inbox` output
        id: String,
    },

    /// Acknowledge (mark delivered) a message in a member's inbox
    Ack {
        /// Member name
        member: String,
        /// Message ID (from `batty inbox` output)
        id: String,
    },

    /// Merge an engineer's worktree branch into main
    Merge {
        /// Engineer instance name (e.g., "eng-1-1")
        engineer: String,
    },

    /// Generate shell completions
    Completions {
        /// Shell to generate completion script for
        #[arg(value_enum)]
        shell: CompletionShell,
    },

    /// Set up Telegram bot for human communication
    Telegram,

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_subcommand_defaults_to_simple() {
        let cli = Cli::parse_from(["batty", "init"]);
        match cli.command {
            Command::Init { template } => assert_eq!(template, InitTemplate::Simple),
            other => panic!("expected init command, got {other:?}"),
        }
    }

    #[test]
    fn init_subcommand_accepts_large_template() {
        let cli = Cli::parse_from(["batty", "init", "--template", "large"]);
        match cli.command {
            Command::Init { template } => assert_eq!(template, InitTemplate::Large),
            other => panic!("expected init command, got {other:?}"),
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
    fn inbox_subcommand_parses_member() {
        let cli = Cli::parse_from(["batty", "inbox", "architect"]);
        match cli.command {
            Command::Inbox { member } => assert_eq!(member, "architect"),
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
    fn telegram_subcommand_parses() {
        let cli = Cli::parse_from(["batty", "telegram"]);
        assert!(matches!(cli.command, Command::Telegram));
    }

    #[test]
    fn verbose_flag_is_global() {
        let cli = Cli::parse_from(["batty", "-vv", "status"]);
        assert_eq!(cli.verbose, 2);
    }
}
