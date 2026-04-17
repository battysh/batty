//! Persistent task learnings used to enrich future dispatch prompts.

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::task::Task;

const LEARNINGS_DIR: &str = "learnings";
const TASK_LEARNINGS_FILE: &str = "task_learnings.jsonl";
const MAX_RELEVANT_LEARNINGS: usize = 3;
const MAX_RELATED_TASKS: usize = 3;
const MAX_FILE_PREDICTIONS: usize = 5;
const MAX_FAILURE_PATTERNS: usize = 2;
const MAX_CONTEXT_WORDS: usize = 500;

#[derive(Debug, Clone, Default)]
struct TaskMetricsSummary {
    retries: i64,
    escalations: i64,
    context_restart_count: i64,
    completed_at: Option<i64>,
}

#[derive(Debug)]
struct RelatedTaskContext {
    task: Task,
    score: usize,
    changed_paths: Vec<String>,
    metrics: TaskMetricsSummary,
    git_summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct LearningEntry {
    pub task_id: u32,
    pub title: String,
    pub summary: String,
    pub tags: Vec<String>,
    pub keywords: Vec<String>,
    pub engineer: String,
    pub completed_at: String,
}

pub(crate) fn append_task_completion_learning(
    project_root: &Path,
    task: &Task,
    engineer: &str,
    summary: &str,
) -> Result<()> {
    let path = task_learnings_path(project_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let mut keywords = extract_keywords(&task.title);
    keywords.extend(extract_keywords(&task.description));
    keywords.sort();
    keywords.dedup();

    let entry = LearningEntry {
        task_id: task.id,
        title: task.title.clone(),
        summary: summary.trim().to_string(),
        tags: task.tags.clone(),
        keywords,
        engineer: engineer.to_string(),
        completed_at: task
            .completed
            .clone()
            .unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
    };

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    serde_json::to_writer(&mut file, &entry)
        .with_context(|| format!("failed to serialize learning entry to {}", path.display()))?;
    writeln!(file).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn assignment_packet(task: &Task, ack_recipient: &str) -> String {
    let allowed_files = crate::team::daemon::verification::parse_scope_fence(&task.description);
    let ack_token = crate::team::daemon::verification::scope_ack_token(task.id);
    let ack_required = !allowed_files.is_empty();
    let mut packet = String::from("Assignment Packet:\n```yaml\n");
    packet.push_str(&format!("task_id: {}\n", task.id));
    if allowed_files.is_empty() {
        packet.push_str("allowed_files: []\n");
    } else {
        packet.push_str("allowed_files:\n");
        for path in allowed_files {
            packet.push_str(&format!("  - {path}\n"));
        }
    }
    packet.push_str(&format!("scope_ack_required: {ack_required}\n"));
    packet.push_str(&format!("scope_ack_token: \"{ack_token}\"\n"));
    packet.push_str(&format!(
        "scope_ack_command: \"batty send {ack_recipient} \\\"{ack_token}\\\"\"\n"
    ));
    packet.push_str("```\n");
    if ack_required {
        packet.push_str(&format!(
            "Before your first file write, run `batty send {ack_recipient} \"{ack_token}\"`.\n"
        ));
    }
    packet
}

pub(crate) fn augment_assignment_message(
    project_root: &Path,
    task: &Task,
    ack_recipient: &str,
) -> Result<String> {
    let mut body = format!("Task #{}: {}\n\n{}", task.id, task.title, task.description);
    body.push_str("\n\n");
    body.push_str(&assignment_packet(task, ack_recipient));
    let context = build_dispatch_context(project_root, task)?;
    if context.is_empty() {
        return Ok(body);
    }

    body.push_str("\n\nDispatch context:\n");
    body.push_str(&context);
    Ok(body)
}

fn build_dispatch_context(project_root: &Path, task: &Task) -> Result<String> {
    let related = related_completed_tasks(project_root, task, MAX_RELATED_TASKS)?;
    let learnings = relevant_learnings(project_root, task, MAX_RELEVANT_LEARNINGS)?;
    let file_predictions = predict_files(&related, task);
    let failure_patterns = relevant_failure_patterns(project_root, &related, MAX_FAILURE_PATTERNS)?;

    let mut sections = Vec::new();

    if !related.is_empty() {
        let mut section = String::from("Recent related completions:\n");
        for entry in &related {
            let mut notes = Vec::new();
            if let Some(summary) = entry.git_summary.as_deref() {
                notes.push(format!("git: {summary}"));
            }
            if entry.metrics.retries > 0 {
                notes.push(format!("retries={}", entry.metrics.retries));
            }
            if entry.metrics.escalations > 0 {
                notes.push(format!("escalations={}", entry.metrics.escalations));
            }
            if entry.metrics.context_restart_count > 0 {
                notes.push(format!(
                    "context_restarts={}",
                    entry.metrics.context_restart_count
                ));
            }
            let suffix = if notes.is_empty() {
                String::new()
            } else {
                format!(" ({})", notes.join(", "))
            };
            section.push_str(&format!(
                "- Task #{}: {}{}\n",
                entry.task.id, entry.task.title, suffix
            ));
        }
        sections.push(section);
    }

    if !failure_patterns.is_empty() {
        let mut section = String::from("Failure history from similar tasks:\n");
        for pattern in failure_patterns {
            section.push_str(&format!("- {}\n", pattern.description));
        }
        sections.push(section);
    }

    if !file_predictions.is_empty() {
        let mut section = String::from("Likely files to inspect:\n");
        for path in file_predictions {
            section.push_str(&format!("- {path}\n"));
        }
        sections.push(section);
    }

    if !learnings.is_empty() {
        let mut section = String::from("Relevant prior learnings:\n");
        for learning in learnings {
            let tags = if learning.tags.is_empty() {
                "untagged".to_string()
            } else {
                learning.tags.join(", ")
            };
            section.push_str(&format!(
                "- Task #{} [{}] {}\n",
                learning.task_id, tags, learning.summary
            ));
        }
        sections.push(section);
    }

    Ok(limit_word_count(&sections.join("\n"), MAX_CONTEXT_WORDS))
}

fn task_learnings_path(project_root: &Path) -> PathBuf {
    project_root
        .join(".batty")
        .join(LEARNINGS_DIR)
        .join(TASK_LEARNINGS_FILE)
}

fn load_task_learnings(project_root: &Path) -> Result<Vec<LearningEntry>> {
    let path = task_learnings_path(project_root);
    if !path.exists() {
        return Ok(Vec::new());
    }

    let file =
        fs::File::open(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut entries = Vec::new();
    for line in reader.lines() {
        let line = line.with_context(|| format!("failed to read {}", path.display()))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<LearningEntry>(trimmed) {
            Ok(entry) => entries.push(entry),
            Err(error) => {
                tracing::warn!(path = %path.display(), error = %error, "skipping malformed learning entry");
            }
        }
    }
    Ok(entries)
}

fn related_completed_tasks(
    project_root: &Path,
    task: &Task,
    limit: usize,
) -> Result<Vec<RelatedTaskContext>> {
    let board_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board");
    let tasks_dir = board_dir.join("tasks");
    if !tasks_dir.exists() {
        return Ok(Vec::new());
    }
    let mut metrics_by_task = load_task_metrics(project_root)?;

    let mut scored = Vec::new();
    for candidate in crate::task::load_tasks_from_dir(&tasks_dir)? {
        if candidate.id == task.id || candidate.status != "done" {
            continue;
        }

        let changed_paths = load_changed_paths(candidate.source_path.as_path())?;
        let score = related_task_score(task, &candidate, &changed_paths);
        if score == 0 {
            continue;
        }

        let metrics = metrics_by_task.remove(&candidate.id).unwrap_or_default();
        let git_summary = candidate
            .commit
            .as_deref()
            .and_then(|commit| git_commit_summary(project_root, commit));
        scored.push(RelatedTaskContext {
            metrics,
            git_summary,
            changed_paths,
            score,
            task: candidate,
        });
    }

    scored.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| right.metrics.completed_at.cmp(&left.metrics.completed_at))
            .then_with(|| right.task.completed.cmp(&left.task.completed))
            .then_with(|| right.task.id.cmp(&left.task.id))
    });
    scored.truncate(limit);
    Ok(scored)
}

fn relevant_learnings(
    project_root: &Path,
    task: &Task,
    limit: usize,
) -> Result<Vec<LearningEntry>> {
    let task_keywords: HashSet<String> =
        extract_keywords(&format!("{}\n{}", task.title, task.description))
            .into_iter()
            .collect();
    let task_tags: HashSet<String> = task
        .tags
        .iter()
        .map(|tag| tag.to_ascii_lowercase())
        .collect();

    let mut scored: Vec<(usize, LearningEntry)> = load_task_learnings(project_root)?
        .into_iter()
        .filter(|entry| entry.task_id != task.id)
        .filter_map(|entry| {
            let tag_matches = entry
                .tags
                .iter()
                .map(|tag| tag.to_ascii_lowercase())
                .filter(|tag| task_tags.contains(tag))
                .count();
            let keyword_matches = entry
                .keywords
                .iter()
                .filter(|keyword| task_keywords.contains(*keyword))
                .count();
            let score = tag_matches * 3 + keyword_matches;
            (score > 0).then_some((score, entry))
        })
        .collect();

    scored.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| right.1.completed_at.cmp(&left.1.completed_at))
    });
    Ok(scored
        .into_iter()
        .take(limit)
        .map(|(_, entry)| entry)
        .collect())
}

fn relevant_failure_patterns(
    project_root: &Path,
    related: &[RelatedTaskContext],
    limit: usize,
) -> Result<Vec<crate::team::failure_patterns::PatternMatch>> {
    if related.is_empty() {
        return Ok(Vec::new());
    }

    let related_ids: HashSet<String> = related
        .iter()
        .map(|entry| entry.task.id.to_string())
        .collect();
    let events_path = crate::team::team_events_path(project_root);
    if !events_path.exists() {
        return Ok(Vec::new());
    }
    let events = crate::team::events::read_events(&events_path)?;
    let mut window = crate::team::failure_patterns::FailureWindow::new(100);
    for event in events.into_iter().filter(|event| {
        event
            .task
            .as_deref()
            .is_some_and(|task_id| related_ids.contains(task_id))
    }) {
        window.push(&event);
    }

    let mut patterns = window.detect_failure_patterns();
    patterns.truncate(limit);
    Ok(patterns)
}

fn predict_files(related: &[RelatedTaskContext], task: &Task) -> Vec<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for entry in related {
        for path in &entry.changed_paths {
            *counts.entry(path.clone()).or_insert(0) += entry.score.max(1);
        }
    }
    for hinted in extract_path_hints(task) {
        *counts.entry(hinted).or_insert(0) += 2;
    }

    let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    ranked
        .into_iter()
        .take(MAX_FILE_PREDICTIONS)
        .map(|(path, _)| path)
        .collect()
}

fn load_task_metrics(project_root: &Path) -> Result<HashMap<u32, TaskMetricsSummary>> {
    let conn = match crate::team::telemetry_db::open(project_root) {
        Ok(conn) => conn,
        Err(_) => return Ok(HashMap::new()),
    };
    let mut metrics = HashMap::new();
    for row in crate::team::telemetry_db::query_task_metrics(&conn)? {
        let Ok(task_id) = row.task_id.parse::<u32>() else {
            continue;
        };
        metrics.insert(
            task_id,
            TaskMetricsSummary {
                retries: row.retries,
                escalations: row.escalations,
                context_restart_count: row.context_restart_count,
                completed_at: row.completed_at,
            },
        );
    }
    Ok(metrics)
}

fn related_task_score(task: &Task, candidate: &Task, changed_paths: &[String]) -> usize {
    let task_tags: HashSet<String> = task
        .tags
        .iter()
        .map(|tag| tag.to_ascii_lowercase())
        .collect();
    let candidate_tags: HashSet<String> = candidate
        .tags
        .iter()
        .map(|tag| tag.to_ascii_lowercase())
        .collect();
    let task_keywords: HashSet<String> =
        extract_keywords(&format!("{}\n{}", task.title, task.description))
            .into_iter()
            .collect();
    let candidate_keywords: HashSet<String> =
        extract_keywords(&format!("{}\n{}", candidate.title, candidate.description))
            .into_iter()
            .collect();
    let task_dirs: HashSet<String> = extract_path_hints(task)
        .into_iter()
        .filter_map(|path| parent_dir(&path))
        .collect();
    let candidate_dirs: HashSet<String> = changed_paths
        .iter()
        .filter_map(|path| parent_dir(path))
        .collect();

    let tag_matches = task_tags.intersection(&candidate_tags).count();
    let keyword_matches = task_keywords.intersection(&candidate_keywords).count();
    let dir_matches = task_dirs.intersection(&candidate_dirs).count();
    tag_matches * 4 + dir_matches * 3 + keyword_matches
}

fn load_changed_paths(path: &Path) -> Result<Vec<String>> {
    if path.as_os_str().is_empty() || !path.exists() {
        return Ok(Vec::new());
    }

    let content = fs::read_to_string(path)?;
    let Some(frontmatter) = extract_frontmatter(&content) else {
        return Ok(Vec::new());
    };
    let parsed: LearningTaskFrontmatter = serde_yaml::from_str(frontmatter).unwrap_or_default();
    Ok(parsed.changed_paths)
}

fn git_commit_summary(project_root: &Path, commit: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["log", "-1", "--format=%s", commit])
        .current_dir(project_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let summary = String::from_utf8(output.stdout).ok()?;
    let trimmed = summary.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn extract_path_hints(task: &Task) -> HashSet<String> {
    task.description
        .split_whitespace()
        .filter_map(clean_task_path_token)
        .collect()
}

fn clean_task_path_token(token: &str) -> Option<String> {
    let cleaned = token.trim_matches(|ch: char| {
        matches!(
            ch,
            '"' | '\'' | ',' | ':' | ';' | '(' | ')' | '[' | ']' | '`'
        )
    });
    parent_dir(cleaned).map(|_| cleaned.to_string())
}

fn parent_dir(path: &str) -> Option<String> {
    PathBuf::from(path)
        .parent()
        .map(|parent| parent.to_string_lossy().replace('\\', "/"))
        .filter(|parent| !parent.is_empty() && parent != ".")
}

fn extract_frontmatter(content: &str) -> Option<&str> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }
    let after_open = trimmed[3..].strip_prefix('\n').unwrap_or(&trimmed[3..]);
    let close_pos = after_open.find("\n---")?;
    Some(&after_open[..close_pos])
}

fn limit_word_count(text: &str, max_words: usize) -> String {
    let mut words = 0usize;
    let mut lines = Vec::new();
    for line in text.lines() {
        let line_words = line.split_whitespace().count();
        if line_words == 0 {
            if !lines.is_empty() && !lines.last().is_some_and(|last: &String| last.is_empty()) {
                lines.push(String::new());
            }
            continue;
        }

        if words + line_words <= max_words {
            lines.push(line.to_string());
            words += line_words;
            continue;
        }

        let remaining = max_words.saturating_sub(words);
        if remaining == 0 {
            break;
        }
        let truncated = line
            .split_whitespace()
            .take(remaining)
            .collect::<Vec<_>>()
            .join(" ");
        if !truncated.is_empty() {
            lines.push(format!("{truncated} ..."));
        }
        break;
    }
    lines.join("\n").trim().to_string()
}

#[derive(Debug, Default, Deserialize)]
struct LearningTaskFrontmatter {
    #[serde(default)]
    changed_paths: Vec<String>,
}

fn extract_keywords(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    text.split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter_map(|word| {
            let normalized = word.trim().to_ascii_lowercase();
            if normalized.len() < 4 || normalized.chars().all(|ch| ch.is_ascii_digit()) {
                return None;
            }
            seen.insert(normalized.clone()).then_some(normalized)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::events::EventSink;
    use crate::team::events::TeamEvent;
    use crate::team::telemetry_db;

    fn write_task_file(project_root: &Path, filename: &str, content: &str) {
        let tasks_dir = project_root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        fs::write(tasks_dir.join(filename), content).unwrap();
    }

    fn ensure_batty_dirs(project_root: &Path) {
        fs::create_dir_all(project_root.join(".batty").join("team_config")).unwrap();
    }

    fn sample_task() -> Task {
        Task {
            id: 42,
            title: "Improve dispatch scoring".to_string(),
            status: "todo".to_string(),
            priority: "high".to_string(),
            assignee: None,
            claimed_by: Some("eng-1".to_string()),
            claimed_at: None,
            claim_ttl_secs: None,
            claim_expires_at: None,
            last_progress_at: None,
            claim_warning_sent_at: None,
            claim_extensions: None,
            last_output_bytes: None,
            blocked: None,
            tags: vec!["dispatch".to_string(), "daemon".to_string()],
            depends_on: Vec::new(),
            review_owner: None,
            blocked_on: None,
            worktree_path: None,
            branch: None,
            commit: None,
            artifacts: Vec::new(),
            next_action: None,
            scheduled_for: None,
            cron_schedule: None,
            cron_last_run: None,
            completed: Some("2026-04-06T08:00:00Z".to_string()),
            description: "Teach dispatch queue scoring to prefer daemon work.".to_string(),
            batty_config: None,
            source_path: PathBuf::from("/tmp/task.md"),
        }
    }

    #[test]
    fn append_and_match_learnings_by_tag_and_keyword() {
        let tmp = tempfile::tempdir().unwrap();
        let completed = sample_task();
        append_task_completion_learning(
            tmp.path(),
            &completed,
            "eng-1",
            "Prefer prior daemon dispatch patterns over generic scoring.",
        )
        .unwrap();

        let mut current = sample_task();
        current.id = 99;
        current.tags = vec!["dispatch".to_string()];
        current.description = "Refine daemon dispatch prompts using queue history.".to_string();

        let augmented = augment_assignment_message(tmp.path(), &current, "manager").unwrap();
        assert!(augmented.contains("Relevant prior learnings:"));
        assert!(augmented.contains("Prefer prior daemon dispatch patterns"));
        assert!(augmented.contains("[dispatch, daemon]"));
    }

    #[test]
    fn augment_assignment_message_skips_when_no_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let mut prior = sample_task();
        prior.tags = vec!["grafana".to_string()];
        prior.description = "Alerting rules and dashboards.".to_string();
        append_task_completion_learning(
            tmp.path(),
            &prior,
            "eng-1",
            "Keep alert thresholds conservative.",
        )
        .unwrap();

        let mut current = sample_task();
        current.tags = vec!["dispatch".to_string()];
        current.description = "Dispatch prompt and work allocation.".to_string();

        let augmented = augment_assignment_message(tmp.path(), &current, "manager").unwrap();
        assert!(!augmented.contains("Relevant prior learnings:"));
    }

    #[test]
    fn augment_assignment_message_includes_richer_dispatch_context() {
        let tmp = tempfile::tempdir().unwrap();
        ensure_batty_dirs(tmp.path());
        let conn = telemetry_db::open(tmp.path()).unwrap();
        telemetry_db::insert_event(&conn, &TeamEvent::task_assigned("eng-2", "10")).unwrap();
        telemetry_db::insert_event(&conn, &TeamEvent::task_completed("eng-2", Some("10"))).unwrap();
        let mut escalation = TeamEvent::task_escalated("eng-2", "10", Some("needed rework"));
        escalation.task = Some("10".to_string());
        telemetry_db::insert_event(&conn, &escalation).unwrap();
        let mut escalation_repeat = TeamEvent::task_escalated("eng-2", "10", Some("second rework"));
        escalation_repeat.task = Some("10".to_string());
        telemetry_db::insert_event(&conn, &escalation_repeat).unwrap();
        let mut event_sink = EventSink::new(&crate::team::team_events_path(tmp.path())).unwrap();
        event_sink.emit(escalation.clone()).unwrap();
        event_sink.emit(escalation_repeat).unwrap();

        write_task_file(
            tmp.path(),
            "010-related.md",
            "---\nid: 10\ntitle: Prior dispatch context work\nstatus: done\npriority: high\nclaimed_by: eng-2\ncommit: abc123\ntags:\n  - dispatch\nchanged_paths:\n  - src/team/learnings.rs\n  - src/team/dispatch/queue.rs\nclass: standard\ncompleted: 2026-04-06T08:00:00Z\n---\n\nImprove dispatch context for queue prompts.\n",
        );
        append_task_completion_learning(
            tmp.path(),
            &Task::from_file(
                &tmp.path()
                    .join(".batty")
                    .join("team_config")
                    .join("board")
                    .join("tasks")
                    .join("010-related.md"),
            )
            .unwrap(),
            "eng-2",
            "Call out likely queue files before coding.",
        )
        .unwrap();

        let mut current = sample_task();
        current.id = 99;
        current.description =
            "Expand dispatch context in src/team/learnings.rs and src/team/dispatch/queue.rs."
                .to_string();

        let augmented = augment_assignment_message(tmp.path(), &current, "manager").unwrap();
        assert!(augmented.contains("Dispatch context:"));
        assert!(augmented.contains("Recent related completions:"));
        assert!(augmented.contains("Failure history from similar tasks:"));
        assert!(augmented.contains("Likely files to inspect:"));
        assert!(augmented.contains("Relevant prior learnings:"));
        assert!(augmented.contains("src/team/learnings.rs"));
    }

    #[test]
    fn dispatch_context_is_capped_to_500_words() {
        let repeated = std::iter::repeat_n("context", 700)
            .collect::<Vec<_>>()
            .join(" ");
        let truncated = limit_word_count(&repeated, MAX_CONTEXT_WORDS);
        assert!(truncated.split_whitespace().count() <= MAX_CONTEXT_WORDS + 1);
    }

    #[test]
    fn augment_assignment_message_includes_scope_packet() {
        let tmp = tempfile::tempdir().unwrap();
        let mut current = sample_task();
        current.id = 587;
        current.description =
            "Harden scope validation.\nSCOPE FENCE: src/team/completion.rs, src/team/review.rs\n"
                .to_string();

        let augmented = augment_assignment_message(tmp.path(), &current, "manager").unwrap();
        assert!(augmented.contains("Assignment Packet:"));
        assert!(augmented.contains("allowed_files:"));
        assert!(augmented.contains("src/team/completion.rs"));
        assert!(augmented.contains("src/team/review.rs"));
        assert!(augmented.contains("scope_ack_required: true"));
        assert!(augmented.contains("Scope ACK #587"));
        assert!(augmented.contains("batty send manager"));
    }
}
