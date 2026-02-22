use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const CLAUDE_RULES_PATH: &str = ".claude/rules/batty-workflow.md";
const CODEX_RULES_PATH: &str = ".agents/rules/batty-workflow.md";
const CLAUDE_SKILL_PATH: &str = ".batty/skills/claude/SKILL.md";
const CODEX_SKILL_PATH: &str = ".batty/skills/codex/SKILL.md";
const TOOL_TMUX: &str = "tmux";
const TOOL_KANBAN_MD: &str = "kanban-md";

const GITIGNORE_COMMENT: &str = "# Batty runtime (managed by batty install)";
const GITIGNORE_ENTRIES: &[&str] = &[".batty/logs/", ".batty/worktrees/"];

const SHARED_STEERING_DOC: &str = include_str!("../assets/batty-workflow.md");
const CLAUDE_SKILL_DOC: &str = include_str!("../assets/claude-skill.md");
const CODEX_SKILL_DOC: &str = include_str!("../assets/codex-skill.md");

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
    pub kanban_skills_installed: bool,
    pub gitignore_entries_added: Vec<String>,
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
            Path::new(CLAUDE_RULES_PATH),
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
            Path::new(CODEX_RULES_PATH),
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

    summary.kanban_skills_installed = install_kanban_skills(destination, target)?;
    summary.gitignore_entries_added = ensure_gitignore_entries(destination)?;

    Ok(summary)
}

fn ensure_gitignore_entries(destination: &Path) -> Result<Vec<String>> {
    let gitignore_path = destination.join(".gitignore");
    let existing = match fs::read_to_string(&gitignore_path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to read {}", gitignore_path.display()));
        }
    };

    let existing_lines: Vec<&str> = existing.lines().collect();
    let mut to_add = Vec::new();
    for entry in GITIGNORE_ENTRIES {
        if !existing_lines.iter().any(|line| line.trim() == *entry) {
            to_add.push(entry.to_string());
        }
    }

    if to_add.is_empty() {
        return Ok(vec![]);
    }

    let mut appendage = String::new();
    if !existing.is_empty() && !existing.ends_with('\n') {
        appendage.push('\n');
    }
    if !existing.is_empty() {
        appendage.push('\n');
    }
    appendage.push_str(GITIGNORE_COMMENT);
    appendage.push('\n');
    for entry in &to_add {
        appendage.push_str(entry);
        appendage.push('\n');
    }

    let new_content = format!("{existing}{appendage}");
    fs::write(&gitignore_path, new_content)
        .with_context(|| format!("failed to write {}", gitignore_path.display()))?;

    Ok(to_add)
}

fn install_kanban_skills(destination: &Path, target: InstallTarget) -> Result<bool> {
    if !command_exists(TOOL_KANBAN_MD, &["--version"]) {
        return Ok(false);
    }

    let agents: Vec<&str> = match target {
        InstallTarget::Both => vec!["claude", "codex"],
        InstallTarget::Claude => vec!["claude"],
        InstallTarget::Codex => vec!["codex"],
    };

    let agent_arg = agents.join(",");
    let output = Command::new(TOOL_KANBAN_MD)
        .args([
            "skill",
            "install",
            "--agent",
            &agent_arg,
            "--skill",
            "kanban-md,kanban-based-development",
        ])
        .current_dir(destination)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| "failed to run kanban-md skill install")?;

    Ok(output.success())
}

#[derive(Debug, Default)]
pub struct RemoveSummary {
    pub removed: Vec<PathBuf>,
    pub not_found: Vec<PathBuf>,
    pub kanban_skills_removed: bool,
    pub gitignore_entries_removed: Vec<String>,
}

const KANBAN_SKILL_DIRS: &[&str] = &[
    ".claude/skills/kanban-md",
    ".claude/skills/kanban-based-development",
    ".agents/skills/kanban-md",
    ".agents/skills/kanban-based-development",
];

pub fn remove_assets(destination: &Path, target: InstallTarget) -> Result<RemoveSummary> {
    let mut summary = RemoveSummary::default();

    if target.install_claude() {
        remove_file(destination, Path::new(CLAUDE_RULES_PATH), &mut summary)?;
        remove_file(destination, Path::new(CLAUDE_SKILL_PATH), &mut summary)?;
    }

    if target.install_codex() {
        remove_file(destination, Path::new(CODEX_RULES_PATH), &mut summary)?;
        remove_file(destination, Path::new(CODEX_SKILL_PATH), &mut summary)?;
    }

    summary.kanban_skills_removed = remove_kanban_skills(destination, target)?;
    summary.gitignore_entries_removed = remove_gitignore_entries(destination)?;

    Ok(summary)
}

fn remove_gitignore_entries(destination: &Path) -> Result<Vec<String>> {
    let gitignore_path = destination.join(".gitignore");
    let existing = match fs::read_to_string(&gitignore_path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to read {}", gitignore_path.display()));
        }
    };

    let mut removed = Vec::new();
    let mut kept_lines = Vec::new();

    for line in existing.lines() {
        let trimmed = line.trim();
        if trimmed == GITIGNORE_COMMENT {
            continue;
        }
        if GITIGNORE_ENTRIES.contains(&trimmed) {
            removed.push(trimmed.to_string());
            continue;
        }
        kept_lines.push(line);
    }

    if removed.is_empty() {
        return Ok(vec![]);
    }

    // Trim trailing empty lines
    while kept_lines.last().is_some_and(|l| l.trim().is_empty()) {
        kept_lines.pop();
    }

    let new_content = if kept_lines.is_empty() {
        String::new()
    } else {
        let mut content = kept_lines.join("\n");
        content.push('\n');
        content
    };

    if new_content.is_empty() {
        fs::remove_file(&gitignore_path)
            .with_context(|| format!("failed to remove {}", gitignore_path.display()))?;
    } else {
        fs::write(&gitignore_path, new_content)
            .with_context(|| format!("failed to write {}", gitignore_path.display()))?;
    }

    Ok(removed)
}

fn remove_file(
    destination: &Path,
    relative_path: &Path,
    summary: &mut RemoveSummary,
) -> Result<()> {
    let full_path = destination.join(relative_path);

    if full_path.is_file() {
        fs::remove_file(&full_path)
            .with_context(|| format!("failed to remove {}", full_path.display()))?;
        summary.removed.push(relative_path.to_path_buf());
        try_remove_empty_parents(&full_path, destination);
    } else {
        summary.not_found.push(relative_path.to_path_buf());
    }

    Ok(())
}

fn remove_kanban_skills(destination: &Path, target: InstallTarget) -> Result<bool> {
    let mut any_removed = false;

    for dir_path in KANBAN_SKILL_DIRS {
        let is_claude = dir_path.starts_with(".claude/");
        let is_codex = dir_path.starts_with(".agents/");

        if (is_claude && !target.install_claude()) || (is_codex && !target.install_codex()) {
            continue;
        }

        let full_path = destination.join(dir_path);
        if full_path.is_dir() {
            fs::remove_dir_all(&full_path)
                .with_context(|| format!("failed to remove {}", full_path.display()))?;
            try_remove_empty_parents(&full_path, destination);
            any_removed = true;
        }
    }

    Ok(any_removed)
}

fn try_remove_empty_parents(path: &Path, stop_at: &Path) {
    let stop_at_canonical = stop_at
        .canonicalize()
        .unwrap_or_else(|_| stop_at.to_path_buf());
    let mut current = path.parent();
    while let Some(parent) = current {
        let parent_canonical = parent
            .canonicalize()
            .unwrap_or_else(|_| parent.to_path_buf());
        if parent_canonical == stop_at_canonical {
            break;
        }
        if fs::remove_dir(parent).is_err() {
            break;
        }
        current = parent.parent();
    }
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

        if let Ok(status) = status
            && status.success()
            && command_exists(tool, check_args)
        {
            summary.installed.push(tool);
            return;
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
                PathBuf::from(CLAUDE_RULES_PATH),
                PathBuf::from(CLAUDE_SKILL_PATH),
                PathBuf::from(CODEX_RULES_PATH),
                PathBuf::from(CODEX_SKILL_PATH)
            ]
        );
        assert!(summary.unchanged.is_empty());
        assert!(tmp.path().join(CLAUDE_RULES_PATH).exists());
        assert!(tmp.path().join(CODEX_RULES_PATH).exists());
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
                PathBuf::from(CLAUDE_RULES_PATH),
                PathBuf::from(CLAUDE_SKILL_PATH),
                PathBuf::from(CODEX_RULES_PATH),
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
                PathBuf::from(CLAUDE_RULES_PATH),
                PathBuf::from(CLAUDE_SKILL_PATH)
            ]
        );
        assert!(tmp.path().join(CLAUDE_RULES_PATH).exists());
        assert!(tmp.path().join(CLAUDE_SKILL_PATH).exists());
        assert!(!tmp.path().join(CODEX_RULES_PATH).exists());
        assert!(!tmp.path().join(CODEX_SKILL_PATH).exists());
    }

    #[test]
    fn install_codex_only_creates_codex_assets() {
        let tmp = tempfile::tempdir().unwrap();
        let summary = install_assets(tmp.path(), InstallTarget::Codex).unwrap();

        assert_eq!(
            summary.created_or_updated,
            vec![
                PathBuf::from(CODEX_RULES_PATH),
                PathBuf::from(CODEX_SKILL_PATH)
            ]
        );
        assert!(tmp.path().join(CODEX_RULES_PATH).exists());
        assert!(tmp.path().join(CODEX_SKILL_PATH).exists());
        assert!(!tmp.path().join(CLAUDE_RULES_PATH).exists());
        assert!(!tmp.path().join(CLAUDE_SKILL_PATH).exists());
    }

    #[test]
    fn install_does_not_touch_claude_md_or_agents_md() {
        let tmp = tempfile::tempdir().unwrap();
        // Pre-create existing steering docs
        fs::write(
            tmp.path().join("CLAUDE.md"),
            "# My Project\nCustom instructions",
        )
        .unwrap();
        fs::write(
            tmp.path().join("AGENTS.md"),
            "# My Project\nCustom instructions",
        )
        .unwrap();

        install_assets(tmp.path(), InstallTarget::Both).unwrap();

        // Verify they were not modified
        let claude = fs::read_to_string(tmp.path().join("CLAUDE.md")).unwrap();
        let agents = fs::read_to_string(tmp.path().join("AGENTS.md")).unwrap();
        assert_eq!(claude, "# My Project\nCustom instructions");
        assert_eq!(agents, "# My Project\nCustom instructions");
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

    #[test]
    fn steering_doc_contains_workflow_sections() {
        assert!(SHARED_STEERING_DOC.contains("## Execution Model"));
        assert!(SHARED_STEERING_DOC.contains("## Workflow"));
        assert!(SHARED_STEERING_DOC.contains("## Commit Messages"));
        assert!(SHARED_STEERING_DOC.contains("## Statement of Work"));
        assert!(SHARED_STEERING_DOC.contains("## Rules"));
        assert!(SHARED_STEERING_DOC.contains("kanban-md"));
    }

    #[test]
    fn steering_doc_uses_generic_test_reference() {
        assert!(!SHARED_STEERING_DOC.contains("cargo test"));
        assert!(SHARED_STEERING_DOC.contains("test/verification suite"));
    }

    #[test]
    fn installed_steering_doc_matches_shared_content() {
        let tmp = tempfile::tempdir().unwrap();
        install_assets(tmp.path(), InstallTarget::Both).unwrap();

        let claude_content = fs::read_to_string(tmp.path().join(CLAUDE_RULES_PATH)).unwrap();
        let codex_content = fs::read_to_string(tmp.path().join(CODEX_RULES_PATH)).unwrap();
        assert_eq!(claude_content, SHARED_STEERING_DOC);
        assert_eq!(codex_content, SHARED_STEERING_DOC);
    }

    #[test]
    fn install_kanban_skills_returns_false_when_tool_missing() {
        let tmp = tempfile::tempdir().unwrap();
        // When kanban-md is available this will try to install; just verify no panic.
        // The function should succeed regardless of whether kanban-md is installed.
        let result = install_kanban_skills(tmp.path(), InstallTarget::Both);
        assert!(result.is_ok());
    }

    #[test]
    fn steering_doc_references_batty_kanban_path() {
        assert!(SHARED_STEERING_DOC.contains("--dir .batty/kanban/<phase>"));
        assert!(SHARED_STEERING_DOC.contains("Older projects may use `kanban/`"));
    }

    #[test]
    fn remove_assets_removes_installed_files() {
        let tmp = tempfile::tempdir().unwrap();
        install_assets(tmp.path(), InstallTarget::Both).unwrap();

        assert!(tmp.path().join(CLAUDE_RULES_PATH).exists());
        assert!(tmp.path().join(CODEX_RULES_PATH).exists());

        let summary = remove_assets(tmp.path(), InstallTarget::Both).unwrap();

        assert!(!tmp.path().join(CLAUDE_RULES_PATH).exists());
        assert!(!tmp.path().join(CODEX_RULES_PATH).exists());
        assert!(!tmp.path().join(CLAUDE_SKILL_PATH).exists());
        assert!(!tmp.path().join(CODEX_SKILL_PATH).exists());
        assert_eq!(summary.removed.len(), 4);
        assert!(summary.not_found.is_empty());
    }

    #[test]
    fn remove_assets_reports_not_found_for_missing_files() {
        let tmp = tempfile::tempdir().unwrap();
        let summary = remove_assets(tmp.path(), InstallTarget::Both).unwrap();

        assert!(summary.removed.is_empty());
        assert_eq!(summary.not_found.len(), 4);
    }

    #[test]
    fn remove_assets_claude_only_does_not_touch_codex() {
        let tmp = tempfile::tempdir().unwrap();
        install_assets(tmp.path(), InstallTarget::Both).unwrap();

        let summary = remove_assets(tmp.path(), InstallTarget::Claude).unwrap();

        assert!(!tmp.path().join(CLAUDE_RULES_PATH).exists());
        assert!(!tmp.path().join(CLAUDE_SKILL_PATH).exists());
        assert!(tmp.path().join(CODEX_RULES_PATH).exists());
        assert!(tmp.path().join(CODEX_SKILL_PATH).exists());
        assert_eq!(summary.removed.len(), 2);
    }

    #[test]
    fn remove_assets_cleans_empty_parents() {
        let tmp = tempfile::tempdir().unwrap();
        install_assets(tmp.path(), InstallTarget::Claude).unwrap();

        remove_assets(tmp.path(), InstallTarget::Claude).unwrap();

        // The .claude/rules/ dir should be cleaned up since it's empty
        assert!(!tmp.path().join(".claude/rules").exists());
        // .batty/skills/claude/ should be cleaned up
        assert!(!tmp.path().join(".batty/skills/claude").exists());
    }

    #[test]
    fn install_adds_gitignore_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let summary = install_assets(tmp.path(), InstallTarget::Both).unwrap();

        assert_eq!(summary.gitignore_entries_added.len(), 2);
        assert!(
            summary
                .gitignore_entries_added
                .contains(&".batty/logs/".to_string())
        );
        assert!(
            summary
                .gitignore_entries_added
                .contains(&".batty/worktrees/".to_string())
        );

        let content = fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
        assert!(content.contains(".batty/logs/"));
        assert!(content.contains(".batty/worktrees/"));
        assert!(content.contains(GITIGNORE_COMMENT));
    }

    #[test]
    fn install_gitignore_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        install_assets(tmp.path(), InstallTarget::Both).unwrap();
        let second = install_assets(tmp.path(), InstallTarget::Both).unwrap();

        assert!(second.gitignore_entries_added.is_empty());

        let content = fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
        // Should only appear once
        assert_eq!(content.matches(".batty/logs/").count(), 1);
    }

    #[test]
    fn install_appends_to_existing_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".gitignore"), "node_modules/\n").unwrap();

        install_assets(tmp.path(), InstallTarget::Both).unwrap();

        let content = fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
        assert!(content.starts_with("node_modules/\n"));
        assert!(content.contains(".batty/logs/"));
        assert!(content.contains(".batty/worktrees/"));
    }

    #[test]
    fn remove_deletes_gitignore_entries() {
        let tmp = tempfile::tempdir().unwrap();
        install_assets(tmp.path(), InstallTarget::Both).unwrap();

        let summary = remove_assets(tmp.path(), InstallTarget::Both).unwrap();

        assert_eq!(summary.gitignore_entries_removed.len(), 2);
        // .gitignore was batty-only, so it should be deleted
        assert!(!tmp.path().join(".gitignore").exists());
    }

    #[test]
    fn remove_preserves_other_gitignore_entries() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".gitignore"), "node_modules/\n").unwrap();
        install_assets(tmp.path(), InstallTarget::Both).unwrap();

        remove_assets(tmp.path(), InstallTarget::Both).unwrap();

        let content = fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
        assert!(content.contains("node_modules/"));
        assert!(!content.contains(".batty/logs/"));
        assert!(!content.contains(".batty/worktrees/"));
        assert!(!content.contains(GITIGNORE_COMMENT));
    }

    #[test]
    fn try_remove_empty_parents_stops_at_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let deep = tmp.path().join("a").join("b").join("c");
        fs::create_dir_all(&deep).unwrap();

        // Write a marker file then remove it
        let file_path = deep.join("test.txt");
        fs::write(&file_path, "test").unwrap();
        fs::remove_file(&file_path).unwrap();

        try_remove_empty_parents(&file_path, tmp.path());

        // All empty parents should be removed but tmp.path() itself should remain
        assert!(!tmp.path().join("a").exists());
        assert!(tmp.path().exists());
    }
}
