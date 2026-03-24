//! Agent Shim POC — two modes:
//!
//! 1. `agent-shim chat` — interactive chat frontend (spawns a shim subprocess)
//! 2. `agent-shim shim` — the shim process itself (called by chat or orchestrator)

mod chat;
mod classifier;
mod protocol;
mod shim;

use std::os::unix::io::FromRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use classifier::AgentType;

#[derive(Parser)]
#[command(name = "agent-shim", about = "POC: Agent shim process")]
struct Cli {
    #[command(subcommand)]
    command: Sub,
}

#[derive(Subcommand)]
enum Sub {
    /// Interactive chat with an agent (spawns a shim subprocess).
    Chat {
        /// Agent type: claude, codex, kiro, generic
        #[arg(long, default_value = "generic")]
        agent_type: String,

        /// Shell command to launch the agent CLI.
        /// For generic: defaults to "bash"
        /// For claude: defaults to "claude --dangerously-skip-permissions"
        #[arg(long)]
        cmd: Option<String>,

        /// Working directory for the agent.
        #[arg(long, default_value = ".")]
        cwd: String,
    },

    /// Run as a shim process (normally invoked by chat or orchestrator).
    Shim {
        /// Unique agent identifier.
        #[arg(long)]
        id: String,

        /// Agent type: claude, codex, kiro, generic
        #[arg(long)]
        agent_type: String,

        /// Shell command to launch the agent CLI.
        #[arg(long)]
        cmd: String,

        /// Working directory for the agent.
        #[arg(long)]
        cwd: String,

        /// Terminal rows.
        #[arg(long, default_value = "50")]
        rows: u16,

        /// Terminal columns.
        #[arg(long, default_value = "220")]
        cols: u16,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Sub::Chat {
            agent_type,
            cmd,
            cwd,
        } => {
            let at: AgentType = agent_type
                .parse()
                .map_err(|e: String| anyhow::anyhow!(e))?;

            let cmd = cmd.unwrap_or_else(|| match at {
                AgentType::Claude => "claude --dangerously-skip-permissions".to_string(),
                AgentType::Codex => "codex".to_string(),
                AgentType::Kiro => "kiro".to_string(),
                AgentType::Generic => "bash".to_string(),
            });

            let cwd = PathBuf::from(&cwd)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(&cwd));

            chat::run(at, &cmd, &cwd)
        }

        Sub::Shim {
            id,
            agent_type,
            cmd,
            cwd,
            rows,
            cols,
        } => {
            let at: AgentType = agent_type
                .parse()
                .map_err(|e: String| anyhow::anyhow!(e))?;

            // Recover the channel socket from fd 3 (inherited from parent).
            let stream = unsafe { UnixStream::from_raw_fd(3) };
            let channel = protocol::Channel::new(stream);

            let args = shim::ShimArgs {
                id,
                agent_type: at,
                cmd,
                cwd: PathBuf::from(cwd),
                rows,
                cols,
            };

            shim::run(args, channel)
        }
    }
}
