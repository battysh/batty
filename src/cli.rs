use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "batty",
    about = "Hierarchical agent command system for software development",
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
    /// Execute a task or work through the board
    Work {
        /// Task ID or "all" to work through the board
        target: String,

        /// Number of parallel agents
        #[arg(long, default_value = "1")]
        parallel: u32,

        /// Override the default agent
        #[arg(long)]
        agent: Option<String>,

        /// Override the default policy
        #[arg(long)]
        policy: Option<String>,

        /// Auto-attach to the tmux session after startup
        #[arg(long, default_value_t = false)]
        attach: bool,

        /// Run in an isolated phase worktree
        #[arg(long, default_value_t = false)]
        worktree: bool,

        /// Force creation of a new phase worktree run (requires --worktree)
        #[arg(long, default_value_t = false, requires = "worktree")]
        new: bool,

        /// Show composed launch context and exit without running the executor
        #[arg(long, default_value_t = false)]
        dry_run: bool,

        /// Internal: keep work process in foreground (skip auto-backgrounding).
        #[arg(long, hide = true, default_value_t = false)]
        foreground: bool,
    },

    /// Attach to a running batty tmux session
    Attach {
        /// Phase name to attach to (e.g., "phase-1")
        target: String,
    },

    /// Resume supervision for an existing phase/session run
    Resume {
        /// Phase name (e.g., "phase-2.5") or tmux session name (e.g., "batty-phase-2-5")
        target: String,
    },

    /// Show project configuration
    Config {
        /// Emit machine-readable JSON output
        #[arg(long, default_value_t = false)]
        json: bool,
    },

    /// Initialize Batty assets and required external tools
    Install {
        /// Steering/skill install target (default: both)
        #[arg(long, value_enum, default_value_t = InstallTarget::Both)]
        target: InstallTarget,

        /// Destination directory (default: current directory)
        #[arg(long, default_value = ".")]
        dir: String,
    },

    /// Remove installed Batty assets from a project
    Remove {
        /// Steering/skill removal target (default: both)
        #[arg(long, value_enum, default_value_t = InstallTarget::Both)]
        target: InstallTarget,

        /// Target directory (default: current directory)
        #[arg(long, default_value = ".")]
        dir: String,
    },

    /// Open kanban-md TUI for a phase (prefers active run worktree)
    Board {
        /// Phase name (e.g., "phase-2.5")
        target: String,

        /// Print resolved board directory and exit
        #[arg(long, default_value_t = false)]
        print_dir: bool,
    },

    /// List all phase boards with status and task counts
    BoardList,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum InstallTarget {
    Both,
    Claude,
    Codex,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_subcommand_parses_target() {
        let cli = Cli::parse_from(["batty", "resume", "phase-2.5"]);
        match cli.command {
            Command::Resume { target } => assert_eq!(target, "phase-2.5"),
            other => panic!("expected resume command, got {other:?}"),
        }
    }

    #[test]
    fn install_subcommand_parses_defaults() {
        let cli = Cli::parse_from(["batty", "install"]);
        match cli.command {
            Command::Install { target, dir } => {
                assert_eq!(target, InstallTarget::Both);
                assert_eq!(dir, ".");
            }
            other => panic!("expected install command, got {other:?}"),
        }
    }

    #[test]
    fn install_subcommand_parses_target_and_dir() {
        let cli = Cli::parse_from(["batty", "install", "--target", "codex", "--dir", "/tmp/x"]);
        match cli.command {
            Command::Install { target, dir } => {
                assert_eq!(target, InstallTarget::Codex);
                assert_eq!(dir, "/tmp/x");
            }
            other => panic!("expected install command, got {other:?}"),
        }
    }

    #[test]
    fn config_subcommand_parses_json_flag() {
        let cli = Cli::parse_from(["batty", "config", "--json"]);
        match cli.command {
            Command::Config { json } => assert!(json),
            other => panic!("expected config command, got {other:?}"),
        }
    }

    #[test]
    fn work_subcommand_requires_worktree_for_new() {
        let err = Cli::try_parse_from(["batty", "work", "phase-2.5", "--new"]).unwrap_err();
        assert!(err.to_string().contains("--worktree"));
    }

    #[test]
    fn remove_subcommand_parses_defaults() {
        let cli = Cli::parse_from(["batty", "remove"]);
        match cli.command {
            Command::Remove { target, dir } => {
                assert_eq!(target, InstallTarget::Both);
                assert_eq!(dir, ".");
            }
            other => panic!("expected remove command, got {other:?}"),
        }
    }

    #[test]
    fn remove_subcommand_parses_target_and_dir() {
        let cli = Cli::parse_from(["batty", "remove", "--target", "claude", "--dir", "/tmp/x"]);
        match cli.command {
            Command::Remove { target, dir } => {
                assert_eq!(target, InstallTarget::Claude);
                assert_eq!(dir, "/tmp/x");
            }
            other => panic!("expected remove command, got {other:?}"),
        }
    }

    #[test]
    fn work_subcommand_parses_worktree_and_new() {
        let cli = Cli::parse_from(["batty", "work", "phase-2.5", "--worktree", "--new"]);
        match cli.command {
            Command::Work { worktree, new, .. } => {
                assert!(worktree);
                assert!(new);
            }
            other => panic!("expected work command, got {other:?}"),
        }
    }

    #[test]
    fn board_subcommand_parses_target() {
        let cli = Cli::parse_from(["batty", "board", "phase-2.5"]);
        match cli.command {
            Command::Board { target, print_dir } => {
                assert_eq!(target, "phase-2.5");
                assert!(!print_dir);
            }
            other => panic!("expected board command, got {other:?}"),
        }
    }

    #[test]
    fn board_list_subcommand_parses() {
        let cli = Cli::parse_from(["batty", "board-list"]);
        assert!(matches!(cli.command, Command::BoardList));
    }
}
