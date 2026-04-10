use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer};
use serde_yaml::{Mapping, Value};
use std::path::{Path, PathBuf};

use crate::config::Policy;

/// Accept either a string reason or a boolean flag for the `blocked` field.
///
/// kanban-md writes `blocked: true` alongside a separate `block_reason` string,
/// while legacy batty tasks stored `blocked: "reason"` directly. This
/// deserializer normalizes both shapes into `Option<String>` so downstream
/// callers can still rely on `task.blocked.is_some()` as "is this blocked".
fn deserialize_blocked_field<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum BlockedField {
        String(String),
        Bool(bool),
    }

    let raw: Option<BlockedField> = Option::deserialize(deserializer)?;
    Ok(match raw {
        Some(BlockedField::String(s)) => Some(s),
        Some(BlockedField::Bool(true)) => Some("blocked".to_string()),
        Some(BlockedField::Bool(false)) => None,
        None => None,
    })
}

/// A parsed kanban-md task file.
#[derive(Debug)]
pub struct Task {
    pub id: u32,
    pub title: String,
    pub status: String,
    pub priority: String,
    pub claimed_by: Option<String>,
    pub claimed_at: Option<String>,
    pub claim_ttl_secs: Option<u64>,
    pub claim_expires_at: Option<String>,
    pub last_progress_at: Option<String>,
    pub claim_warning_sent_at: Option<String>,
    pub claim_extensions: Option<u32>,
    pub last_output_bytes: Option<u64>,
    pub blocked: Option<String>,
    pub tags: Vec<String>,
    pub depends_on: Vec<u32>,
    pub review_owner: Option<String>,
    pub blocked_on: Option<String>,
    pub worktree_path: Option<String>,
    pub branch: Option<String>,
    pub commit: Option<String>,
    pub artifacts: Vec<String>,
    pub next_action: Option<String>,
    pub scheduled_for: Option<String>,
    pub cron_schedule: Option<String>,
    pub cron_last_run: Option<String>,
    pub completed: Option<String>,
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
    claimed_by: Option<String>,
    #[serde(default)]
    claimed_at: Option<String>,
    #[serde(default)]
    claim_ttl_secs: Option<u64>,
    #[serde(default)]
    claim_expires_at: Option<String>,
    #[serde(default)]
    last_progress_at: Option<String>,
    #[serde(default)]
    claim_warning_sent_at: Option<String>,
    #[serde(default)]
    claim_extensions: Option<u32>,
    #[serde(default)]
    last_output_bytes: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_blocked_field")]
    blocked: Option<String>,
    #[serde(default)]
    block_reason: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    depends_on: Vec<u32>,
    #[serde(default)]
    review_owner: Option<String>,
    #[serde(default)]
    blocked_on: Option<String>,
    #[serde(default)]
    worktree_path: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    commit: Option<String>,
    #[serde(default)]
    artifacts: Vec<String>,
    #[serde(default)]
    next_action: Option<String>,
    #[serde(default)]
    scheduled_for: Option<String>,
    #[serde(default)]
    cron_schedule: Option<String>,
    #[serde(default)]
    cron_last_run: Option<String>,
    #[serde(default)]
    completed: Option<String>,
}

fn default_status() -> String {
    "backlog".to_string()
}

impl Task {
    /// Returns true if this task has a `scheduled_for` timestamp in the future.
    pub fn is_schedule_blocked(&self) -> bool {
        self.scheduled_for.as_ref().is_some_and(|scheduled| {
            chrono::DateTime::parse_from_rfc3339(scheduled).is_ok_and(|ts| ts > chrono::Utc::now())
        })
    }

    /// Parse a kanban-md task file from a path.
    pub fn from_file(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read task file: {}", path.display()))?;
        let normalized = normalize_blocked_frontmatter_content(&contents)?;
        let contents = match normalized {
            Some(updated) => {
                std::fs::write(path, &updated)
                    .with_context(|| format!("failed to repair task file: {}", path.display()))?;
                updated
            }
            None => contents,
        };
        let mut task = Self::parse(&contents)
            .with_context(|| format!("failed to parse task file: {}", path.display()))?;
        task.source_path = path.to_path_buf();
        Ok(task)
    }

    /// Parse a kanban-md task from its string content.
    pub fn parse(content: &str) -> Result<Self> {
        let (frontmatter_str, body) = split_frontmatter(content)?;

        let fm: Frontmatter =
            serde_yaml::from_str(frontmatter_str).context("failed to parse YAML frontmatter")?;

        let (description, batty_config) = parse_body(body);

        Ok(Task {
            id: fm.id,
            title: fm.title,
            status: fm.status,
            priority: fm.priority,
            claimed_by: fm.claimed_by,
            claimed_at: fm.claimed_at,
            claim_ttl_secs: fm.claim_ttl_secs,
            claim_expires_at: fm.claim_expires_at,
            last_progress_at: fm.last_progress_at,
            claim_warning_sent_at: fm.claim_warning_sent_at,
            claim_extensions: fm.claim_extensions,
            last_output_bytes: fm.last_output_bytes,
            // Prefer the richer `block_reason` if present so operators see
            // the real reason, not the "blocked" placeholder from `blocked: true`.
            blocked: fm
                .block_reason
                .or(fm.blocked)
                .or_else(|| fm.blocked_on.clone()),
            tags: fm.tags,
            depends_on: fm.depends_on,
            review_owner: fm.review_owner,
            blocked_on: fm.blocked_on,
            worktree_path: fm.worktree_path,
            branch: fm.branch,
            commit: fm.commit,
            artifacts: fm.artifacts,
            next_action: fm.next_action,
            scheduled_for: fm.scheduled_for,
            cron_schedule: fm.cron_schedule,
            cron_last_run: fm.cron_last_run,
            completed: fm.completed,
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

fn yaml_key(key: &str) -> Value {
    Value::String(key.to_string())
}

fn clear_blocked(mapping: &mut Mapping) {
    mapping.remove(yaml_key("blocked"));
    mapping.remove(yaml_key("block_reason"));
    mapping.remove(yaml_key("blocked_on"));
}

fn set_optional_string(mapping: &mut Mapping, key: &str, value: Option<&str>) {
    let key = yaml_key(key);
    match value {
        Some(value) => {
            mapping.insert(key, Value::String(value.to_string()));
        }
        None => {
            mapping.remove(key);
        }
    }
}

fn set_blocked_reason(mapping: &mut Mapping, reason: Option<&str>, blocked_on: Option<&str>) {
    if reason.is_none() && blocked_on.is_none() {
        clear_blocked(mapping);
        return;
    }

    mapping.insert(yaml_key("blocked"), Value::Bool(true));
    set_optional_string(mapping, "block_reason", reason);
    set_optional_string(mapping, "blocked_on", blocked_on.or(reason));
}

fn normalize_blocked_frontmatter_content(content: &str) -> Result<Option<String>> {
    let (frontmatter, body) = split_frontmatter(content)?;
    let mut mapping: Mapping =
        serde_yaml::from_str(frontmatter).context("failed to parse YAML frontmatter")?;

    let blocked_value = mapping.get(yaml_key("blocked")).cloned();
    let block_reason = mapping
        .get(yaml_key("block_reason"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let blocked_on = mapping
        .get(yaml_key("blocked_on"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let status_is_blocked = mapping
        .get(yaml_key("status"))
        .and_then(Value::as_str)
        .is_some_and(|status| status == "blocked");

    let rewrites_hidden_string_block = matches!(
        blocked_value.as_ref(),
        Some(Value::String(reason)) if !reason.trim().is_empty()
    );
    let legacy_reason = match blocked_value.as_ref() {
        Some(Value::String(reason)) if !reason.trim().is_empty() => Some(reason.as_str()),
        Some(Value::Bool(true)) => block_reason.as_deref().or(blocked_on.as_deref()),
        Some(Value::Bool(false)) => None,
        _ => block_reason.as_deref().or(blocked_on.as_deref()),
    };

    let desired_reason = legacy_reason.as_deref();
    let desired_blocked_on = blocked_on.as_deref().or(desired_reason).map(str::to_string);
    let rewrites_incomplete_blocked_task = status_is_blocked && legacy_reason.is_some();
    let rewrites_incomplete_bool_shape = matches!(blocked_value, Some(Value::Bool(true)))
        && (block_reason.as_deref() != desired_reason
            || mapping.get(yaml_key("blocked_on")).and_then(Value::as_str)
                != desired_blocked_on.as_deref());

    if !rewrites_hidden_string_block
        && !rewrites_incomplete_blocked_task
        && !rewrites_incomplete_bool_shape
    {
        return Ok(None);
    }

    set_blocked_reason(&mut mapping, desired_reason, desired_blocked_on.as_deref());

    let mut rendered =
        serde_yaml::to_string(&mapping).context("failed to serialize task frontmatter")?;
    if let Some(stripped) = rendered.strip_prefix("---\n") {
        rendered = stripped.to_string();
    }

    let mut updated = String::from("---\n");
    updated.push_str(&rendered);
    if !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str("---\n");
    updated.push_str(body);
    Ok(Some(updated))
}

pub(crate) fn normalize_blocked_frontmatter(task_path: &Path) -> Result<bool> {
    let contents = std::fs::read_to_string(task_path)
        .with_context(|| format!("failed to read task file: {}", task_path.display()))?;
    let Some(updated) = normalize_blocked_frontmatter_content(&contents)? else {
        return Ok(false);
    };
    std::fs::write(task_path, updated)
        .with_context(|| format!("failed to repair task file: {}", task_path.display()))?;
    Ok(true)
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

fn task_id_from_filename(path: &Path) -> Option<u32> {
    let name = path.file_name()?.to_str()?;
    if !name.ends_with(".md") {
        return None;
    }
    name.split('-').next()?.parse::<u32>().ok()
}

pub fn find_task_path_by_id(tasks_dir: &Path, task_id: u32) -> Result<PathBuf> {
    let entries = std::fs::read_dir(tasks_dir)
        .with_context(|| format!("failed to read tasks directory: {}", tasks_dir.display()))?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if task_id_from_filename(&path) == Some(task_id) {
            return Ok(path);
        }
    }

    load_tasks_from_dir(tasks_dir)?
        .into_iter()
        .find(|task| task.id == task_id)
        .map(|task| task.source_path)
        .with_context(|| format!("task #{task_id} not found in {}", tasks_dir.display()))
}

pub fn load_task_by_id(tasks_dir: &Path, task_id: u32) -> Result<Task> {
    let path = find_task_path_by_id(tasks_dir, task_id)?;
    Task::from_file(&path)
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
        assert!(task.claimed_by.is_none());
        assert!(task.blocked.is_none());
        assert_eq!(task.tags, vec!["core"]);
        assert_eq!(task.depends_on, vec![1]);
        assert!(task.review_owner.is_none());
        assert!(task.blocked_on.is_none());
        assert!(task.worktree_path.is_none());
        assert!(task.branch.is_none());
        assert!(task.commit.is_none());
        assert!(task.artifacts.is_empty());
        assert!(task.next_action.is_none());
        assert!(task.description.contains("Read task files"));
        assert!(task.batty_config.is_none());
    }

    #[test]
    fn parse_task_with_kanban_md_block_flag_uses_block_reason() {
        // kanban-md writes `blocked: true` + a separate `block_reason` string.
        // Before the untagged deserializer, `blocked: true` failed to parse
        // into Option<String> and silently became None, so dispatch treated
        // the task as runnable and auto-assigned it to benched engineers.
        let content = r#"---
id: 42
title: kanban-md-style blocked task
status: todo
priority: high
blocked: true
block_reason: "Deferred per architect"
---

Body.
"#;
        let task = Task::parse(content).unwrap();
        assert_eq!(
            task.blocked.as_deref(),
            Some("Deferred per architect"),
            "block_reason must be surfaced as the blocked reason"
        );
    }

    #[test]
    fn parse_task_with_bool_blocked_only() {
        // If `blocked: true` arrives without a block_reason, fall back to a
        // placeholder string so `task.blocked.is_some()` still short-circuits
        // the dispatch filter.
        let content = r#"---
id: 43
title: blocked without reason
status: todo
priority: high
blocked: true
---

Body.
"#;
        let task = Task::parse(content).unwrap();
        assert!(
            task.blocked.is_some(),
            "blocked: true must produce a Some(...) value"
        );
    }

    #[test]
    fn parse_task_with_blocked_on_only_uses_human_reason() {
        let content = r#"---
id: 430
title: blocked via blocked_on only
status: blocked
priority: high
blocked_on: waiting-for-review
---

Body.
"#;
        let task = Task::parse(content).unwrap();
        assert_eq!(task.blocked.as_deref(), Some("waiting-for-review"));
        assert_eq!(task.blocked_on.as_deref(), Some("waiting-for-review"));
    }

    #[test]
    fn load_tasks_from_dir_repairs_blocked_on_only_shape_to_canonical_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        let task_path = tasks_dir.join("430-blocked.md");
        fs::write(
            &task_path,
            "---\nid: 430\ntitle: blocked via blocked_on only\nstatus: blocked\npriority: high\nblocked_on: waiting-for-review\n---\n\nBody.\n",
        )
        .unwrap();

        let tasks = load_tasks_from_dir(&tasks_dir).unwrap();

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].blocked.as_deref(), Some("waiting-for-review"));
        let content = fs::read_to_string(&task_path).unwrap();
        assert!(content.contains("blocked: true"));
        assert!(content.contains("block_reason: waiting-for-review"));
        assert!(content.contains("blocked_on: waiting-for-review"));
    }

    #[test]
    fn parse_task_with_legacy_string_blocked() {
        // Older batty tasks stored the reason directly in `blocked`. That
        // shape must still parse cleanly so historical archives do not rot.
        let content = r#"---
id: 44
title: legacy blocked task
status: todo
priority: high
blocked: "legacy reason string"
---

Body.
"#;
        let task = Task::parse(content).unwrap();
        assert_eq!(task.blocked.as_deref(), Some("legacy reason string"));
    }

    #[test]
    fn parse_task_with_blocked_false_is_not_blocked() {
        let content = r#"---
id: 45
title: explicitly unblocked
status: todo
priority: high
blocked: false
---

Body.
"#;
        let task = Task::parse(content).unwrap();
        assert!(task.blocked.is_none());
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
        assert!(task.claimed_by.is_none());
        assert!(task.blocked.is_none());
        assert!(task.tags.is_empty());
        assert!(task.depends_on.is_empty());
        assert!(task.review_owner.is_none());
        assert!(task.blocked_on.is_none());
        assert!(task.worktree_path.is_none());
        assert!(task.branch.is_none());
        assert!(task.commit.is_none());
        assert!(task.artifacts.is_empty());
        assert!(task.next_action.is_none());
    }

    #[test]
    fn parse_task_without_workflow_metadata_uses_safe_defaults() {
        let content = r#"---
id: 100
title: legacy task
priority: high
class: standard
---

Older task file without workflow metadata.
"#;
        let task = Task::parse(content).unwrap();
        assert_eq!(task.id, 100);
        assert_eq!(task.status, "backlog");
        assert!(task.depends_on.is_empty());
        assert!(task.batty_config.is_none());
    }

    #[test]
    fn parse_task_ignores_future_workflow_frontmatter_fields() {
        let content = r#"---
id: 101
title: workflow task
status: todo
priority: high
workflow_state: in_review
workflow_owner: architect
class: standard
---

Task description.
"#;
        let task = Task::parse(content).unwrap();
        assert_eq!(task.id, 101);
        assert_eq!(task.status, "todo");
        assert_eq!(task.priority, "high");
        assert!(task.batty_config.is_none());
    }

    #[test]
    fn parse_task_with_claimed_by_and_blocked() {
        let content = r#"---
id: 17
title: assigned task
status: todo
priority: high
claimed_by: eng-1-1
blocked: waiting-on-review
class: standard
---

Task description.
"#;
        let task = Task::parse(content).unwrap();
        assert_eq!(task.claimed_by.as_deref(), Some("eng-1-1"));
        assert_eq!(task.blocked.as_deref(), Some("waiting-on-review"));
    }

    #[test]
    fn parse_task_with_workflow_metadata() {
        let content = r#"---
id: 20
title: workflow metadata
status: review
priority: critical
claimed_by: eng-1-3
depends_on:
    - 18
    - 19
review_owner: manager
blocked_on: waiting-for-tests
worktree_path: .batty/worktrees/eng-1-3
branch: eng-1-3/task-20
commit: abc1234
artifacts:
    - target/debug/batty
    - docs/workflow.md
next_action: Hand off to manager for review
class: standard
---

Workflow description.
"#;
        let task = Task::parse(content).unwrap();
        assert_eq!(task.depends_on, vec![18, 19]);
        assert_eq!(task.review_owner.as_deref(), Some("manager"));
        assert_eq!(task.blocked_on.as_deref(), Some("waiting-for-tests"));
        assert_eq!(
            task.worktree_path.as_deref(),
            Some(".batty/worktrees/eng-1-3")
        );
        assert_eq!(task.branch.as_deref(), Some("eng-1-3/task-20"));
        assert_eq!(task.commit.as_deref(), Some("abc1234"));
        assert_eq!(
            task.artifacts,
            vec!["target/debug/batty", "docs/workflow.md"]
        );
        assert_eq!(
            task.next_action.as_deref(),
            Some("Hand off to manager for review")
        );
    }

    #[test]
    fn parse_task_with_all_schedule_fields() {
        let content = r#"---
id: 200
title: scheduled task
status: backlog
priority: medium
scheduled_for: "2026-04-01T09:00:00Z"
cron_schedule: "0 9 * * 1"
cron_last_run: "2026-03-21T09:00:00Z"
---

A task with all schedule fields.
"#;
        let task = Task::parse(content).unwrap();
        assert_eq!(task.id, 200);
        assert_eq!(task.scheduled_for.as_deref(), Some("2026-04-01T09:00:00Z"));
        assert_eq!(task.cron_schedule.as_deref(), Some("0 9 * * 1"));
        assert_eq!(task.cron_last_run.as_deref(), Some("2026-03-21T09:00:00Z"));
    }

    #[test]
    fn parse_task_with_no_schedule_fields() {
        let content = r#"---
id: 201
title: no schedule
status: todo
---

No schedule fields at all.
"#;
        let task = Task::parse(content).unwrap();
        assert_eq!(task.id, 201);
        assert!(task.scheduled_for.is_none());
        assert!(task.cron_schedule.is_none());
        assert!(task.cron_last_run.is_none());
    }

    #[test]
    fn parse_task_with_only_scheduled_for() {
        let content = r#"---
id: 202
title: future task
status: backlog
scheduled_for: "2026-06-15T12:00:00Z"
---

Only scheduled_for set.
"#;
        let task = Task::parse(content).unwrap();
        assert_eq!(task.scheduled_for.as_deref(), Some("2026-06-15T12:00:00Z"));
        assert!(task.cron_schedule.is_none());
        assert!(task.cron_last_run.is_none());
    }

    #[test]
    fn parse_task_with_only_cron_schedule() {
        let content = r#"---
id: 203
title: recurring task
status: backlog
cron_schedule: "30 8 * * *"
---

Only cron_schedule set.
"#;
        let task = Task::parse(content).unwrap();
        assert!(task.scheduled_for.is_none());
        assert_eq!(task.cron_schedule.as_deref(), Some("30 8 * * *"));
        assert!(task.cron_last_run.is_none());
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

    #[test]
    fn is_schedule_blocked_future_returns_true() {
        let future = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        let content = format!(
            "---\nid: 300\ntitle: future task\nstatus: todo\nscheduled_for: \"{future}\"\n---\n\nDesc.\n"
        );
        let task = Task::parse(&content).unwrap();
        assert!(task.is_schedule_blocked());
    }

    #[test]
    fn is_schedule_blocked_past_returns_false() {
        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let content = format!(
            "---\nid: 301\ntitle: past task\nstatus: todo\nscheduled_for: \"{past}\"\n---\n\nDesc.\n"
        );
        let task = Task::parse(&content).unwrap();
        assert!(!task.is_schedule_blocked());
    }

    #[test]
    fn is_schedule_blocked_absent_returns_false() {
        let content = "---\nid: 302\ntitle: no schedule\nstatus: todo\n---\n\nDesc.\n";
        let task = Task::parse(content).unwrap();
        assert!(!task.is_schedule_blocked());
    }

    #[test]
    fn is_schedule_blocked_malformed_returns_false() {
        let content = "---\nid: 303\ntitle: bad date\nstatus: todo\nscheduled_for: \"not-a-date\"\n---\n\nDesc.\n";
        let task = Task::parse(content).unwrap();
        assert!(!task.is_schedule_blocked());
    }

    #[test]
    fn find_task_path_by_id_handles_slug_rename() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        let renamed = tasks_dir.join("511-renamed-roadmap-item.md");
        fs::write(
            &renamed,
            "---\nid: 511\ntitle: roadmap task renamed\nstatus: todo\npriority: high\nclass: standard\n---\n\nBody.\n",
        )
        .unwrap();

        assert_eq!(find_task_path_by_id(&tasks_dir, 511).unwrap(), renamed);
    }

    #[test]
    fn find_task_path_by_id_uses_unchanged_prefix_fast_path() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        let stable = tasks_dir.join("042-stable-path.md");
        fs::write(&stable, "not valid yaml").unwrap();

        assert_eq!(find_task_path_by_id(&tasks_dir, 42).unwrap(), stable);
    }

    #[test]
    fn find_task_path_by_id_reports_missing_id() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        fs::write(
            tasks_dir.join("001-existing.md"),
            "---\nid: 1\ntitle: existing\nstatus: todo\npriority: high\nclass: standard\n---\n\nBody.\n",
        )
        .unwrap();

        let error = find_task_path_by_id(&tasks_dir, 999).unwrap_err();
        assert!(error.to_string().contains("task #999 not found"));
    }
}
