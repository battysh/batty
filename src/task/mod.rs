use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::config::Policy;

/// A parsed kanban-md task file.
#[derive(Debug)]
pub struct Task {
    pub id: u32,
    pub title: String,
    pub status: String,
    pub priority: String,
    pub tags: Vec<String>,
    pub depends_on: Vec<u32>,
    pub description: String,
    pub batty_config: Option<TaskBattyConfig>,
    pub source_path: PathBuf,
}

/// Per-task overrides from `## Batty Config` section.
#[derive(Debug, Deserialize, Default)]
pub struct TaskBattyConfig {
    pub agent: Option<String>,
    pub policy: Option<Policy>,
    pub dod: Option<String>,
    pub max_retries: Option<u32>,
}

/// Raw YAML frontmatter fields from a kanban-md task file.
#[derive(Debug, Deserialize)]
struct Frontmatter {
    id: u32,
    title: String,
    #[serde(default = "default_status")]
    status: String,
    #[serde(default)]
    priority: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    depends_on: Vec<u32>,
}

fn default_status() -> String {
    "backlog".to_string()
}

impl Task {
    /// Parse a kanban-md task file from a path.
    pub fn from_file(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read task file: {}", path.display()))?;
        let mut task = Self::parse(&contents)
            .with_context(|| format!("failed to parse task file: {}", path.display()))?;
        task.source_path = path.to_path_buf();
        Ok(task)
    }

    /// Parse a kanban-md task from its string content.
    pub fn parse(content: &str) -> Result<Self> {
        let (frontmatter_str, body) = split_frontmatter(content)?;

        let fm: Frontmatter = serde_yaml::from_str(frontmatter_str)
            .context("failed to parse YAML frontmatter")?;

        let (description, batty_config) = parse_body(body);

        Ok(Task {
            id: fm.id,
            title: fm.title,
            status: fm.status,
            priority: fm.priority,
            tags: fm.tags,
            depends_on: fm.depends_on,
            description,
            batty_config,
            source_path: PathBuf::new(),
        })
    }
}

/// Split content into YAML frontmatter and Markdown body.
fn split_frontmatter(content: &str) -> Result<(&str, &str)> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        bail!("task file missing YAML frontmatter (no opening ---)");
    }

    // Skip the opening "---\n"
    let after_open = &trimmed[3..];
    let after_open = after_open.strip_prefix('\n').unwrap_or(after_open);

    let close_pos = after_open
        .find("\n---")
        .context("task file missing closing --- for frontmatter")?;

    let frontmatter = &after_open[..close_pos];
    let body = &after_open[close_pos + 4..]; // skip "\n---"
    let body = body.strip_prefix('\n').unwrap_or(body);

    Ok((frontmatter, body))
}

/// Parse the Markdown body, extracting an optional `## Batty Config` section.
fn parse_body(body: &str) -> (String, Option<TaskBattyConfig>) {
    let marker = "## Batty Config";
    if let Some(pos) = body.find(marker) {
        let description = body[..pos].trim().to_string();
        let config_section = &body[pos + marker.len()..];

        // Find the TOML content after the heading (skip blank lines)
        let config_text = config_section.trim();

        // Try to parse as TOML (the natural config format for Batty)
        if let Ok(config) = toml::from_str::<TaskBattyConfig>(config_text) {
            return (description, Some(config));
        }

        // If there's a fenced code block, extract its content
        if let Some(start) = config_text.find("```") {
            let after_fence = &config_text[start + 3..];
            // Skip the language tag line (e.g., "toml\n")
            let inner_start = after_fence.find('\n').map(|i| i + 1).unwrap_or(0);
            let inner = &after_fence[inner_start..];
            if let Some(end) = inner.find("```") {
                let block = inner[..end].trim();
                if let Ok(config) = toml::from_str::<TaskBattyConfig>(block) {
                    return (description, Some(config));
                }
            }
        }

        (description, None)
    } else {
        (body.trim().to_string(), None)
    }
}

/// Load all task files from a kanban-md tasks directory.
pub fn load_tasks_from_dir(dir: &Path) -> Result<Vec<Task>> {
    let mut tasks = Vec::new();
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("failed to read tasks directory: {}", dir.display()))?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "md") {
            match Task::from_file(&path) {
                Ok(task) => tasks.push(task),
                Err(e) => {
                    tracing::warn!("skipping {}: {e:#}", path.display());
                }
            }
        }
    }

    tasks.sort_by_key(|t| t.id);
    Ok(tasks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parse_basic_task() {
        let content = r#"---
id: 3
title: kanban-md task file reader
status: backlog
priority: critical
tags:
    - core
depends_on:
    - 1
class: standard
---

Read task files from kanban/phase-N/tasks/ directory.
"#;
        let task = Task::parse(content).unwrap();
        assert_eq!(task.id, 3);
        assert_eq!(task.title, "kanban-md task file reader");
        assert_eq!(task.status, "backlog");
        assert_eq!(task.priority, "critical");
        assert_eq!(task.tags, vec!["core"]);
        assert_eq!(task.depends_on, vec![1]);
        assert!(task.description.contains("Read task files"));
        assert!(task.batty_config.is_none());
    }

    #[test]
    fn parse_task_with_batty_config_section() {
        let content = r#"---
id: 7
title: PTY supervision
status: backlog
priority: high
tags:
    - core
depends_on: []
class: standard
---

Implement the PTY supervision layer.

## Batty Config

agent = "codex"
policy = "act"
dod = "cargo test"
max_retries = 5
"#;
        let task = Task::parse(content).unwrap();
        assert_eq!(task.id, 7);
        assert!(task.description.contains("PTY supervision"));
        assert!(!task.description.contains("Batty Config"));

        let config = task.batty_config.unwrap();
        assert_eq!(config.agent.as_deref(), Some("codex"));
        assert_eq!(config.policy, Some(Policy::Act));
        assert_eq!(config.dod.as_deref(), Some("cargo test"));
        assert_eq!(config.max_retries, Some(5));
    }

    #[test]
    fn parse_task_with_fenced_batty_config() {
        let content = r#"---
id: 8
title: policy engine
status: backlog
priority: high
tags: []
depends_on: []
class: standard
---

Build the policy engine.

## Batty Config

```toml
agent = "aider"
dod = "make test"
```
"#;
        let task = Task::parse(content).unwrap();
        let config = task.batty_config.unwrap();
        assert_eq!(config.agent.as_deref(), Some("aider"));
        assert_eq!(config.dod.as_deref(), Some("make test"));
    }

    #[test]
    fn parse_task_no_depends() {
        let content = r#"---
id: 1
title: scaffolding
status: done
priority: critical
tags:
    - core
class: standard
---

Set up the project.
"#;
        let task = Task::parse(content).unwrap();
        assert_eq!(task.id, 1);
        assert!(task.depends_on.is_empty());
    }

    #[test]
    fn parse_task_minimal_frontmatter() {
        let content = r#"---
id: 99
title: minimal task
---

Just a description.
"#;
        let task = Task::parse(content).unwrap();
        assert_eq!(task.id, 99);
        assert_eq!(task.status, "backlog");
        assert!(task.priority.is_empty());
        assert!(task.tags.is_empty());
    }

    #[test]
    fn missing_frontmatter_is_error() {
        let content = "# No frontmatter here\nJust markdown.";
        assert!(Task::parse(content).is_err());
    }

    #[test]
    fn load_from_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path();

        fs::write(
            tasks_dir.join("001-first.md"),
            r#"---
id: 1
title: first task
status: backlog
priority: high
tags: []
depends_on: []
class: standard
---

First task description.
"#,
        )
        .unwrap();

        fs::write(
            tasks_dir.join("002-second.md"),
            r#"---
id: 2
title: second task
status: todo
priority: medium
tags: []
depends_on:
    - 1
class: standard
---

Second task description.
"#,
        )
        .unwrap();

        // Non-markdown file should be skipped
        fs::write(tasks_dir.join("notes.txt"), "not a task").unwrap();

        let tasks = load_tasks_from_dir(tasks_dir).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].id, 1);
        assert_eq!(tasks[1].id, 2);
        assert_eq!(tasks[1].depends_on, vec![1]);
    }

    #[test]
    fn load_real_phase1_tasks() {
        let phase1_dir = Path::new("kanban/phase-1/tasks");
        if !phase1_dir.exists() {
            return; // skip if not in repo root
        }
        let tasks = load_tasks_from_dir(phase1_dir).unwrap();
        assert!(!tasks.is_empty());
        // Task #1 should exist and be done
        let task1 = tasks.iter().find(|t| t.id == 1).unwrap();
        assert_eq!(task1.title, "Rust project scaffolding");
    }
}
