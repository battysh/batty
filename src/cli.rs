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

        /// Force creation of a new phase worktree run
        #[arg(long, default_value_t = false)]
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

    /// Install Batty skill packs and steering docs for agents
    Install {
        /// Install target (default: both)
        #[arg(long, value_enum, default_value_t = InstallTarget::Both)]
        target: InstallTarget,

        /// Destination directory (default: current directory)
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
}
