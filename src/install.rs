use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const CLAUDE_STEERING_PATH: &str = "CLAUDE.md";
const CODEX_STEERING_PATH: &str = "AGENTS.md";
const CLAUDE_SKILL_PATH: &str = ".batty/skills/claude/SKILL.md";
const CODEX_SKILL_PATH: &str = ".batty/skills/codex/SKILL.md";
const TOOL_TMUX: &str = "tmux";
const TOOL_KANBAN_MD: &str = "kanban-md";

const SHARED_STEERING_DOC: &str = r#"# Batty Agent Steering

This repository is managed with Batty.

## Workflow

1. Read the phase board and pick the next unblocked task.
2. Implement the task with focused, minimal changes.
3. Run required verification (`cargo test` at minimum before commit).
4. Record a statement of work on the task.
5. Move the task to done and commit with a detailed message.

## Guardrails

- Keep changes deterministic and idempotent where possible.
- Prefer small, composable implementations over abstractions.
- Do not skip failing tests or quality checks.
"#;

const CLAUDE_SKILL_DOC: &str = r#"# Batty Claude Skill Pack

Use this skill pack when running Claude Code in Batty-supervised phase work.

## Expectations

- Follow `CLAUDE.md` as the authoritative steering document.
- Work through kanban tasks in dependency order.
- Leave a statement of work on every completed task.
"#;

const CODEX_SKILL_DOC: &str = r#"# Batty Codex Skill Pack

Use this skill pack when running Codex in Batty-supervised phase work.

## Expectations

- Follow `AGENTS.md` as the authoritative steering document.
- Work through kanban tasks in dependency order.
- Leave a statement of work on every completed task.
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallTarget {
    Both,
    Claude,
    Codex,
}

impl InstallTarget {
    fn install_claude(self) -> bool {
        matches!(self, Self::Both | Self::Claude)
    }

    fn install_codex(self) -> bool {
        matches!(self, Self::Both | Self::Codex)
    }
}

#[derive(Debug, Default)]
pub struct InstallSummary {
    pub created_or_updated: Vec<PathBuf>,
    pub unchanged: Vec<PathBuf>,
}

#[derive(Debug, Default)]
pub struct PrerequisiteSummary {
    pub present: Vec<&'static str>,
    pub installed: Vec<&'static str>,
}

#[derive(Debug, Clone, Copy)]
struct Installer {
    display: &'static str,
    program: &'static str,
    args: &'static [&'static str],
}

pub fn install_assets(destination: &Path, target: InstallTarget) -> Result<InstallSummary> {
    let mut summary = InstallSummary::default();

    if target.install_claude() {
        install_file(
            destination,
            Path::new(CLAUDE_STEERING_PATH),
            SHARED_STEERING_DOC,
            &mut summary,
        )?;
        install_file(
            destination,
            Path::new(CLAUDE_SKILL_PATH),
            CLAUDE_SKILL_DOC,
            &mut summary,
        )?;
    }

    if target.install_codex() {
        install_file(
            destination,
            Path::new(CODEX_STEERING_PATH),
            SHARED_STEERING_DOC,
            &mut summary,
        )?;
        install_file(
            destination,
            Path::new(CODEX_SKILL_PATH),
            CODEX_SKILL_DOC,
            &mut summary,
        )?;
    }

    Ok(summary)
}

pub fn ensure_prerequisites() -> Result<PrerequisiteSummary> {
    let mut summary = PrerequisiteSummary::default();
    let mut missing = Vec::new();

    ensure_dependency(
        TOOL_TMUX,
        &["-V"],
        &tmux_installers(),
        "Install tmux manually (e.g., `brew install tmux` or `sudo apt-get install -y tmux`).",
        &mut summary,
        &mut missing,
    );
    ensure_dependency(
        TOOL_KANBAN_MD,
        &["--version"],
        &kanban_md_installers(),
        "Install kanban-md manually with `cargo install kanban-md --locked` and ensure `~/.cargo/bin` is on PATH.",
        &mut summary,
        &mut missing,
    );

    if missing.is_empty() {
        Ok(summary)
    } else {
        anyhow::bail!(
            "required project dependencies are missing:\n\n{}",
            missing.join("\n\n")
        );
    }
}

fn ensure_dependency(
    tool: &'static str,
    check_args: &[&str],
    installers: &[Installer],
    manual_hint: &str,
    summary: &mut PrerequisiteSummary,
    missing: &mut Vec<String>,
) {
    if command_exists(tool, check_args) {
        summary.present.push(tool);
        return;
    }

    let mut attempted = Vec::new();
    for installer in installers {
        attempted.push(installer.display.to_string());
        let status = Command::new(installer.program)
            .args(installer.args)
            .status();

        if let Ok(status) = status {
            if status.success() && command_exists(tool, check_args) {
                summary.installed.push(tool);
                return;
            }
        }
    }

    let attempted_text = if attempted.is_empty() {
        "No supported automatic installer was available on this system.".to_string()
    } else {
        format!(
            "Attempted automatic installers: {}.",
            attempted
                .iter()
                .map(|a| format!("`{a}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };

    missing.push(format!(
        "`{tool}` is not available on PATH. {attempted_text} {manual_hint}"
    ));
}

fn command_exists(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn tmux_installers() -> Vec<Installer> {
    let mut installers = Vec::new();
    if command_exists("brew", &["--version"]) {
        installers.push(Installer {
            display: "brew install tmux",
            program: "brew",
            args: &["install", "tmux"],
        });
    }
    if command_exists("sudo", &["-V"]) && command_exists("apt-get", &["--version"]) {
        installers.push(Installer {
            display: "sudo -n apt-get install -y tmux",
            program: "sudo",
            args: &["-n", "apt-get", "install", "-y", "tmux"],
        });
    }
    if command_exists("sudo", &["-V"]) && command_exists("dnf", &["--version"]) {
        installers.push(Installer {
            display: "sudo -n dnf install -y tmux",
            program: "sudo",
            args: &["-n", "dnf", "install", "-y", "tmux"],
        });
    }
    if command_exists("sudo", &["-V"]) && command_exists("pacman", &["--version"]) {
        installers.push(Installer {
            display: "sudo -n pacman -Sy --noconfirm tmux",
            program: "sudo",
            args: &["-n", "pacman", "-Sy", "--noconfirm", "tmux"],
        });
    }
    installers
}

fn kanban_md_installers() -> Vec<Installer> {
    let mut installers = Vec::new();
    if command_exists("cargo", &["--version"]) {
        installers.push(Installer {
            display: "cargo install kanban-md --locked",
            program: "cargo",
            args: &["install", "kanban-md", "--locked"],
        });
    }
    if command_exists("brew", &["--version"]) {
        installers.push(Installer {
            display: "brew install kanban-md",
            program: "brew",
            args: &["install", "kanban-md"],
        });
    }
    installers
}

fn install_file(
    destination: &Path,
    relative_path: &Path,
    content: &str,
    summary: &mut InstallSummary,
) -> Result<()> {
    let full_path = destination.join(relative_path);

    if let Some(parent) = full_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }

    let should_write = match fs::read_to_string(&full_path) {
        Ok(existing) => existing != content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
        Err(err) => {
            return Err(err).with_context(|| format!("failed to read {}", full_path.display()));
        }
    };

    if should_write {
        fs::write(&full_path, content)
            .with_context(|| format!("failed to write {}", full_path.display()))?;
        summary.created_or_updated.push(relative_path.to_path_buf());
    } else {
        summary.unchanged.push(relative_path.to_path_buf());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_both_creates_all_expected_files() {
        let tmp = tempfile::tempdir().unwrap();
        let summary = install_assets(tmp.path(), InstallTarget::Both).unwrap();

        assert_eq!(
            summary.created_or_updated,
            vec![
                PathBuf::from(CLAUDE_STEERING_PATH),
                PathBuf::from(CLAUDE_SKILL_PATH),
                PathBuf::from(CODEX_STEERING_PATH),
                PathBuf::from(CODEX_SKILL_PATH)
            ]
        );
        assert!(summary.unchanged.is_empty());
        assert!(tmp.path().join(CLAUDE_STEERING_PATH).exists());
        assert!(tmp.path().join(CODEX_STEERING_PATH).exists());
        assert!(tmp.path().join(CLAUDE_SKILL_PATH).exists());
        assert!(tmp.path().join(CODEX_SKILL_PATH).exists());
    }

    #[test]
    fn install_is_idempotent_on_rerun() {
        let tmp = tempfile::tempdir().unwrap();
        install_assets(tmp.path(), InstallTarget::Both).unwrap();
        let second = install_assets(tmp.path(), InstallTarget::Both).unwrap();

        assert!(second.created_or_updated.is_empty());
        assert_eq!(
            second.unchanged,
            vec![
                PathBuf::from(CLAUDE_STEERING_PATH),
                PathBuf::from(CLAUDE_SKILL_PATH),
                PathBuf::from(CODEX_STEERING_PATH),
                PathBuf::from(CODEX_SKILL_PATH)
            ]
        );
    }

    #[test]
    fn install_claude_only_creates_claude_assets() {
        let tmp = tempfile::tempdir().unwrap();
        let summary = install_assets(tmp.path(), InstallTarget::Claude).unwrap();

        assert_eq!(
            summary.created_or_updated,
            vec![
                PathBuf::from(CLAUDE_STEERING_PATH),
                PathBuf::from(CLAUDE_SKILL_PATH)
            ]
        );
        assert!(tmp.path().join(CLAUDE_STEERING_PATH).exists());
        assert!(tmp.path().join(CLAUDE_SKILL_PATH).exists());
        assert!(!tmp.path().join(CODEX_STEERING_PATH).exists());
        assert!(!tmp.path().join(CODEX_SKILL_PATH).exists());
    }

    #[test]
    fn install_codex_only_creates_codex_assets() {
        let tmp = tempfile::tempdir().unwrap();
        let summary = install_assets(tmp.path(), InstallTarget::Codex).unwrap();

        assert_eq!(
            summary.created_or_updated,
            vec![
                PathBuf::from(CODEX_STEERING_PATH),
                PathBuf::from(CODEX_SKILL_PATH)
            ]
        );
        assert!(tmp.path().join(CODEX_STEERING_PATH).exists());
        assert!(tmp.path().join(CODEX_SKILL_PATH).exists());
        assert!(!tmp.path().join(CLAUDE_STEERING_PATH).exists());
        assert!(!tmp.path().join(CLAUDE_SKILL_PATH).exists());
    }

    #[test]
    fn command_exists_detects_available_program() {
        assert!(command_exists("cargo", &["--version"]));
    }

    #[test]
    fn command_exists_detects_missing_program() {
        assert!(!command_exists(
            "definitely-missing-batty-test-command",
            &["--version"]
        ));
    }

    #[test]
    fn kanban_md_installers_include_cargo_strategy() {
        let installers = kanban_md_installers();
        assert!(
            installers
                .iter()
                .any(|installer| installer.display == "cargo install kanban-md --locked")
        );
    }
}
