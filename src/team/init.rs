//! Team initialization, template management, and run export.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use super::{
    TEAM_CONFIG_FILE, daemon_log_path, now_unix, orchestrator_log_path, team_config_dir,
    team_config_path, team_events_path,
};

/// Returns `~/.batty/templates/`.
pub fn templates_base_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("cannot determine home directory")?;
    Ok(PathBuf::from(home).join(".batty").join("templates"))
}

/// Scaffold `.batty/team_config/` with default team.yaml and prompt templates.
pub fn init_team(project_root: &Path, template: &str, agent: Option<&str>) -> Result<Vec<PathBuf>> {
    let config_dir = team_config_dir(project_root);
    std::fs::create_dir_all(&config_dir)
        .with_context(|| format!("failed to create {}", config_dir.display()))?;

    let mut created = Vec::new();

    let yaml_path = config_dir.join(TEAM_CONFIG_FILE);
    if yaml_path.exists() {
        bail!(
            "team config already exists at {}; remove it first or edit directly",
            yaml_path.display()
        );
    }

    let yaml_content = match template {
        "solo" => include_str!("templates/team_solo.yaml"),
        "pair" => include_str!("templates/team_pair.yaml"),
        "squad" => include_str!("templates/team_squad.yaml"),
        "large" => include_str!("templates/team_large.yaml"),
        "research" => include_str!("templates/team_research.yaml"),
        "software" => include_str!("templates/team_software.yaml"),
        "batty" => include_str!("templates/team_batty.yaml"),
        _ => include_str!("templates/team_simple.yaml"),
    };
    // Replace all role-level agent backends when --agent is specified
    let yaml_content = if let Some(agent_name) = agent {
        yaml_content
            .replace("agent: claude", &format!("agent: {agent_name}"))
            .replace("agent: codex", &format!("agent: {agent_name}"))
    } else {
        yaml_content.to_string()
    };
    std::fs::write(&yaml_path, &yaml_content)
        .with_context(|| format!("failed to write {}", yaml_path.display()))?;
    created.push(yaml_path);

    // Install prompt .md files matching the template's roles
    let prompt_files: &[(&str, &str)] = match template {
        "research" => &[
            (
                "research_lead.md",
                include_str!("templates/research_lead.md"),
            ),
            ("sub_lead.md", include_str!("templates/sub_lead.md")),
            ("researcher.md", include_str!("templates/researcher.md")),
        ],
        "software" => &[
            ("tech_lead.md", include_str!("templates/tech_lead.md")),
            ("eng_manager.md", include_str!("templates/eng_manager.md")),
            ("developer.md", include_str!("templates/developer.md")),
        ],
        "batty" => &[
            (
                "batty_architect.md",
                include_str!("templates/batty_architect.md"),
            ),
            (
                "batty_manager.md",
                include_str!("templates/batty_manager.md"),
            ),
            (
                "batty_engineer.md",
                include_str!("templates/batty_engineer.md"),
            ),
        ],
        _ => &[
            ("architect.md", include_str!("templates/architect.md")),
            ("manager.md", include_str!("templates/manager.md")),
            ("engineer.md", include_str!("templates/engineer.md")),
        ],
    };

    for (name, content) in prompt_files {
        let path = config_dir.join(name);
        if !path.exists() {
            std::fs::write(&path, content)
                .with_context(|| format!("failed to write {}", path.display()))?;
            created.push(path);
        }
    }

    let directive_files = [
        (
            "replenishment_context.md",
            include_str!("templates/replenishment_context.md"),
        ),
        (
            "review_policy.md",
            include_str!("templates/review_policy.md"),
        ),
        (
            "escalation_policy.md",
            include_str!("templates/escalation_policy.md"),
        ),
    ];
    for (name, content) in directive_files {
        let path = config_dir.join(name);
        if !path.exists() {
            std::fs::write(&path, content)
                .with_context(|| format!("failed to write {}", path.display()))?;
            created.push(path);
        }
    }

    // Initialize kanban-md board in the team config directory
    let board_dir = config_dir.join("board");
    if !board_dir.exists() {
        let output = std::process::Command::new("kanban-md")
            .args(["init", "--dir", &board_dir.to_string_lossy()])
            .output();
        match output {
            Ok(out) if out.status.success() => {
                created.push(board_dir);
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                warn!("kanban-md init failed: {stderr}; falling back to plain kanban.md");
                let kanban_path = config_dir.join("kanban.md");
                std::fs::write(
                    &kanban_path,
                    "# Kanban Board\n\n## Backlog\n\n## In Progress\n\n## Done\n",
                )?;
                created.push(kanban_path);
            }
            Err(_) => {
                warn!("kanban-md not found; falling back to plain kanban.md");
                let kanban_path = config_dir.join("kanban.md");
                std::fs::write(
                    &kanban_path,
                    "# Kanban Board\n\n## Backlog\n\n## In Progress\n\n## Done\n",
                )?;
                created.push(kanban_path);
            }
        }
    }

    info!(dir = %config_dir.display(), files = created.len(), "scaffolded team config");
    Ok(created)
}

pub fn list_available_templates() -> Result<Vec<String>> {
    let templates_dir = templates_base_dir()?;
    if !templates_dir.is_dir() {
        bail!(
            "no templates directory found at {}",
            templates_dir.display()
        );
    }

    let mut templates = Vec::new();
    for entry in std::fs::read_dir(&templates_dir)
        .with_context(|| format!("failed to read {}", templates_dir.display()))?
    {
        let entry = entry?;
        if entry.path().is_dir() {
            templates.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    templates.sort();
    Ok(templates)
}

fn copy_template_dir(src: &Path, dst: &Path, created: &mut Vec<PathBuf>) -> Result<()> {
    std::fs::create_dir_all(dst).with_context(|| format!("failed to create {}", dst.display()))?;
    for entry in
        std::fs::read_dir(src).with_context(|| format!("failed to read {}", src.display()))?
    {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_template_dir(&src_path, &dst_path, created)?;
        } else {
            std::fs::copy(&src_path, &dst_path).with_context(|| {
                format!(
                    "failed to copy template file from {} to {}",
                    src_path.display(),
                    dst_path.display()
                )
            })?;
            created.push(dst_path);
        }
    }
    Ok(())
}

pub fn init_from_template(project_root: &Path, template_name: &str) -> Result<Vec<PathBuf>> {
    let templates_dir = templates_base_dir()?;
    if !templates_dir.is_dir() {
        bail!(
            "no templates directory found at {}",
            templates_dir.display()
        );
    }

    let available = list_available_templates()?;
    if !available.iter().any(|name| name == template_name) {
        let available_display = if available.is_empty() {
            "(none)".to_string()
        } else {
            available.join(", ")
        };
        bail!(
            "template '{}' not found in {}; available templates: {}",
            template_name,
            templates_dir.display(),
            available_display
        );
    }

    let config_dir = team_config_dir(project_root);
    let yaml_path = config_dir.join(TEAM_CONFIG_FILE);
    if yaml_path.exists() {
        bail!(
            "team config already exists at {}; remove it first or edit directly",
            yaml_path.display()
        );
    }

    let source_dir = templates_dir.join(template_name);
    let mut created = Vec::new();
    copy_template_dir(&source_dir, &config_dir, &mut created)?;
    info!(
        template = template_name,
        source = %source_dir.display(),
        dest = %config_dir.display(),
        files = created.len(),
        "copied team config from user template"
    );
    Ok(created)
}

/// Export the current team config as a reusable template.
pub fn export_template(project_root: &Path, name: &str) -> Result<usize> {
    let config_dir = team_config_dir(project_root);
    let team_yaml = config_dir.join(TEAM_CONFIG_FILE);
    if !team_yaml.is_file() {
        bail!("team config missing at {}", team_yaml.display());
    }

    let template_dir = templates_base_dir()?.join(name);
    if template_dir.exists() {
        eprintln!(
            "warning: overwriting existing template at {}",
            template_dir.display()
        );
    }
    std::fs::create_dir_all(&template_dir)
        .with_context(|| format!("failed to create {}", template_dir.display()))?;

    let mut copied = 0usize;
    copy_template_file(&team_yaml, &template_dir.join(TEAM_CONFIG_FILE))?;
    copied += 1;

    let mut prompt_paths = std::fs::read_dir(&config_dir)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect::<Vec<_>>();
    prompt_paths.sort();

    for source in prompt_paths {
        let file_name = source
            .file_name()
            .context("template source missing file name")?;
        copy_template_file(&source, &template_dir.join(file_name))?;
        copied += 1;
    }

    Ok(copied)
}

pub fn export_run(project_root: &Path) -> Result<PathBuf> {
    let team_yaml = team_config_path(project_root);
    if !team_yaml.is_file() {
        bail!("team config missing at {}", team_yaml.display());
    }

    let export_dir = create_run_export_dir(project_root)?;
    copy_template_file(&team_yaml, &export_dir.join(TEAM_CONFIG_FILE))?;

    copy_dir_if_exists(
        &team_config_dir(project_root).join("board").join("tasks"),
        &export_dir.join("board").join("tasks"),
    )?;
    copy_file_if_exists(
        &team_events_path(project_root),
        &export_dir.join("events.jsonl"),
    )?;
    copy_file_if_exists(
        &daemon_log_path(project_root),
        &export_dir.join("daemon.log"),
    )?;
    copy_file_if_exists(
        &orchestrator_log_path(project_root),
        &export_dir.join("orchestrator.log"),
    )?;
    copy_dir_if_exists(
        &project_root.join(".batty").join("retrospectives"),
        &export_dir.join("retrospectives"),
    )?;
    copy_file_if_exists(
        &project_root.join(".batty").join("test_timing.jsonl"),
        &export_dir.join("test_timing.jsonl"),
    )?;

    Ok(export_dir)
}

fn copy_template_file(source: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::copy(source, destination).with_context(|| {
        format!(
            "failed to copy {} to {}",
            source.display(),
            destination.display()
        )
    })?;
    Ok(())
}

fn exports_dir(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("exports")
}

fn create_run_export_dir(project_root: &Path) -> Result<PathBuf> {
    let base = exports_dir(project_root);
    std::fs::create_dir_all(&base)
        .with_context(|| format!("failed to create {}", base.display()))?;

    let timestamp = now_unix();
    let primary = base.join(timestamp.to_string());
    if !primary.exists() {
        std::fs::create_dir(&primary)
            .with_context(|| format!("failed to create {}", primary.display()))?;
        return Ok(primary);
    }

    for suffix in 1.. {
        let candidate = base.join(format!("{timestamp}-{suffix}"));
        if candidate.exists() {
            continue;
        }
        std::fs::create_dir(&candidate)
            .with_context(|| format!("failed to create {}", candidate.display()))?;
        return Ok(candidate);
    }

    unreachable!("infinite suffix iterator should always return or continue");
}

fn copy_file_if_exists(source: &Path, destination: &Path) -> Result<()> {
    if source.is_file() {
        copy_template_file(source, destination)?;
    }
    Ok(())
}

fn copy_dir_if_exists(source: &Path, destination: &Path) -> Result<()> {
    if source.is_dir() {
        let mut created = Vec::new();
        copy_template_dir(source, destination, &mut created)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::ffi::OsString;

    use crate::team::{
        daemon_log_path, orchestrator_log_path, team_config_dir, team_config_path,
        team_events_path,
    };

    struct HomeGuard {
        original_home: Option<OsString>,
    }

    impl HomeGuard {
        fn set(path: &Path) -> Self {
            let original_home = std::env::var_os("HOME");
            unsafe {
                std::env::set_var("HOME", path);
            }
            Self { original_home }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match &self.original_home {
                Some(home) => unsafe {
                    std::env::set_var("HOME", home);
                },
                None => unsafe {
                    std::env::remove_var("HOME");
                },
            }
        }
    }

    #[test]
    fn init_team_creates_scaffolding() {
        let tmp = tempfile::tempdir().unwrap();
        let created = init_team(tmp.path(), "simple", None).unwrap();
        assert!(!created.is_empty());
        assert!(team_config_path(tmp.path()).exists());
        assert!(team_config_dir(tmp.path()).join("architect.md").exists());
        assert!(team_config_dir(tmp.path()).join("manager.md").exists());
        assert!(team_config_dir(tmp.path()).join("engineer.md").exists());
        assert!(
            team_config_dir(tmp.path())
                .join("replenishment_context.md")
                .exists()
        );
        assert!(
            team_config_dir(tmp.path())
                .join("review_policy.md")
                .exists()
        );
        assert!(
            team_config_dir(tmp.path())
                .join("escalation_policy.md")
                .exists()
        );
        // kanban-md creates board/ directory; fallback creates kanban.md
        let config = team_config_dir(tmp.path());
        assert!(config.join("board").is_dir() || config.join("kanban.md").exists());
    }

    #[test]
    fn init_team_refuses_if_exists() {
        let tmp = tempfile::tempdir().unwrap();
        init_team(tmp.path(), "simple", None).unwrap();
        let result = init_team(tmp.path(), "simple", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    #[serial]
    fn init_from_template_copies_files() {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let _home_guard = HomeGuard::set(home.path());

        let template_dir = home.path().join(".batty").join("templates").join("custom");
        std::fs::create_dir_all(template_dir.join("board")).unwrap();
        std::fs::write(template_dir.join("team.yaml"), "name: custom\nroles: []\n").unwrap();
        std::fs::write(template_dir.join("architect.md"), "# Architect\n").unwrap();
        std::fs::write(template_dir.join("board").join("task.md"), "task\n").unwrap();

        let created = init_from_template(project.path(), "custom").unwrap();

        assert!(!created.is_empty());
        assert_eq!(
            std::fs::read_to_string(team_config_path(project.path())).unwrap(),
            "name: custom\nroles: []\n"
        );
        assert!(
            team_config_dir(project.path())
                .join("architect.md")
                .exists()
        );
        assert!(
            team_config_dir(project.path())
                .join("board")
                .join("task.md")
                .exists()
        );
    }

    #[test]
    #[serial]
    fn init_from_template_missing_template_errors_with_available_list() {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let _home_guard = HomeGuard::set(home.path());

        let templates_root = home.path().join(".batty").join("templates");
        std::fs::create_dir_all(templates_root.join("alpha")).unwrap();
        std::fs::create_dir_all(templates_root.join("beta")).unwrap();

        let error = init_from_template(project.path(), "missing").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("template 'missing' not found"));
        assert!(message.contains("alpha"));
        assert!(message.contains("beta"));
    }

    #[test]
    #[serial]
    fn init_from_template_errors_when_templates_dir_is_missing() {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let _home_guard = HomeGuard::set(home.path());

        let error = init_from_template(project.path(), "missing").unwrap_err();
        assert!(error.to_string().contains("no templates directory found"));
    }

    #[test]
    fn init_team_large_template() {
        let tmp = tempfile::tempdir().unwrap();
        let created = init_team(tmp.path(), "large", None).unwrap();
        assert!(!created.is_empty());
        let content = std::fs::read_to_string(team_config_path(tmp.path())).unwrap();
        assert!(content.contains("instances: 3") || content.contains("instances: 5"));
    }

    #[test]
    fn init_team_solo_template() {
        let tmp = tempfile::tempdir().unwrap();
        let created = init_team(tmp.path(), "solo", None).unwrap();
        assert!(!created.is_empty());
        let content = std::fs::read_to_string(team_config_path(tmp.path())).unwrap();
        assert!(content.contains("role_type: engineer"));
        assert!(!content.contains("role_type: manager"));
    }

    #[test]
    fn init_team_pair_template() {
        let tmp = tempfile::tempdir().unwrap();
        let created = init_team(tmp.path(), "pair", None).unwrap();
        assert!(!created.is_empty());
        let content = std::fs::read_to_string(team_config_path(tmp.path())).unwrap();
        assert!(content.contains("role_type: architect"));
        assert!(content.contains("role_type: engineer"));
        assert!(!content.contains("role_type: manager"));
    }

    #[test]
    fn init_team_squad_template() {
        let tmp = tempfile::tempdir().unwrap();
        let created = init_team(tmp.path(), "squad", None).unwrap();
        assert!(!created.is_empty());
        let content = std::fs::read_to_string(team_config_path(tmp.path())).unwrap();
        assert!(content.contains("instances: 5"));
        assert!(content.contains("layout:"));
    }

    #[test]
    fn init_team_research_template() {
        let tmp = tempfile::tempdir().unwrap();
        let created = init_team(tmp.path(), "research", None).unwrap();
        assert!(!created.is_empty());
        let content = std::fs::read_to_string(team_config_path(tmp.path())).unwrap();
        assert!(content.contains("principal"));
        assert!(content.contains("sub-lead"));
        assert!(content.contains("researcher"));
        // Research-specific .md files installed
        assert!(
            team_config_dir(tmp.path())
                .join("research_lead.md")
                .exists()
        );
        assert!(team_config_dir(tmp.path()).join("sub_lead.md").exists());
        assert!(team_config_dir(tmp.path()).join("researcher.md").exists());
        // Generic files NOT installed
        assert!(!team_config_dir(tmp.path()).join("architect.md").exists());
    }

    #[test]
    fn init_team_software_template() {
        let tmp = tempfile::tempdir().unwrap();
        let created = init_team(tmp.path(), "software", None).unwrap();
        assert!(!created.is_empty());
        let content = std::fs::read_to_string(team_config_path(tmp.path())).unwrap();
        assert!(content.contains("tech-lead"));
        assert!(content.contains("backend-mgr"));
        assert!(content.contains("frontend-mgr"));
        assert!(content.contains("developer"));
        // Software-specific .md files installed
        assert!(team_config_dir(tmp.path()).join("tech_lead.md").exists());
        assert!(team_config_dir(tmp.path()).join("eng_manager.md").exists());
        assert!(team_config_dir(tmp.path()).join("developer.md").exists());
    }

    #[test]
    fn init_team_batty_template() {
        let tmp = tempfile::tempdir().unwrap();
        let created = init_team(tmp.path(), "batty", None).unwrap();
        assert!(!created.is_empty());
        let content = std::fs::read_to_string(team_config_path(tmp.path())).unwrap();
        assert!(content.contains("batty-dev"));
        assert!(content.contains("role_type: architect"));
        assert!(content.contains("role_type: manager"));
        assert!(content.contains("instances: 4"));
        assert!(content.contains("batty_architect.md"));
        // Batty-specific .md files installed
        assert!(
            team_config_dir(tmp.path())
                .join("batty_architect.md")
                .exists()
        );
        assert!(
            team_config_dir(tmp.path())
                .join("batty_manager.md")
                .exists()
        );
        assert!(
            team_config_dir(tmp.path())
                .join("batty_engineer.md")
                .exists()
        );
        assert!(
            team_config_dir(tmp.path())
                .join("review_policy.md")
                .exists()
        );
    }

    #[test]
    fn init_with_agent_codex_sets_backend() {
        let tmp = tempfile::tempdir().unwrap();
        let _created = init_team(tmp.path(), "simple", Some("codex")).unwrap();
        let content = std::fs::read_to_string(team_config_path(tmp.path())).unwrap();
        assert!(
            content.contains("agent: codex"),
            "all agent fields should be codex"
        );
        assert!(
            !content.contains("agent: claude"),
            "no claude agents should remain"
        );
    }

    #[test]
    fn init_with_agent_kiro_sets_backend() {
        let tmp = tempfile::tempdir().unwrap();
        let _created = init_team(tmp.path(), "pair", Some("kiro")).unwrap();
        let content = std::fs::read_to_string(team_config_path(tmp.path())).unwrap();
        assert!(
            content.contains("agent: kiro"),
            "all agent fields should be kiro"
        );
        assert!(
            !content.contains("agent: claude"),
            "no claude agents should remain"
        );
        assert!(
            !content.contains("agent: codex"),
            "no codex agents should remain"
        );
    }

    #[test]
    fn init_default_agent_is_claude() {
        let tmp = tempfile::tempdir().unwrap();
        let _created = init_team(tmp.path(), "simple", None).unwrap();
        let content = std::fs::read_to_string(team_config_path(tmp.path())).unwrap();
        assert!(
            content.contains("agent: claude"),
            "default agent should be claude"
        );
    }

    #[test]
    #[serial]
    fn export_template_creates_directory_and_copies_files() {
        let tmp = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(tmp.path());
        let project_root = tmp.path().join("project");
        let config_dir = team_config_dir(&project_root);
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("team.yaml"), "name: demo\n").unwrap();
        std::fs::write(config_dir.join("architect.md"), "architect prompt\n").unwrap();

        let copied = export_template(&project_root, "demo-template").unwrap();
        let template_dir = templates_base_dir().unwrap().join("demo-template");

        assert_eq!(copied, 2);
        assert_eq!(
            std::fs::read_to_string(template_dir.join("team.yaml")).unwrap(),
            "name: demo\n"
        );
        assert_eq!(
            std::fs::read_to_string(template_dir.join("architect.md")).unwrap(),
            "architect prompt\n"
        );
    }

    #[test]
    #[serial]
    fn export_template_overwrites_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(tmp.path());
        let project_root = tmp.path().join("project");
        let config_dir = team_config_dir(&project_root);
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("team.yaml"), "name: first\n").unwrap();
        std::fs::write(config_dir.join("manager.md"), "v1\n").unwrap();

        export_template(&project_root, "demo-template").unwrap();

        std::fs::write(config_dir.join("team.yaml"), "name: second\n").unwrap();
        std::fs::write(config_dir.join("manager.md"), "v2\n").unwrap();

        let copied = export_template(&project_root, "demo-template").unwrap();
        let template_dir = templates_base_dir().unwrap().join("demo-template");

        assert_eq!(copied, 2);
        assert_eq!(
            std::fs::read_to_string(template_dir.join("team.yaml")).unwrap(),
            "name: second\n"
        );
        assert_eq!(
            std::fs::read_to_string(template_dir.join("manager.md")).unwrap(),
            "v2\n"
        );
    }

    #[test]
    #[serial]
    fn export_template_missing_team_yaml_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let _home = HomeGuard::set(tmp.path());
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(team_config_dir(&project_root)).unwrap();

        let error = export_template(&project_root, "demo-template").unwrap_err();

        assert!(error.to_string().contains("team config missing"));
    }

    #[test]
    fn export_run_copies_requested_run_state_only() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("project");
        let config_dir = team_config_dir(&project_root);
        let tasks_dir = config_dir.join("board").join("tasks");
        let retrospectives_dir = project_root.join(".batty").join("retrospectives");
        let worktree_dir = project_root
            .join(".batty")
            .join("worktrees")
            .join("eng-1-1")
            .join(".codex")
            .join("sessions");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::create_dir_all(&retrospectives_dir).unwrap();
        std::fs::create_dir_all(&worktree_dir).unwrap();

        std::fs::write(config_dir.join("team.yaml"), "name: demo\n").unwrap();
        std::fs::write(tasks_dir.join("001-task.md"), "---\nid: 1\n---\n").unwrap();
        std::fs::write(
            team_events_path(&project_root),
            "{\"event\":\"daemon_started\"}\n",
        )
        .unwrap();
        std::fs::write(daemon_log_path(&project_root), "daemon-log\n").unwrap();
        std::fs::write(orchestrator_log_path(&project_root), "orchestrator-log\n").unwrap();
        std::fs::write(retrospectives_dir.join("retro.md"), "# Retro\n").unwrap();
        std::fs::write(
            project_root.join(".batty").join("test_timing.jsonl"),
            "{\"task_id\":1}\n",
        )
        .unwrap();
        std::fs::write(worktree_dir.join("session.jsonl"), "secret\n").unwrap();

        let export_dir = export_run(&project_root).unwrap();

        assert_eq!(
            std::fs::read_to_string(export_dir.join("team.yaml")).unwrap(),
            "name: demo\n"
        );
        assert_eq!(
            std::fs::read_to_string(export_dir.join("board").join("tasks").join("001-task.md"))
                .unwrap(),
            "---\nid: 1\n---\n"
        );
        assert_eq!(
            std::fs::read_to_string(export_dir.join("events.jsonl")).unwrap(),
            "{\"event\":\"daemon_started\"}\n"
        );
        assert_eq!(
            std::fs::read_to_string(export_dir.join("daemon.log")).unwrap(),
            "daemon-log\n"
        );
        assert_eq!(
            std::fs::read_to_string(export_dir.join("orchestrator.log")).unwrap(),
            "orchestrator-log\n"
        );
        assert_eq!(
            std::fs::read_to_string(export_dir.join("retrospectives").join("retro.md")).unwrap(),
            "# Retro\n"
        );
        assert_eq!(
            std::fs::read_to_string(export_dir.join("test_timing.jsonl")).unwrap(),
            "{\"task_id\":1}\n"
        );
        assert!(!export_dir.join("worktrees").exists());
        assert!(!export_dir.join(".codex").exists());
        assert!(!export_dir.join("sessions").exists());
    }

    #[test]
    fn export_run_skips_missing_optional_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("project");
        let config_dir = team_config_dir(&project_root);
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("team.yaml"), "name: demo\n").unwrap();

        let export_dir = export_run(&project_root).unwrap();

        assert!(export_dir.join("team.yaml").is_file());
        assert!(!export_dir.join("board").exists());
        assert!(!export_dir.join("events.jsonl").exists());
        assert!(!export_dir.join("daemon.log").exists());
        assert!(!export_dir.join("orchestrator.log").exists());
        assert!(!export_dir.join("retrospectives").exists());
        assert!(!export_dir.join("test_timing.jsonl").exists());
    }

    #[test]
    fn export_run_missing_team_yaml_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(team_config_dir(&project_root)).unwrap();

        let error = export_run(&project_root).unwrap_err();

        assert!(error.to_string().contains("team config missing"));
    }
}
