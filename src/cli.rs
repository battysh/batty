use clap::{Parser, Subcommand};

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
    },

    /// Attach to a running batty tmux session
    Attach {
        /// Phase name to attach to (e.g., "phase-1")
        target: String,
    },

    /// Show project configuration
    Config,
}
