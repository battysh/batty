use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::task::Task;

const GITHUB_FEEDBACK_FILE: &str = "github_verification.jsonl";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GithubVerificationRecord {
    pub(crate) task_id: u32,
    #[serde(default)]
    pub(crate) branch: Option<String>,
    #[serde(default)]
    pub(crate) commit: Option<String>,
    #[serde(alias = "check")]
    pub(crate) check_name: String,
    #[serde(alias = "conclusion")]
    pub(crate) status: String,
    #[serde(default)]
    pub(crate) next_action: Option<String>,
    #[serde(default)]
    pub(crate) details: Option<String>,
    #[serde(default)]
    pub(crate) ts: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GithubFeedbackWarningKind {
    UnknownTask,
    StaleCommit,
    StaleBranch,
    UnknownStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GithubFeedbackWarning {
    pub(crate) kind: GithubFeedbackWarningKind,
    pub(crate) task_id: u32,
    pub(crate) check_name: String,
    pub(crate) reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct GithubFeedbackSnapshot {
    pub(crate) failed: HashMap<u32, GithubVerificationRecord>,
    pub(crate) passed: HashMap<u32, GithubVerificationRecord>,
    pub(crate) warnings: Vec<GithubFeedbackWarning>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubReleaseFeedbackSummary {
    pub current_commit: Option<String>,
    pub clean: bool,
    pub failing: Vec<GithubReleaseFeedbackItem>,
    pub warnings: Vec<GithubReleaseFeedbackItem>,
    pub stale: Vec<GithubReleaseFeedbackItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubReleaseFeedbackItem {
    pub check_name: String,
    pub status: String,
    pub commit: Option<String>,
    pub age_secs: Option<u64>,
    pub next_action: Option<String>,
    pub details: Option<String>,
}

pub(crate) fn github_feedback_path(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join(GITHUB_FEEDBACK_FILE)
}

pub(crate) fn load_github_feedback(project_root: &Path) -> Result<Vec<GithubVerificationRecord>> {
    let path = github_feedback_path(project_root);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file =
        fs::File::open(&path).with_context(|| format!("failed to open {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut records = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("failed to read {}", path.display()))?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        records.push(serde_json::from_str(trimmed).with_context(|| {
            format!(
                "failed to parse GitHub verification record {} in {}",
                index + 1,
                path.display()
            )
        })?);
    }
    Ok(records)
}

#[cfg(test)]
pub(crate) fn write_github_feedback_record(
    project_root: &Path,
    record: &GithubVerificationRecord,
) -> Result<()> {
    let path = github_feedback_path(project_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut line = serde_json::to_string(record).context("failed to serialize GitHub feedback")?;
    line.push('\n');
    use std::io::Write;
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?
        .write_all(line.as_bytes())
        .with_context(|| format!("failed to append {}", path.display()))?;
    Ok(())
}

pub(crate) fn summarize_github_feedback_for_tasks(
    project_root: &Path,
    tasks: &[Task],
) -> Result<GithubFeedbackSnapshot> {
    let records = load_github_feedback(project_root)?;
    Ok(summarize_github_feedback_records(tasks, &records))
}

pub(crate) fn summarize_github_feedback_records(
    tasks: &[Task],
    records: &[GithubVerificationRecord],
) -> GithubFeedbackSnapshot {
    let tasks_by_id = tasks
        .iter()
        .map(|task| (task.id, task))
        .collect::<HashMap<_, _>>();
    let mut snapshot = GithubFeedbackSnapshot::default();

    for record in records {
        let Some(task) = tasks_by_id.get(&record.task_id) else {
            snapshot.warnings.push(GithubFeedbackWarning {
                kind: GithubFeedbackWarningKind::UnknownTask,
                task_id: record.task_id,
                check_name: record.check_name.clone(),
                reason: format!(
                    "GitHub check '{}' references unknown task #{}",
                    record.check_name, record.task_id
                ),
            });
            continue;
        };

        if let (Some(record_commit), Some(task_commit)) =
            (record.commit.as_deref(), task.commit.as_deref())
            && !git_ref_matches(record_commit, task_commit)
        {
            snapshot.warnings.push(GithubFeedbackWarning {
                kind: GithubFeedbackWarningKind::StaleCommit,
                task_id: record.task_id,
                check_name: record.check_name.clone(),
                reason: format!(
                    "GitHub check '{}' for task #{} targets stale commit {}; current task commit is {}",
                    record.check_name, record.task_id, record_commit, task_commit
                ),
            });
            continue;
        }

        if let (Some(record_branch), Some(task_branch)) =
            (record.branch.as_deref(), task.branch.as_deref())
            && record_branch != task_branch
        {
            snapshot.warnings.push(GithubFeedbackWarning {
                kind: GithubFeedbackWarningKind::StaleBranch,
                task_id: record.task_id,
                check_name: record.check_name.clone(),
                reason: format!(
                    "GitHub check '{}' for task #{} targets branch {}; current task branch is {}",
                    record.check_name, record.task_id, record_branch, task_branch
                ),
            });
            continue;
        }

        if record.is_failure() {
            snapshot.failed.insert(record.task_id, record.clone());
            snapshot.passed.remove(&record.task_id);
        } else if record.is_success() {
            snapshot.failed.remove(&record.task_id);
            snapshot.passed.insert(record.task_id, record.clone());
        } else {
            snapshot.warnings.push(GithubFeedbackWarning {
                kind: GithubFeedbackWarningKind::UnknownStatus,
                task_id: record.task_id,
                check_name: record.check_name.clone(),
                reason: format!(
                    "GitHub check '{}' for task #{} has unknown status '{}'",
                    record.check_name, record.task_id, record.status
                ),
            });
        }
    }

    snapshot
}

pub(crate) fn summarize_release_github_feedback(
    project_root: &Path,
    current_commit: Option<&str>,
) -> Result<GithubReleaseFeedbackSummary> {
    let records = load_github_feedback(project_root)?;
    Ok(summarize_release_github_feedback_records(
        &records,
        current_commit,
        chrono::Utc::now().timestamp().max(0) as u64,
    ))
}

pub(crate) fn summarize_release_github_feedback_records(
    records: &[GithubVerificationRecord],
    current_commit: Option<&str>,
    now_secs: u64,
) -> GithubReleaseFeedbackSummary {
    let mut latest_current = BTreeMap::<String, GithubReleaseFeedbackItem>::new();
    let mut stale = Vec::new();

    for record in records {
        let item = release_feedback_item(record, now_secs);
        if let (Some(record_commit), Some(current_commit)) =
            (record.commit.as_deref(), current_commit)
            && !git_ref_matches(record_commit, current_commit)
        {
            stale.push(item);
            continue;
        }
        latest_current.insert(record.check_name.clone(), item);
    }

    stale.sort_by(|left, right| {
        left.check_name
            .cmp(&right.check_name)
            .then_with(|| left.commit.cmp(&right.commit))
    });

    let mut failing = Vec::new();
    let mut warnings = Vec::new();
    for item in latest_current.into_values() {
        if GithubVerificationRecord::status_is_failure(&item.status) {
            failing.push(item);
        } else if GithubVerificationRecord::status_is_warning(&item.status) {
            warnings.push(item);
        }
    }

    GithubReleaseFeedbackSummary {
        current_commit: current_commit.map(str::to_string),
        clean: failing.is_empty() && warnings.is_empty(),
        failing,
        warnings,
        stale,
    }
}

pub(crate) fn active_github_blockers_for_tasks(
    project_root: &Path,
    tasks: &[&Task],
) -> Vec<GithubVerificationRecord> {
    let owned = tasks.iter().map(|task| (*task).clone()).collect::<Vec<_>>();
    summarize_github_feedback_for_tasks(project_root, &owned)
        .map(|snapshot| {
            tasks
                .iter()
                .filter_map(|task| snapshot.failed.get(&task.id).cloned())
                .collect()
        })
        .unwrap_or_default()
}

impl GithubVerificationRecord {
    fn status_is_failure(status: &str) -> bool {
        matches!(
            normalize_status(status).as_str(),
            "failure" | "failed" | "error" | "cancelled" | "timed_out"
        )
    }

    fn status_is_success(status: &str) -> bool {
        matches!(
            normalize_status(status).as_str(),
            "success" | "succeeded" | "passed" | "pass"
        )
    }

    fn status_is_warning(status: &str) -> bool {
        matches!(
            normalize_status(status).as_str(),
            "warning" | "warn" | "neutral" | "skipped" | "pending" | "queued"
        )
    }

    pub(crate) fn is_failure(&self) -> bool {
        Self::status_is_failure(&self.status)
    }

    pub(crate) fn is_success(&self) -> bool {
        Self::status_is_success(&self.status)
    }

    pub(crate) fn status_summary(&self) -> String {
        let branch = self.branch.as_deref().unwrap_or("unknown-branch");
        let commit = self.commit.as_deref().unwrap_or("unknown-commit");
        if self.is_failure() {
            format!(
                "GitHub check failed: {} on {}@{}",
                self.check_name, branch, commit
            )
        } else {
            format!(
                "GitHub check passed: {} on {}@{}",
                self.check_name, branch, commit
            )
        }
    }

    pub(crate) fn blocked_on_summary(&self) -> String {
        let mut summary = self.status_summary();
        if let Some(details) = self
            .details
            .as_deref()
            .filter(|details| !details.is_empty())
        {
            summary.push_str(&format!(" ({details})"));
        }
        summary
    }

    pub(crate) fn next_action_summary(&self) -> String {
        self.next_action
            .clone()
            .unwrap_or_else(|| format!("Fix failing GitHub check '{}'", self.check_name))
    }

    pub(crate) fn intervention_line(&self) -> String {
        format!(
            "#{} {}. Next action: {}",
            self.task_id,
            self.blocked_on_summary(),
            self.next_action_summary()
        )
    }
}

fn release_feedback_item(
    record: &GithubVerificationRecord,
    now_secs: u64,
) -> GithubReleaseFeedbackItem {
    GithubReleaseFeedbackItem {
        check_name: record.check_name.clone(),
        status: record.status.clone(),
        commit: record.commit.clone(),
        age_secs: record.ts.map(|ts| now_secs.saturating_sub(ts)),
        next_action: record.next_action.clone(),
        details: record.details.clone(),
    }
}

fn normalize_status(status: &str) -> String {
    status.trim().to_ascii_lowercase().replace('-', "_")
}

fn git_ref_matches(left: &str, right: &str) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: u32, branch: &str, commit: &str) -> Task {
        Task {
            id,
            title: format!("Task {id}"),
            status: "review".to_string(),
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
            tags: Vec::new(),
            depends_on: Vec::new(),
            review_owner: Some("manager".to_string()),
            blocked_on: None,
            worktree_path: None,
            branch: Some(branch.to_string()),
            commit: Some(commit.to_string()),
            artifacts: Vec::new(),
            next_action: None,
            scheduled_for: None,
            cron_schedule: None,
            cron_last_run: None,
            completed: None,
            description: String::new(),
            batty_config: None,
            source_path: PathBuf::from("task.md"),
        }
    }

    fn record(task_id: u32, status: &str, commit: &str) -> GithubVerificationRecord {
        GithubVerificationRecord {
            task_id,
            branch: Some(format!("eng-1/{task_id}")),
            commit: Some(commit.to_string()),
            check_name: "ci/test".to_string(),
            status: status.to_string(),
            next_action: Some("fix CI".to_string()),
            details: None,
            ts: None,
        }
    }

    #[test]
    fn failed_check_becomes_task_blocker() {
        let tasks = vec![task(42, "eng-1/42", "abcdef1")];
        let snapshot =
            summarize_github_feedback_records(&tasks, &[record(42, "failure", "abcdef1")]);

        assert!(snapshot.failed.contains_key(&42));
        assert!(snapshot.warnings.is_empty());
    }

    #[test]
    fn passing_record_clears_prior_failure() {
        let tasks = vec![task(42, "eng-1/42", "abcdef1")];
        let records = vec![
            record(42, "failure", "abcdef1"),
            record(42, "success", "abcdef1"),
        ];
        let snapshot = summarize_github_feedback_records(&tasks, &records);

        assert!(!snapshot.failed.contains_key(&42));
        assert!(snapshot.passed.contains_key(&42));
    }

    #[test]
    fn stale_commit_is_warning_not_blocker() {
        let tasks = vec![task(42, "eng-1/42", "abcdef1")];
        let snapshot =
            summarize_github_feedback_records(&tasks, &[record(42, "failure", "deadbee")]);

        assert!(!snapshot.failed.contains_key(&42));
        assert_eq!(
            snapshot.warnings[0].kind,
            GithubFeedbackWarningKind::StaleCommit
        );
    }

    #[test]
    fn unknown_task_is_warning_not_blocker() {
        let tasks = vec![task(42, "eng-1/42", "abcdef1")];
        let snapshot =
            summarize_github_feedback_records(&tasks, &[record(99, "failure", "abcdef1")]);

        assert!(snapshot.failed.is_empty());
        assert_eq!(
            snapshot.warnings[0].kind,
            GithubFeedbackWarningKind::UnknownTask
        );
    }

    #[test]
    fn release_feedback_reports_clean_when_no_current_warnings_or_failures() {
        let records = vec![record(42, "success", "abcdef123456")];
        let snapshot =
            summarize_release_github_feedback_records(&records, Some("abcdef123456"), 1_000);

        assert!(snapshot.clean);
        assert!(snapshot.failing.is_empty());
        assert!(snapshot.warnings.is_empty());
        assert!(snapshot.stale.is_empty());
    }

    #[test]
    fn release_feedback_classifies_failure_warning_and_stale_records() {
        let records = vec![
            GithubVerificationRecord {
                status: "failure".to_string(),
                ts: Some(900),
                ..record(42, "failure", "abcdef123456")
            },
            GithubVerificationRecord {
                check_name: "ci/lint".to_string(),
                status: "warning".to_string(),
                ts: Some(880),
                ..record(42, "warning", "abcdef123456")
            },
            GithubVerificationRecord {
                check_name: "ci/old".to_string(),
                status: "failure".to_string(),
                ts: Some(100),
                ..record(42, "failure", "deadbee")
            },
        ];
        let snapshot =
            summarize_release_github_feedback_records(&records, Some("abcdef123456"), 1_000);

        assert!(!snapshot.clean);
        assert_eq!(snapshot.failing[0].check_name, "ci/test");
        assert_eq!(snapshot.failing[0].age_secs, Some(100));
        assert_eq!(snapshot.warnings[0].check_name, "ci/lint");
        assert_eq!(snapshot.warnings[0].age_secs, Some(120));
        assert_eq!(snapshot.stale[0].check_name, "ci/old");
        assert_eq!(snapshot.stale[0].age_secs, Some(900));
    }
}
