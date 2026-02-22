mod agent;
mod cli;
mod config;
mod dod;
mod log;
mod policy;
mod prompt;
mod supervisor;
mod task;
mod work;

use anyhow::Result;
use clap::Parser;
use tracing::info;

use cli::{Cli, Command};
use config::ProjectConfig;

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
            parallel: _,
            agent,
            policy,
        } => {
            let agent_name = agent.as_deref().unwrap_or(&config.defaults.agent);
            let policy_str = policy.as_deref();

            work::run_phase(&target, &config, agent_name, policy_str, &cwd)?;
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
            if let Some(ref p) = config_path {
                println!("  source:      {}", p.display());
            } else {
                println!("  source:      (defaults — no .batty/config.toml found)");
            }
        }
    }

    Ok(())
}
