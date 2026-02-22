use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

const CLAUDE_STEERING_PATH: &str = "CLAUDE.md";
const CODEX_STEERING_PATH: &str = "AGENTS.md";
const CLAUDE_SKILL_PATH: &str = ".batty/skills/claude/SKILL.md";
const CODEX_SKILL_PATH: &str = ".batty/skills/codex/SKILL.md";

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
}
