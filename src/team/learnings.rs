//! Persistent task learnings used to enrich future dispatch prompts.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::task::Task;

const LEARNINGS_DIR: &str = "learnings";
const TASK_LEARNINGS_FILE: &str = "task_learnings.jsonl";
const MAX_RELEVANT_LEARNINGS: usize = 3;

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

pub(crate) fn augment_assignment_message(project_root: &Path, task: &Task) -> Result<String> {
    let mut body = format!("Task #{}: {}\n\n{}", task.id, task.title, task.description);
    let relevant = relevant_learnings(project_root, task, MAX_RELEVANT_LEARNINGS)?;
    if relevant.is_empty() {
        return Ok(body);
    }

    body.push_str("\n\nRelevant prior learnings:\n");
    for learning in relevant {
        let tags = if learning.tags.is_empty() {
            "untagged".to_string()
        } else {
            learning.tags.join(", ")
        };
        body.push_str(&format!(
            "- Task #{} [{}] {}\n",
            learning.task_id, tags, learning.summary
        ));
    }
    Ok(body)
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

    fn sample_task() -> Task {
        Task {
            id: 42,
            title: "Improve dispatch scoring".to_string(),
            status: "todo".to_string(),
            priority: "high".to_string(),
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

        let augmented = augment_assignment_message(tmp.path(), &current).unwrap();
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

        let augmented = augment_assignment_message(tmp.path(), &current).unwrap();
        assert!(!augmented.contains("Relevant prior learnings:"));
    }
}
