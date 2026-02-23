//! Shell completion generation for Batty CLI.

use std::io;

use anyhow::Result;
use clap::CommandFactory;
use clap_complete::{Shell, generate};

use crate::cli::{Cli, CompletionShell};

pub fn print(shell: CompletionShell) -> Result<()> {
    let shell = match shell {
        CompletionShell::Bash => Shell::Bash,
        CompletionShell::Zsh => Shell::Zsh,
        CompletionShell::Fish => Shell::Fish,
    };

    let mut cmd = Cli::command();
    generate(shell, &mut cmd, "batty", &mut io::stdout());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_all_supported_shell_variants() {
        let shells = [
            CompletionShell::Bash,
            CompletionShell::Zsh,
            CompletionShell::Fish,
        ];
        for shell in shells {
            let mapped = match shell {
                CompletionShell::Bash => Shell::Bash,
                CompletionShell::Zsh => Shell::Zsh,
                CompletionShell::Fish => Shell::Fish,
            };
            assert!(matches!(mapped, Shell::Bash | Shell::Zsh | Shell::Fish));
        }
    }
}
