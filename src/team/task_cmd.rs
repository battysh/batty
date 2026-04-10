use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde_yaml::{Mapping, Value};

use crate::task::Task;

use super::board::{read_workflow_metadata, write_workflow_metadata};
use super::workflow::{ReviewDisposition, TaskState, can_transition};

pub fn cmd_transition(board_dir: &Path, task_id: u32, target: &str) -> Result<()> {
    transition_task(board_dir, task_id, target)?;
    println!("Task #{task_id} transitioned to {}.", target.trim());
    Ok(())
}

pub(crate) fn transition_task(board_dir: &Path, task_id: u32, target: &str) -> Result<()> {
    let task_path = find_task_path(board_dir, task_id)?;
    let task = Task::from_file(&task_path)?;
    let current = parse_task_state(&task.status)?;
    let target = parse_task_state(target)?;

    can_transition(current, target).map_err(anyhow::Error::msg)?;

    update_task_frontmatter(&task_path, |mapping| {
        set_status(mapping, target);
        if target != TaskState::Blocked {
            clear_blocked(mapping);
        }
    })?;
    Ok(())
}

pub fn cmd_assign(
    board_dir: &Path,
    task_id: u32,
    exec_owner: Option<&str>,
    review_owner: Option<&str>,
) -> Result<()> {
    if exec_owner.is_none() && review_owner.is_none() {
        bail!("at least one owner must be provided");
    }

    assign_task_owners(board_dir, task_id, exec_owner, review_owner)?;

    println!("Task #{task_id} ownership updated.");
    Ok(())
}

pub(crate) fn assign_task_owners(
    board_dir: &Path,
    task_id: u32,
    exec_owner: Option<&str>,
    review_owner: Option<&str>,
) -> Result<()> {
    if exec_owner.is_none() && review_owner.is_none() {
        bail!("at least one owner must be provided");
    }

    let task_path = find_task_path(board_dir, task_id)?;
    update_task_frontmatter(&task_path, |mapping| {
        if let Some(owner) = exec_owner {
            set_optional_string(mapping, "claimed_by", normalize_optional(owner));
            if normalize_optional(owner).is_some() {
                let now = Utc::now();
                set_optional_string(mapping, "claimed_at", Some(&now.to_rfc3339()));
            }
        }
        if let Some(owner) = review_owner {
            set_optional_string(mapping, "review_owner", normalize_optional(owner));
        }
    })?;
    Ok(())
}

/// Remove the claimed_by and review_owner fields from a task.
pub(crate) fn unclaim_task(board_dir: &Path, task_id: u32) -> Result<()> {
    let task_path = find_task_path(board_dir, task_id)?;
    update_task_frontmatter(&task_path, |mapping| {
        set_optional_string(mapping, "claimed_by", None);
        set_optional_string(mapping, "review_owner", None);
        set_optional_string(mapping, "claimed_at", None);
        set_optional_u64(mapping, "claim_ttl_secs", None);
        set_optional_string(mapping, "claim_expires_at", None);
        set_optional_string(mapping, "last_progress_at", None);
        set_optional_string(mapping, "claim_warning_sent_at", None);
        set_optional_u32(mapping, "claim_extensions", None);
        set_optional_u64(mapping, "last_output_bytes", None);
    })?;
    Ok(())
}

/// Release engineer ownership while preserving downstream review/block metadata.
pub(crate) fn release_engineer_claim(board_dir: &Path, task_id: u32) -> Result<()> {
    let task_path = find_task_path(board_dir, task_id)?;
    update_task_frontmatter(&task_path, |mapping| {
        set_optional_string(mapping, "claimed_by", None);
        set_optional_string(mapping, "claimed_at", None);
        set_optional_u64(mapping, "claim_ttl_secs", None);
        set_optional_string(mapping, "claim_expires_at", None);
        set_optional_string(mapping, "last_progress_at", None);
        set_optional_string(mapping, "claim_warning_sent_at", None);
        set_optional_u32(mapping, "claim_extensions", None);
        set_optional_u64(mapping, "last_output_bytes", None);
    })?;
    Ok(())
}

pub(crate) fn initialize_task_claim(
    board_dir: &Path,
    task_id: u32,
    ttl_secs: u64,
    now: DateTime<Utc>,
    output_bytes: u64,
) -> Result<()> {
    let task_path = find_task_path(board_dir, task_id)?;
    let expires_at = now + ChronoDuration::seconds(ttl_secs as i64);
    update_task_frontmatter(&task_path, |mapping| {
        set_optional_string(mapping, "claimed_at", Some(&now.to_rfc3339()));
        set_optional_u64(mapping, "claim_ttl_secs", Some(ttl_secs));
        set_optional_string(mapping, "claim_expires_at", Some(&expires_at.to_rfc3339()));
        set_optional_string(mapping, "last_progress_at", Some(&now.to_rfc3339()));
        set_optional_string(mapping, "claim_warning_sent_at", None);
        set_optional_u32(mapping, "claim_extensions", Some(0));
        set_optional_u64(mapping, "last_output_bytes", Some(output_bytes));
    })?;
    Ok(())
}

pub(crate) fn refresh_task_claim_progress(
    board_dir: &Path,
    task_id: u32,
    ttl_secs: u64,
    now: DateTime<Utc>,
    output_bytes: u64,
    extensions: u32,
) -> Result<()> {
    let task_path = find_task_path(board_dir, task_id)?;
    let expires_at = now + ChronoDuration::seconds(ttl_secs as i64);
    update_task_frontmatter(&task_path, |mapping| {
        set_optional_u64(mapping, "claim_ttl_secs", Some(ttl_secs));
        set_optional_string(mapping, "claim_expires_at", Some(&expires_at.to_rfc3339()));
        set_optional_string(mapping, "last_progress_at", Some(&now.to_rfc3339()));
        set_optional_string(mapping, "claim_warning_sent_at", None);
        set_optional_u32(mapping, "claim_extensions", Some(extensions));
        set_optional_u64(mapping, "last_output_bytes", Some(output_bytes));
    })?;
    Ok(())
}

pub(crate) fn mark_task_claim_warning(
    board_dir: &Path,
    task_id: u32,
    now: DateTime<Utc>,
) -> Result<()> {
    let task_path = find_task_path(board_dir, task_id)?;
    update_task_frontmatter(&task_path, |mapping| {
        set_optional_string(mapping, "claim_warning_sent_at", Some(&now.to_rfc3339()));
    })?;
    Ok(())
}

pub(crate) fn reclaim_task_claim(board_dir: &Path, task_id: u32, next_action: &str) -> Result<()> {
    let task_path = find_task_path(board_dir, task_id)?;
    update_task_frontmatter(&task_path, |mapping| {
        set_optional_string(mapping, "claimed_by", None);
        set_optional_string(mapping, "review_owner", None);
        set_optional_string(mapping, "claimed_at", None);
        set_optional_u64(mapping, "claim_ttl_secs", None);
        set_optional_string(mapping, "claim_expires_at", None);
        set_optional_string(mapping, "last_progress_at", None);
        set_optional_string(mapping, "claim_warning_sent_at", None);
        set_optional_u32(mapping, "claim_extensions", None);
        set_optional_u64(mapping, "last_output_bytes", None);
        set_optional_string(mapping, "next_action", Some(next_action));
        set_status(mapping, TaskState::Todo);
    })?;
    Ok(())
}

pub(crate) fn append_task_dependencies(
    board_dir: &Path,
    task_id: u32,
    dependency_ids: &[u32],
) -> Result<Vec<u32>> {
    let task_path = find_task_path(board_dir, task_id)?;
    let mut merged = Vec::new();
    update_task_frontmatter(&task_path, |mapping| {
        let key = yaml_key("depends_on");
        let mut deps = BTreeSet::new();
        if let Some(Value::Sequence(existing)) = mapping.get(&key) {
            for value in existing {
                if let Some(dep_id) = value.as_u64() {
                    deps.insert(dep_id as u32);
                }
            }
        }
        deps.extend(dependency_ids.iter().copied());
        merged = deps.iter().copied().collect();
        if merged.is_empty() {
            mapping.remove(key);
        } else {
            mapping.insert(
                key,
                Value::Sequence(
                    merged
                        .iter()
                        .map(|dep_id| Value::Number((*dep_id as u64).into()))
                        .collect(),
                ),
            );
        }
    })?;
    Ok(merged)
}

pub fn cmd_review(
    board_dir: &Path,
    task_id: u32,
    disposition: &str,
    feedback: Option<&str>,
) -> Result<()> {
    let task_path = find_task_path(board_dir, task_id)?;
    let task = Task::from_file(&task_path)?;
    let current = parse_task_state(&task.status)?;
    let disposition = parse_review_disposition(disposition)?;
    let target = match disposition {
        ReviewDisposition::Approved => TaskState::Done,
        ReviewDisposition::ChangesRequested => TaskState::InProgress,
        ReviewDisposition::Rejected => TaskState::Archived,
    };

    can_transition(current, target).map_err(anyhow::Error::msg)?;

    update_task_frontmatter(&task_path, |mapping| {
        set_status(mapping, target);
        clear_blocked(mapping);
        if let Some(text) = feedback {
            set_optional_string(mapping, "review_feedback", Some(text));
        }
    })?;

    let mut metadata = read_workflow_metadata(&task_path)?;
    metadata.outcome = Some(review_disposition_name(disposition).to_string());
    if disposition == ReviewDisposition::Approved {
        metadata.review_blockers.clear();
    }
    write_workflow_metadata(&task_path, &metadata)?;

    if disposition == ReviewDisposition::ChangesRequested {
        if let Some(text) = feedback {
            // Deliver feedback to the engineer's inbox.
            // board_dir is <project_root>/.batty/team_config/board
            if let Some(engineer) = task.claimed_by.as_deref() {
                if let Some(project_root) = board_dir
                    .parent() // team_config
                    .and_then(|p| p.parent()) // .batty
                    .and_then(|p| p.parent())
                // project_root
                {
                    let inbox_root = super::inbox::inboxes_root(project_root);
                    if let Ok(()) = queue_review_feedback(&inbox_root, engineer, task_id, text) {
                        println!("Review feedback delivered to {engineer}'s inbox.");
                    }
                }
            }
        }
    }

    println!(
        "Task #{task_id} review recorded as {}.",
        review_disposition_name(disposition)
    );
    Ok(())
}

fn queue_review_feedback(
    inbox_root: &Path,
    engineer: &str,
    task_id: u32,
    feedback: &str,
) -> Result<()> {
    use super::inbox;
    let message = format!("Review feedback for task #{task_id}: {feedback}");
    let msg = inbox::InboxMessage::new_send("reviewer", engineer, &message);
    inbox::deliver_to_inbox(inbox_root, &msg)?;
    Ok(())
}

/// Structured review: stores review_disposition, review_feedback, reviewed_by,
/// reviewed_at in task frontmatter and applies the correct state transition.
///
/// Disposition mapping:
///   approve         → Done
///   request-changes → InProgress (feedback delivered to engineer inbox)
///   reject          → Blocked (reason stored in blocked_on)
pub fn cmd_review_structured(
    board_dir: &Path,
    task_id: u32,
    disposition: &str,
    feedback: Option<&str>,
    reviewer: &str,
) -> Result<()> {
    let task_path = find_task_path(board_dir, task_id)?;
    let task = Task::from_file(&task_path)?;
    let current = parse_task_state(&task.status)?;

    let (target_state, disposition_str) = match disposition {
        "approve" => (TaskState::Done, "approved"),
        "request-changes" | "request_changes" => (TaskState::InProgress, "changes_requested"),
        "reject" => (TaskState::Blocked, "rejected"),
        other => bail!("unknown review disposition: {other}"),
    };

    can_transition(current, target_state).map_err(anyhow::Error::msg)?;

    let now = chrono::Utc::now().to_rfc3339();
    let default_reject_reason = format!("rejected by {reviewer}");

    update_task_frontmatter(&task_path, |mapping| {
        set_status(mapping, target_state);
        set_optional_string(mapping, "review_disposition", Some(disposition_str));
        set_optional_string(mapping, "reviewed_by", Some(reviewer));
        set_optional_string(mapping, "reviewed_at", Some(&now));
        if let Some(text) = feedback {
            set_optional_string(mapping, "review_feedback", Some(text));
        }
        if target_state == TaskState::Blocked {
            let reason = feedback.unwrap_or(&default_reject_reason);
            set_blocked_reason(mapping, Some(reason), Some(reason));
        } else {
            clear_blocked(mapping);
        }
    })?;

    // Update workflow metadata outcome
    let mut metadata = read_workflow_metadata(&task_path)?;
    metadata.outcome = Some(disposition_str.to_string());
    if disposition == "approve" {
        metadata.review_blockers.clear();
    }
    write_workflow_metadata(&task_path, &metadata)?;

    // Deliver feedback to engineer inbox on request-changes
    if disposition == "request-changes" || disposition == "request_changes" {
        if let Some(text) = feedback {
            if let Some(engineer) = task.claimed_by.as_deref() {
                if let Some(project_root) = board_dir
                    .parent()
                    .and_then(|p| p.parent())
                    .and_then(|p| p.parent())
                {
                    let inbox_root = super::inbox::inboxes_root(project_root);
                    if let Ok(()) = queue_review_feedback(&inbox_root, engineer, task_id, text) {
                        println!("Review feedback delivered to {engineer}'s inbox.");
                    }
                }
            }
        }
    }

    println!("Task #{task_id} review recorded as {disposition_str} by {reviewer}.");
    Ok(())
}

pub fn cmd_update(board_dir: &Path, task_id: u32, fields: HashMap<String, String>) -> Result<()> {
    if fields.is_empty() {
        bail!("no workflow fields provided");
    }

    let task_path = find_task_path(board_dir, task_id)?;
    let mut metadata = read_workflow_metadata(&task_path)?;
    let mut metadata_changed = false;

    if let Some(branch) = fields.get("branch") {
        metadata.branch = normalize_optional(branch).map(str::to_string);
        metadata_changed = true;
    }
    if let Some(commit) = fields.get("commit") {
        metadata.commit = normalize_optional(commit).map(str::to_string);
        metadata_changed = true;
    }
    if metadata_changed {
        write_workflow_metadata(&task_path, &metadata)?;
    }

    let blocked_on = fields.get("blocked_on").cloned();
    let block_reason = fields.get("block_reason").cloned();
    let should_clear_blocked = fields.contains_key("clear_blocked");
    if blocked_on.is_some() || block_reason.is_some() || should_clear_blocked {
        update_task_frontmatter(&task_path, |mapping| {
            if should_clear_blocked {
                clear_blocked(mapping);
            }
            let normalized_reason = block_reason
                .as_deref()
                .and_then(normalize_optional)
                .or_else(|| blocked_on.as_deref().and_then(normalize_optional));
            let normalized_blocked_on = blocked_on.as_deref().and_then(normalize_optional);
            if normalized_reason.is_some() || normalized_blocked_on.is_some() {
                set_blocked_reason(mapping, normalized_reason, normalized_blocked_on);
            }
        })?;
    }

    println!("Task #{task_id} metadata updated.");
    Ok(())
}

pub fn cmd_schedule(
    board_dir: &Path,
    task_id: u32,
    at: Option<&str>,
    cron_expr: Option<&str>,
    clear: bool,
) -> Result<()> {
    if !clear && at.is_none() && cron_expr.is_none() {
        bail!("at least one of --at, --cron, or --clear is required");
    }

    // Validate --at as RFC3339
    if let Some(ts) = at {
        chrono::DateTime::parse_from_rfc3339(ts)
            .with_context(|| format!("invalid RFC3339 timestamp: {ts}"))?;
    }

    // Validate --cron expression (the cron crate requires 6-7 fields with seconds;
    // auto-prepend "0 " for standard 5-field cron expressions)
    if let Some(expr) = cron_expr {
        use std::str::FromStr;
        let normalized = normalize_cron(expr);
        cron::Schedule::from_str(&normalized)
            .map_err(|e| anyhow::anyhow!("invalid cron expression: {e}"))?;
    }

    let task_path = find_task_path(board_dir, task_id)?;
    update_task_frontmatter(&task_path, |mapping| {
        if clear {
            mapping.remove(yaml_key("scheduled_for"));
            mapping.remove(yaml_key("cron_schedule"));
        } else {
            if let Some(ts) = at {
                mapping.insert(yaml_key("scheduled_for"), Value::String(ts.to_string()));
            }
            if let Some(expr) = cron_expr {
                mapping.insert(yaml_key("cron_schedule"), Value::String(expr.to_string()));
            }
        }
    })?;

    if clear {
        println!("Task #{task_id} schedule cleared.");
    } else {
        let mut parts = Vec::new();
        if let Some(ts) = at {
            parts.push(format!("scheduled_for={ts}"));
        }
        if let Some(expr) = cron_expr {
            parts.push(format!("cron_schedule={expr}"));
        }
        println!("Task #{task_id} schedule updated: {}", parts.join(", "));
    }
    Ok(())
}

pub fn cmd_auto_merge(task_id: u32, enabled: bool, project_root: &Path) -> Result<()> {
    super::auto_merge::save_override(project_root, task_id, enabled)?;
    let action = if enabled { "enabled" } else { "disabled" };
    println!(
        "Auto-merge {action} for task #{task_id}. The daemon will pick this up on its next completion evaluation."
    );
    Ok(())
}

pub(crate) fn find_task_path(board_dir: &Path, task_id: u32) -> Result<PathBuf> {
    crate::task::find_task_path_by_id(&board_dir.join("tasks"), task_id)
}

fn parse_task_state(value: &str) -> Result<TaskState> {
    match value.trim().replace('-', "_").as_str() {
        "backlog" => Ok(TaskState::Backlog),
        "todo" => Ok(TaskState::Todo),
        "in_progress" => Ok(TaskState::InProgress),
        "review" => Ok(TaskState::Review),
        "blocked" => Ok(TaskState::Blocked),
        "done" => Ok(TaskState::Done),
        "archived" => Ok(TaskState::Archived),
        other => bail!("unknown task state `{other}`"),
    }
}

fn parse_review_disposition(value: &str) -> Result<ReviewDisposition> {
    match value.trim().replace('-', "_").as_str() {
        "approved" => Ok(ReviewDisposition::Approved),
        "changes_requested" => Ok(ReviewDisposition::ChangesRequested),
        "rejected" => Ok(ReviewDisposition::Rejected),
        other => bail!("unknown review disposition `{other}`"),
    }
}

fn state_name(state: TaskState) -> &'static str {
    match state {
        TaskState::Backlog => "backlog",
        TaskState::Todo => "todo",
        TaskState::InProgress => "in-progress",
        TaskState::Review => "review",
        TaskState::Blocked => "blocked",
        TaskState::Done => "done",
        TaskState::Archived => "archived",
    }
}

fn review_disposition_name(disposition: ReviewDisposition) -> &'static str {
    match disposition {
        ReviewDisposition::Approved => "approved",
        ReviewDisposition::ChangesRequested => "changes_requested",
        ReviewDisposition::Rejected => "rejected",
    }
}

pub(crate) fn update_task_frontmatter<F>(task_path: &Path, mutator: F) -> Result<()>
where
    F: FnOnce(&mut Mapping),
{
    let content = std::fs::read_to_string(task_path)
        .with_context(|| format!("failed to read {}", task_path.display()))?;
    let (frontmatter, body) = split_task_frontmatter(&content)?;
    let mut mapping: Mapping =
        serde_yaml::from_str(frontmatter).context("failed to parse task frontmatter")?;
    mutator(&mut mapping);

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

    std::fs::write(task_path, updated)
        .with_context(|| format!("failed to write {}", task_path.display()))?;
    Ok(())
}

fn split_task_frontmatter(content: &str) -> Result<(&str, &str)> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        bail!("task file missing YAML frontmatter (no opening ---)");
    }

    let after_open = &trimmed[3..];
    let after_open = after_open.strip_prefix('\n').unwrap_or(after_open);
    let close_pos = after_open
        .find("\n---")
        .context("task file missing closing --- for frontmatter")?;

    let frontmatter = &after_open[..close_pos];
    let body = &after_open[close_pos + 4..];
    Ok((frontmatter, body.strip_prefix('\n').unwrap_or(body)))
}

fn set_status(mapping: &mut Mapping, state: TaskState) {
    mapping.insert(
        yaml_key("status"),
        Value::String(state_name(state).to_string()),
    );
}

fn clear_blocked(mapping: &mut Mapping) {
    mapping.remove(yaml_key("blocked"));
    mapping.remove(yaml_key("block_reason"));
    mapping.remove(yaml_key("blocked_on"));
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

pub(crate) fn normalize_blocked_frontmatter(task_path: &Path) -> Result<bool> {
    let mut changed = false;
    update_task_frontmatter(task_path, |mapping| {
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

        let legacy_reason = match blocked_value {
            Some(Value::String(reason)) if !reason.trim().is_empty() => Some(reason),
            Some(Value::Bool(true)) => block_reason.clone().or(blocked_on.clone()),
            Some(Value::Bool(false)) => None,
            _ => block_reason.clone().or(blocked_on.clone()),
        };

        if status_is_blocked && legacy_reason.is_some() {
            let desired_reason = legacy_reason.as_deref();
            let desired_blocked_on = blocked_on.as_deref().or(desired_reason).map(str::to_string);
            let needs_rewrite =
                !matches!(mapping.get(yaml_key("blocked")), Some(Value::Bool(true)))
                    || block_reason.as_deref() != desired_reason
                    || mapping.get(yaml_key("blocked_on")).and_then(Value::as_str)
                        != desired_blocked_on.as_deref();
            if needs_rewrite {
                set_blocked_reason(mapping, desired_reason, desired_blocked_on.as_deref());
                changed = true;
            }
        }
    })?;
    Ok(changed)
}

pub(crate) fn set_optional_string(mapping: &mut Mapping, key: &str, value: Option<&str>) {
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

pub(crate) fn set_optional_u64(mapping: &mut Mapping, key: &str, value: Option<u64>) {
    let key = yaml_key(key);
    match value {
        Some(value) => {
            mapping.insert(key, Value::Number(value.into()));
        }
        None => {
            mapping.remove(key);
        }
    }
}

pub(crate) fn set_optional_u32(mapping: &mut Mapping, key: &str, value: Option<u32>) {
    set_optional_u64(mapping, key, value.map(u64::from));
}

pub(crate) fn yaml_key(name: &str) -> Value {
    Value::String(name.to_string())
}

/// Normalize a cron expression for the `cron` crate which requires 6-7 fields
/// (sec min hour dom month dow [year]). If the user provides a standard 5-field
/// expression, prepend "0 " to add a seconds field.
fn normalize_cron(expr: &str) -> String {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() == 5 {
        format!("0 {expr}")
    } else {
        expr.to_string()
    }
}

fn normalize_optional(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_task_file(dir: &Path, id: u32, status: &str) -> PathBuf {
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let path = tasks_dir.join(format!("{id:03}-task-{id}.md"));
        std::fs::write(
            &path,
            format!(
                "---\nid: {id}\ntitle: Task {id}\nstatus: {status}\npriority: high\nclass: standard\n---\n\nTask body.\n"
            ),
        )
        .unwrap();
        path
    }

    #[test]
    fn transition_updates_task_status() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        let task_path = write_task_file(board_dir, 7, "todo");

        cmd_transition(board_dir, 7, "in-progress").unwrap();

        let task = Task::from_file(&task_path).unwrap();
        assert_eq!(task.status, "in-progress");
    }

    #[test]
    fn illegal_transition_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        write_task_file(board_dir, 8, "backlog");

        let error = cmd_transition(board_dir, 8, "done")
            .unwrap_err()
            .to_string();
        assert!(error.contains("illegal task state transition"));
    }

    #[test]
    fn assign_updates_execution_and_review_owners() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        let task_path = write_task_file(board_dir, 9, "todo");

        cmd_assign(board_dir, 9, Some("eng-1-2"), Some("manager-1")).unwrap();

        let task = Task::from_file(&task_path).unwrap();
        assert_eq!(task.claimed_by.as_deref(), Some("eng-1-2"));
        assert_eq!(task.review_owner.as_deref(), Some("manager-1"));
    }

    #[test]
    fn review_updates_status_and_outcome() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        let task_path = write_task_file(board_dir, 10, "review");

        cmd_review(board_dir, 10, "approved", None).unwrap();

        let task = Task::from_file(&task_path).unwrap();
        assert_eq!(task.status, "done");
        let metadata = read_workflow_metadata(&task_path).unwrap();
        assert_eq!(metadata.outcome.as_deref(), Some("approved"));
    }

    #[test]
    fn update_writes_board_metadata_and_block_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        let task_path = write_task_file(board_dir, 11, "blocked");

        let fields = HashMap::from([
            ("branch".to_string(), "eng-1-2/task-11".to_string()),
            ("commit".to_string(), "abc1234".to_string()),
            ("blocked_on".to_string(), "waiting for review".to_string()),
        ]);
        cmd_update(board_dir, 11, fields).unwrap();

        let metadata = read_workflow_metadata(&task_path).unwrap();
        assert_eq!(metadata.branch.as_deref(), Some("eng-1-2/task-11"));
        assert_eq!(metadata.commit.as_deref(), Some("abc1234"));

        let task = Task::from_file(&task_path).unwrap();
        assert_eq!(task.blocked.as_deref(), Some("waiting for review"));
        assert_eq!(task.blocked_on.as_deref(), Some("waiting for review"));
        let content = std::fs::read_to_string(&task_path).unwrap();
        assert!(content.contains("blocked: true"));
        assert!(content.contains("block_reason: waiting for review"));

        cmd_update(
            board_dir,
            11,
            HashMap::from([("clear_blocked".to_string(), "true".to_string())]),
        )
        .unwrap();

        let task = Task::from_file(&task_path).unwrap();
        assert!(task.blocked.is_none());
        assert!(task.blocked_on.is_none());
    }

    #[test]
    fn normalize_blocked_frontmatter_repairs_legacy_string_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        let task_path = write_task_file(board_dir, 14, "blocked");
        std::fs::write(
            &task_path,
            "---\nid: 14\ntitle: Task 14\nstatus: blocked\npriority: high\nblocked: legacy verification reason\nclass: standard\n---\n\nTask body.\n",
        )
        .unwrap();

        let changed = normalize_blocked_frontmatter(&task_path).unwrap();

        assert!(changed);
        let content = std::fs::read_to_string(&task_path).unwrap();
        assert!(content.contains("blocked: true"));
        assert!(content.contains("block_reason: legacy verification reason"));
        let task = Task::from_file(&task_path).unwrap();
        assert_eq!(task.blocked.as_deref(), Some("legacy verification reason"));
    }

    #[test]
    fn update_requires_at_least_one_field() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        write_task_file(board_dir, 12, "todo");

        let error = cmd_update(board_dir, 12, HashMap::new())
            .unwrap_err()
            .to_string();
        assert!(error.contains("no workflow fields provided"));
    }

    #[test]
    fn task_commands_work_without_orchestrator_runtime() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        let task_path = write_task_file(board_dir, 13, "todo");

        cmd_assign(board_dir, 13, Some("eng-1-2"), Some("manager-1")).unwrap();
        cmd_transition(board_dir, 13, "in-progress").unwrap();
        cmd_transition(board_dir, 13, "review").unwrap();
        cmd_update(
            board_dir,
            13,
            HashMap::from([
                ("branch".to_string(), "eng-1-2/task-13".to_string()),
                ("commit".to_string(), "deadbeef".to_string()),
            ]),
        )
        .unwrap();
        cmd_review(board_dir, 13, "approved", None).unwrap();

        let task = Task::from_file(&task_path).unwrap();
        let metadata = read_workflow_metadata(&task_path).unwrap();
        assert_eq!(task.status, "done");
        assert_eq!(task.claimed_by.as_deref(), Some("eng-1-2"));
        assert_eq!(task.review_owner.as_deref(), Some("manager-1"));
        assert_eq!(metadata.branch.as_deref(), Some("eng-1-2/task-13"));
        assert_eq!(metadata.commit.as_deref(), Some("deadbeef"));
        assert_eq!(metadata.outcome.as_deref(), Some("approved"));
    }

    fn write_review_task_with_engineer(dir: &Path, id: u32, engineer: &str) -> PathBuf {
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let path = tasks_dir.join(format!("{id:03}-task-{id}.md"));
        std::fs::write(
            &path,
            format!(
                "---\nid: {id}\ntitle: Task {id}\nstatus: review\npriority: high\nclass: standard\nclaimed_by: {engineer}\n---\n\nTask body.\n"
            ),
        )
        .unwrap();
        path
    }

    #[test]
    fn review_feedback_stored_in_task() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        let task_path = write_review_task_with_engineer(board_dir, 42, "eng-1-2");

        cmd_review(
            board_dir,
            42,
            "changes_requested",
            Some("fix the error handling"),
        )
        .unwrap();

        let content = std::fs::read_to_string(&task_path).unwrap();
        assert!(
            content.contains("fix the error handling"),
            "feedback should be stored in task frontmatter"
        );
    }

    #[test]
    fn review_feedback_delivered_to_engineer() {
        let tmp = tempfile::tempdir().unwrap();

        // Create project structure: board_dir must be at <root>/.batty/team_config/board
        let project_root = tmp.path().join("project");
        let actual_board_dir = project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        std::fs::create_dir_all(actual_board_dir.join("tasks")).unwrap();

        // Create inbox for engineer
        let inbox_root = crate::team::inbox::inboxes_root(&project_root);
        crate::team::inbox::init_inbox(&inbox_root, "eng-1-2").unwrap();

        // Write task in the actual board dir
        let task_path = actual_board_dir.join("tasks").join("042-task-42.md");
        std::fs::write(
            &task_path,
            "---\nid: 42\ntitle: Task 42\nstatus: review\npriority: high\nclass: standard\nclaimed_by: eng-1-2\n---\n\nTask body.\n",
        )
        .unwrap();

        cmd_review(
            &actual_board_dir,
            42,
            "changes_requested",
            Some("fix the error handling"),
        )
        .unwrap();

        let pending = crate::team::inbox::pending_messages(&inbox_root, "eng-1-2").unwrap();
        assert_eq!(pending.len(), 1);
        assert!(
            pending[0].body.contains("fix the error handling"),
            "feedback message should be delivered to engineer inbox"
        );
        assert!(pending[0].body.contains("#42"));
    }

    #[test]
    fn schedule_task_sets_scheduled_for() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        let task_path = write_task_file(board_dir, 60, "todo");

        cmd_schedule(
            board_dir,
            60,
            Some("2026-03-25T09:00:00-04:00"),
            None,
            false,
        )
        .unwrap();

        let task = Task::from_file(&task_path).unwrap();
        assert_eq!(
            task.scheduled_for.as_deref(),
            Some("2026-03-25T09:00:00-04:00")
        );
        assert!(task.cron_schedule.is_none());
    }

    #[test]
    fn schedule_task_sets_cron_schedule() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        let task_path = write_task_file(board_dir, 61, "todo");

        cmd_schedule(board_dir, 61, None, Some("0 9 * * *"), false).unwrap();

        let task = Task::from_file(&task_path).unwrap();
        assert!(task.scheduled_for.is_none());
        assert_eq!(task.cron_schedule.as_deref(), Some("0 9 * * *"));
    }

    #[test]
    fn schedule_task_clear_removes_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        let task_path = write_task_file(board_dir, 62, "todo");

        // Set both fields first
        cmd_schedule(
            board_dir,
            62,
            Some("2026-04-01T00:00:00Z"),
            Some("0 9 * * 1"),
            false,
        )
        .unwrap();
        let task = Task::from_file(&task_path).unwrap();
        assert!(task.scheduled_for.is_some());
        assert!(task.cron_schedule.is_some());

        // Clear
        cmd_schedule(board_dir, 62, None, None, true).unwrap();
        let task = Task::from_file(&task_path).unwrap();
        assert!(task.scheduled_for.is_none());
        assert!(task.cron_schedule.is_none());
    }

    #[test]
    fn schedule_task_sets_both() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        let task_path = write_task_file(board_dir, 63, "todo");

        cmd_schedule(
            board_dir,
            63,
            Some("2026-04-01T00:00:00Z"),
            Some("0 9 * * 1"),
            false,
        )
        .unwrap();

        let task = Task::from_file(&task_path).unwrap();
        assert_eq!(task.scheduled_for.as_deref(), Some("2026-04-01T00:00:00Z"));
        assert_eq!(task.cron_schedule.as_deref(), Some("0 9 * * 1"));
    }

    #[test]
    fn schedule_rejects_invalid_timestamp() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        write_task_file(board_dir, 64, "todo");

        let err = cmd_schedule(board_dir, 64, Some("not-a-date"), None, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid RFC3339 timestamp"));
    }

    #[test]
    fn schedule_rejects_invalid_cron() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        write_task_file(board_dir, 65, "todo");

        let err = cmd_schedule(board_dir, 65, None, Some("not-a-cron"), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid cron expression"));
    }

    #[test]
    fn schedule_requires_at_least_one_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        write_task_file(board_dir, 66, "todo");

        let err = cmd_schedule(board_dir, 66, None, None, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("at least one of --at, --cron, or --clear"));
    }

    // --- Structured review tests ---

    #[test]
    fn structured_review_approve_stores_frontmatter_and_moves_to_done() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        let task_path = write_task_file(board_dir, 70, "review");

        cmd_review_structured(board_dir, 70, "approve", None, "manager-1").unwrap();

        let task = Task::from_file(&task_path).unwrap();
        assert_eq!(task.status, "done");

        let content = std::fs::read_to_string(&task_path).unwrap();
        assert!(content.contains("review_disposition: approved"));
        assert!(content.contains("reviewed_by: manager-1"));
        assert!(content.contains("reviewed_at:"));

        let metadata = read_workflow_metadata(&task_path).unwrap();
        assert_eq!(metadata.outcome.as_deref(), Some("approved"));
    }

    #[test]
    fn structured_review_request_changes_stores_feedback_and_moves_to_in_progress() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        let task_path = write_task_file(board_dir, 71, "review");

        cmd_review_structured(
            board_dir,
            71,
            "request-changes",
            Some("fix the error handling"),
            "manager-1",
        )
        .unwrap();

        let task = Task::from_file(&task_path).unwrap();
        assert_eq!(task.status, "in-progress");

        let content = std::fs::read_to_string(&task_path).unwrap();
        assert!(content.contains("review_disposition: changes_requested"));
        assert!(content.contains("review_feedback: fix the error handling"));
        assert!(content.contains("reviewed_by: manager-1"));
        assert!(content.contains("reviewed_at:"));
    }

    #[test]
    fn structured_review_reject_moves_to_blocked_with_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        let task_path = write_task_file(board_dir, 72, "review");

        cmd_review_structured(
            board_dir,
            72,
            "reject",
            Some("does not meet requirements"),
            "manager-1",
        )
        .unwrap();

        let task = Task::from_file(&task_path).unwrap();
        assert_eq!(task.status, "blocked");

        let content = std::fs::read_to_string(&task_path).unwrap();
        assert!(content.contains("review_disposition: rejected"));
        assert!(content.contains("review_feedback: does not meet requirements"));
        assert!(content.contains("reviewed_by: manager-1"));
        assert!(content.contains("blocked_on: does not meet requirements"));
    }

    #[test]
    fn structured_review_reject_without_feedback_uses_default_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        let task_path = write_task_file(board_dir, 73, "review");

        cmd_review_structured(board_dir, 73, "reject", None, "manager-1").unwrap();

        let task = Task::from_file(&task_path).unwrap();
        assert_eq!(task.status, "blocked");

        let content = std::fs::read_to_string(&task_path).unwrap();
        assert!(content.contains("blocked_on: rejected by manager-1"));
    }

    #[test]
    fn structured_review_rejects_non_review_state() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        write_task_file(board_dir, 74, "in-progress");

        let err = cmd_review_structured(board_dir, 74, "approve", None, "manager-1")
            .unwrap_err()
            .to_string();
        assert!(err.contains("illegal task state transition"));
    }

    #[test]
    fn structured_review_feedback_delivered_to_engineer_inbox() {
        let tmp = tempfile::tempdir().unwrap();

        // Create project structure: board_dir must be at <root>/.batty/team_config/board
        let project_root = tmp.path().join("project");
        let actual_board_dir = project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        std::fs::create_dir_all(actual_board_dir.join("tasks")).unwrap();

        // Create inbox for engineer
        let inbox_root = crate::team::inbox::inboxes_root(&project_root);
        crate::team::inbox::init_inbox(&inbox_root, "eng-1-2").unwrap();

        // Write task in the actual board dir
        let task_path = actual_board_dir.join("tasks").join("075-task-75.md");
        std::fs::write(
            &task_path,
            "---\nid: 75\ntitle: Task 75\nstatus: review\npriority: high\nclass: standard\nclaimed_by: eng-1-2\n---\n\nTask body.\n",
        )
        .unwrap();

        cmd_review_structured(
            &actual_board_dir,
            75,
            "request-changes",
            Some("add more tests"),
            "manager-1",
        )
        .unwrap();

        let pending = crate::team::inbox::pending_messages(&inbox_root, "eng-1-2").unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].body.contains("add more tests"));
        assert!(pending[0].body.contains("#75"));
    }
}
