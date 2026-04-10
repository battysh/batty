#![cfg_attr(not(test), allow(dead_code))]

//! Review and merge transitions for Batty-managed workflow metadata.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::task::Task;

use super::workflow::{ReviewDisposition, TaskState, WorkflowMeta, can_transition};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeDisposition {
    MergeReady,
    ReworkRequired,
    Discarded,
    Escalated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewState {
    pub reviewer: String,
    #[serde(default)]
    pub packet_ref: Option<String>,
    pub disposition: MergeDisposition,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub reviewed_at: Option<u64>,
    #[serde(default)]
    pub nudge_sent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewEligibility {
    Eligible,
    MissingMetadata { reasons: Vec<String> },
    AlreadyMerged { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReviewNormalizationStep {
    Merge,
    Archive,
    Rework,
}

impl ReviewNormalizationStep {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Archive => "archive",
            Self::Rework => "rework",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StaleReviewState {
    pub(crate) reason: String,
    pub(crate) next_step: ReviewNormalizationStep,
}

impl StaleReviewState {
    pub(crate) fn status_next_action(&self) -> String {
        format!(
            "stale review -> {}: {}",
            self.next_step.as_str(),
            self.reason
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReviewQueueState {
    Current,
    Stale(StaleReviewState),
}

pub fn apply_review(
    meta: &mut WorkflowMeta,
    disposition: MergeDisposition,
    reviewer: &str,
) -> Result<(), String> {
    validate_review_readiness(meta)?;

    let packet_ref = meta
        .review
        .as_ref()
        .and_then(|review| review.packet_ref.clone());
    let notes = meta.review.as_ref().and_then(|review| review.notes.clone());

    let (next_state, review_disposition, blocked_on) = match disposition {
        MergeDisposition::MergeReady => (TaskState::Done, Some(ReviewDisposition::Approved), None),
        MergeDisposition::ReworkRequired => (
            TaskState::InProgress,
            Some(ReviewDisposition::ChangesRequested),
            None,
        ),
        MergeDisposition::Discarded => {
            (TaskState::Archived, Some(ReviewDisposition::Rejected), None)
        }
        MergeDisposition::Escalated => (
            TaskState::Blocked,
            None,
            Some(format!("escalated by {reviewer}")),
        ),
    };

    can_transition(meta.state, next_state)?;
    meta.state = next_state;
    meta.review_owner = Some(reviewer.to_string());
    meta.review_disposition = review_disposition;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    meta.review = Some(ReviewState {
        reviewer: reviewer.to_string(),
        packet_ref,
        disposition,
        notes,
        reviewed_at: Some(now),
        nudge_sent: false,
    });
    meta.blocked_on = blocked_on;

    Ok(())
}

pub fn validate_review_readiness(meta: &WorkflowMeta) -> Result<(), String> {
    if meta.state == TaskState::Review {
        Ok(())
    } else {
        Err(format!(
            "task must be in Review state before applying review, found {:?}",
            meta.state
        ))
    }
}

pub fn validate_review_candidate(
    project_root: &Path,
    meta: &WorkflowMeta,
) -> anyhow::Result<ReviewEligibility> {
    let branch = meta
        .branch
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let commit = meta
        .commit
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let mut reasons = Vec::new();

    if branch.is_none() {
        reasons.push("branch metadata missing".to_string());
    }
    if commit.is_none() {
        reasons.push("commit metadata missing".to_string());
    }
    if !reasons.is_empty() {
        return Ok(ReviewEligibility::MissingMetadata { reasons });
    }

    let branch = branch.expect("branch checked above");
    let commit = commit.expect("commit checked above");

    let commit_merged =
        match crate::team::git_cmd::merge_base_is_ancestor(project_root, commit, "main") {
            Ok(merged) => merged,
            Err(_) => {
                return Ok(ReviewEligibility::MissingMetadata {
                    reasons: vec![format!("commit metadata is invalid: `{commit}`")],
                });
            }
        };
    if commit_merged {
        return Ok(ReviewEligibility::AlreadyMerged {
            reason: format!("commit `{commit}` is already on main"),
        });
    }

    let branch_merged =
        match crate::team::task_loop::branch_is_merged_into(project_root, branch, "main") {
            Ok(merged) => merged,
            Err(_) => {
                return Ok(ReviewEligibility::MissingMetadata {
                    reasons: vec![format!("branch metadata is invalid: `{branch}`")],
                });
            }
        };
    if branch_merged {
        return Ok(ReviewEligibility::AlreadyMerged {
            reason: format!("branch `{branch}` is already merged into main"),
        });
    }

    Ok(ReviewEligibility::Eligible)
}

pub(crate) fn classify_review_task(
    project_root: &Path,
    task: &Task,
    board_tasks: &[Task],
) -> ReviewQueueState {
    if task.status != "review" {
        return ReviewQueueState::Current;
    }

    if let Some(branch) = task.branch.as_deref() {
        let blockers = task_reference_mismatch_blockers(task.id, branch, &[]);
        if !blockers.is_empty() {
            return ReviewQueueState::Stale(StaleReviewState {
                reason: blockers.join("; "),
                next_step: ReviewNormalizationStep::Rework,
            });
        }
    }

    if let Some(ReviewEligibility::AlreadyMerged { reason }) =
        review_eligibility_for_task(project_root, task)
    {
        return ReviewQueueState::Stale(StaleReviewState {
            reason,
            next_step: ReviewNormalizationStep::Merge,
        });
    }

    let Some(engineer) = task.claimed_by.as_deref() else {
        return ReviewQueueState::Current;
    };

    let active_claims = board_tasks
        .iter()
        .filter(|candidate| candidate.id != task.id)
        .filter(|candidate| candidate.claimed_by.as_deref() == Some(engineer))
        .filter(|candidate| candidate.status != "review")
        .filter(|candidate| candidate.status != "done")
        .filter(|candidate| candidate.status != "archived")
        .collect::<Vec<_>>();

    if let Some(current_lane) = select_current_lane(engineer, &active_claims, project_root) {
        let branch_suffix = current_lane
            .branch
            .as_deref()
            .map(|branch| format!(" on branch `{branch}`"))
            .unwrap_or_default();
        return ReviewQueueState::Stale(StaleReviewState {
            reason: format!(
                "{engineer} already moved to task #{}{}",
                current_lane.id, branch_suffix
            ),
            next_step: ReviewNormalizationStep::Merge,
        });
    }

    if let Some(current_branch) = review_worktree_branch(project_root, engineer) {
        let blockers = task_reference_mismatch_blockers(task.id, &current_branch, &[]);
        if !blockers.is_empty() {
            return ReviewQueueState::Stale(StaleReviewState {
                reason: blockers.join("; "),
                next_step: ReviewNormalizationStep::Archive,
            });
        }
    }

    ReviewQueueState::Current
}

fn review_eligibility_for_task(project_root: &Path, task: &Task) -> Option<ReviewEligibility> {
    let meta = WorkflowMeta {
        state: TaskState::Review,
        branch: task.branch.clone().or_else(|| {
            task.claimed_by
                .as_deref()
                .and_then(|engineer| review_worktree_branch(project_root, engineer))
        }),
        commit: task.commit.clone().or_else(|| {
            task.claimed_by
                .as_deref()
                .and_then(|engineer| review_worktree_commit(project_root, engineer))
        }),
        ..WorkflowMeta::default()
    };
    validate_review_candidate(project_root, &meta).ok()
}

fn select_current_lane<'a>(
    engineer: &str,
    active_claims: &[&'a Task],
    project_root: &Path,
) -> Option<&'a Task> {
    if active_claims.is_empty() {
        return None;
    }

    let current_branch = review_worktree_branch(project_root, engineer);
    if let Some(current_branch) = current_branch.as_deref() {
        let mut branch_matches = active_claims.iter().copied().filter(|candidate| {
            candidate
                .branch
                .as_deref()
                .map(|branch| branch == current_branch)
                .unwrap_or_else(|| format!("{engineer}/{}", candidate.id) == current_branch)
        });
        if let Some(candidate) = branch_matches.next()
            && branch_matches.next().is_none()
        {
            return Some(candidate);
        }
        // Worktree exists but its branch does not match any active claim.
        // That usually means the engineer is still on the review branch
        // itself, so we should not mark the review as stale. Return None
        // to preserve the existing "Current" classification.
        return None;
    }

    // No worktree at all (unit tests, or the engineer has not been
    // provisioned yet). Fall back to the single unambiguous active claim if
    // there is exactly one — that is the current lane by deduction. With
    // zero or multiple active claims, return None to avoid guessing.
    if active_claims.len() == 1 {
        return Some(active_claims[0]);
    }

    None
}

fn review_worktree_branch(project_root: &Path, engineer: &str) -> Option<String> {
    let worktree_dir = review_worktree_dir(project_root, engineer);
    worktree_dir
        .is_dir()
        .then_some(worktree_dir)
        .and_then(|worktree_dir| {
            crate::team::task_loop::current_worktree_branch(&worktree_dir).ok()
        })
}

fn review_worktree_commit(project_root: &Path, engineer: &str) -> Option<String> {
    let worktree_dir = review_worktree_dir(project_root, engineer);
    if !worktree_dir.is_dir() {
        return None;
    }

    let output = std::process::Command::new("git")
        .args(["rev-parse", "--verify", "HEAD"])
        .current_dir(&worktree_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn review_worktree_dir(project_root: &Path, engineer: &str) -> std::path::PathBuf {
    project_root.join(".batty").join("worktrees").join(engineer)
}

pub(crate) fn task_reference_mismatch_blockers(
    expected_task_id: u32,
    branch_name: &str,
    commit_messages: &[String],
) -> Vec<String> {
    let branch_task_ids = extract_task_ids(branch_name);
    let commit_task_ids = commit_messages
        .iter()
        .flat_map(|message| extract_task_ids(message))
        .collect::<BTreeSet<_>>();
    let mut blockers = Vec::new();

    if !branch_task_ids.is_empty() && !branch_task_ids.contains(&expected_task_id) {
        blockers.push(format!(
            "branch `{branch_name}` references task(s) {} but assigned task is #{expected_task_id}",
            format_task_id_list(&branch_task_ids)
        ));
    }

    if !commit_task_ids.is_empty() && !commit_task_ids.contains(&expected_task_id) {
        blockers.push(format!(
            "commit messages reference task(s) {} but assigned task is #{expected_task_id}",
            format_task_id_list(&commit_task_ids)
        ));
    }

    blockers
}

fn extract_task_ids(text: &str) -> BTreeSet<u32> {
    let mut ids = BTreeSet::new();
    let hash_pattern = Regex::new(r"(?i)#(\d+)\b").expect("hash task id regex should compile");
    let task_pattern =
        Regex::new(r"(?i)\btask[-_ /]?(\d+)\b").expect("task task id regex should compile");

    for captures in hash_pattern.captures_iter(text) {
        if let Some(id) = captures
            .get(1)
            .and_then(|value| value.as_str().parse::<u32>().ok())
        {
            ids.insert(id);
        }
    }
    for captures in task_pattern.captures_iter(text) {
        if let Some(id) = captures
            .get(1)
            .and_then(|value| value.as_str().parse::<u32>().ok())
        {
            ids.insert(id);
        }
    }

    ids
}

fn format_task_id_list(task_ids: &BTreeSet<u32>) -> String {
    task_ids
        .iter()
        .map(|task_id| format!("#{task_id}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::task_loop::{
        checkout_worktree_branch_from_main, engineer_base_branch_name, setup_engineer_worktree,
    };
    use crate::team::test_support::{git_ok, init_git_repo};
    use std::fs;
    use std::process::Command;
    use tempfile::tempdir;

    fn review_meta() -> WorkflowMeta {
        WorkflowMeta {
            state: TaskState::Review,
            review: Some(ReviewState {
                reviewer: "manager-0".to_string(),
                packet_ref: Some("review/packet-1.json".to_string()),
                disposition: MergeDisposition::MergeReady,
                notes: Some("initial packet".to_string()),
                reviewed_at: None,
                nudge_sent: false,
            }),
            ..WorkflowMeta::default()
        }
    }

    #[test]
    fn merge_ready_moves_review_to_done() {
        let mut meta = review_meta();

        apply_review(&mut meta, MergeDisposition::MergeReady, "manager-1").unwrap();

        assert_eq!(meta.state, TaskState::Done);
        assert_eq!(meta.review_owner.as_deref(), Some("manager-1"));
        assert_eq!(meta.review_disposition, Some(ReviewDisposition::Approved));
        assert_eq!(meta.blocked_on, None);
        let review = meta.review.unwrap();
        assert_eq!(review.disposition, MergeDisposition::MergeReady);
        assert_eq!(review.packet_ref.as_deref(), Some("review/packet-1.json"));
    }

    #[test]
    fn rework_required_moves_review_to_in_progress() {
        let mut meta = review_meta();

        apply_review(&mut meta, MergeDisposition::ReworkRequired, "manager-1").unwrap();

        assert_eq!(meta.state, TaskState::InProgress);
        assert_eq!(
            meta.review_disposition,
            Some(ReviewDisposition::ChangesRequested)
        );
        assert_eq!(meta.blocked_on, None);
        assert_eq!(
            meta.review.as_ref().map(|review| review.disposition),
            Some(MergeDisposition::ReworkRequired)
        );
    }

    #[test]
    fn discarded_moves_review_to_archived() {
        let mut meta = review_meta();

        apply_review(&mut meta, MergeDisposition::Discarded, "manager-1").unwrap();

        assert_eq!(meta.state, TaskState::Archived);
        assert_eq!(meta.review_disposition, Some(ReviewDisposition::Rejected));
        assert_eq!(meta.blocked_on, None);
        assert_eq!(
            meta.review.as_ref().map(|review| review.disposition),
            Some(MergeDisposition::Discarded)
        );
    }

    #[test]
    fn escalated_moves_review_to_blocked() {
        let mut meta = review_meta();

        apply_review(&mut meta, MergeDisposition::Escalated, "manager-1").unwrap();

        assert_eq!(meta.state, TaskState::Blocked);
        assert_eq!(meta.review_disposition, None);
        assert_eq!(meta.blocked_on.as_deref(), Some("escalated by manager-1"));
        assert_eq!(
            meta.review.as_ref().map(|review| review.disposition),
            Some(MergeDisposition::Escalated)
        );
    }

    #[test]
    fn apply_review_rejects_non_review_tasks() {
        let mut meta = WorkflowMeta {
            state: TaskState::InProgress,
            ..WorkflowMeta::default()
        };

        let err = apply_review(&mut meta, MergeDisposition::MergeReady, "manager-1")
            .expect_err("non-review tasks should be rejected");

        assert!(err.contains("Review state"));
        assert_eq!(meta.state, TaskState::InProgress);
        assert_eq!(meta.review_disposition, None);
        assert_eq!(meta.blocked_on, None);
        assert!(meta.review.is_none());
    }

    #[test]
    fn validate_review_readiness_rejects_non_review_state() {
        let meta = WorkflowMeta {
            state: TaskState::Todo,
            ..WorkflowMeta::default()
        };

        let err = validate_review_readiness(&meta).expect_err("todo should not be review-ready");
        assert!(err.contains("Review state"));
    }

    #[test]
    fn validate_review_candidate_rejects_missing_metadata() {
        let tmp = tempdir().unwrap();
        let repo = init_git_repo(&tmp, "review_missing_meta");
        let meta = WorkflowMeta {
            state: TaskState::Review,
            ..WorkflowMeta::default()
        };

        let eligibility = validate_review_candidate(&repo, &meta).unwrap();
        assert_eq!(
            eligibility,
            ReviewEligibility::MissingMetadata {
                reasons: vec![
                    "branch metadata missing".to_string(),
                    "commit metadata missing".to_string()
                ]
            }
        );
    }

    #[test]
    fn validate_review_candidate_rejects_already_merged_commit() {
        let tmp = tempdir().unwrap();
        let repo = init_git_repo(&tmp, "review_merged_meta");

        git_ok(&repo, &["checkout", "-b", "eng-1/task-42"]);
        fs::write(
            repo.join("src").join("review_candidate.rs"),
            "pub fn review_candidate() {}\n",
        )
        .unwrap();
        git_ok(&repo, &["add", "."]);
        git_ok(&repo, &["commit", "-m", "review candidate"]);
        let commit = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&repo)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        git_ok(&repo, &["checkout", "main"]);
        git_ok(
            &repo,
            &[
                "merge",
                "--no-ff",
                "eng-1/task-42",
                "-m",
                "merge review candidate",
            ],
        );

        let meta = WorkflowMeta {
            state: TaskState::Review,
            branch: Some("eng-1/task-42".to_string()),
            commit: Some(commit.clone()),
            ..WorkflowMeta::default()
        };

        let eligibility = validate_review_candidate(&repo, &meta).unwrap();
        assert_eq!(
            eligibility,
            ReviewEligibility::AlreadyMerged {
                reason: format!("commit `{commit}` is already on main")
            }
        );
    }

    #[test]
    fn review_state_uses_merge_disposition() {
        let state = ReviewState {
            reviewer: "manager-1".to_string(),
            packet_ref: Some("packet-42".to_string()),
            disposition: MergeDisposition::MergeReady,
            notes: Some("ready to merge".to_string()),
            reviewed_at: Some(1700000000),
            nudge_sent: false,
        };

        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"disposition\":\"merge_ready\""));
        assert!(json.contains("\"reviewed_at\":1700000000"));
    }

    #[test]
    fn task_reference_mismatch_detects_branch_and_commit_ids() {
        let blockers = task_reference_mismatch_blockers(
            497,
            "eng-1-1/task-449",
            &[
                "Task #449: implement wrong fix".to_string(),
                "follow-up for task-449".to_string(),
            ],
        );

        assert_eq!(blockers.len(), 2);
        assert!(blockers[0].contains("assigned task is #497"));
        assert!(blockers[0].contains("#449"));
        assert!(blockers[1].contains("commit messages"));
    }

    #[test]
    fn task_reference_mismatch_allows_expected_task_id() {
        let blockers = task_reference_mismatch_blockers(
            497,
            "eng-1-1/task-497",
            &[
                "Task #497: implement expected fix".to_string(),
                "follow-up for task-497".to_string(),
            ],
        );

        assert!(blockers.is_empty());
    }

    #[test]
    fn task_reference_mismatch_ignores_unlabeled_commits() {
        let blockers =
            task_reference_mismatch_blockers(497, "eng-1-1", &["refactor review flow".to_string()]);

        assert!(blockers.is_empty());
    }

    fn review_task(id: u32, claimed_by: &str) -> Task {
        Task {
            id,
            title: format!("review-task-{id}"),
            status: "review".to_string(),
            priority: "high".to_string(),
            claimed_by: Some(claimed_by.to_string()),
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
            branch: None,
            commit: None,
            artifacts: Vec::new(),
            next_action: None,
            scheduled_for: None,
            cron_schedule: None,
            cron_last_run: None,
            completed: None,
            description: String::new(),
            batty_config: None,
            source_path: Path::new("review.md").to_path_buf(),
        }
    }

    #[test]
    fn classify_review_task_keeps_live_review_current() {
        let tmp = tempdir().unwrap();
        let repo = init_git_repo(&tmp, "review-current");
        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let base_branch = engineer_base_branch_name("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
        checkout_worktree_branch_from_main(&worktree_dir, "eng-1/42").unwrap();
        fs::write(worktree_dir.join("live-review.txt"), "live review\n").unwrap();
        git_ok(&worktree_dir, &["add", "live-review.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "live review branch"]);

        let mut review = review_task(42, "eng-1");
        review.branch = Some("eng-1/42".to_string());

        assert_eq!(
            classify_review_task(&repo, &review, &[review_task(42, "eng-1")]),
            ReviewQueueState::Current
        );
    }

    #[test]
    fn classify_review_task_detects_engineer_moved_on() {
        let tmp = tempdir().unwrap();
        let repo = init_git_repo(&tmp, "review-moved-on");
        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let base_branch = engineer_base_branch_name("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
        checkout_worktree_branch_from_main(&worktree_dir, "eng-1/77").unwrap();
        fs::write(worktree_dir.join("moved-on.txt"), "moved on\n").unwrap();
        git_ok(&worktree_dir, &["add", "moved-on.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "moved on branch"]);

        let review = review_task(42, "eng-1");
        let mut active = review_task(77, "eng-1");
        active.status = "in-progress".to_string();
        active.branch = Some("eng-1/77".to_string());
        let review_board_entry = review_task(42, "eng-1");

        assert_eq!(
            classify_review_task(&repo, &review, &[review_board_entry, active]),
            ReviewQueueState::Stale(StaleReviewState {
                reason: "eng-1 already moved to task #77 on branch `eng-1/77`".to_string(),
                next_step: ReviewNormalizationStep::Merge,
            })
        );
    }

    #[test]
    fn classify_review_task_keeps_review_current_when_other_task_is_claimed() {
        let tmp = tempdir().unwrap();
        let repo = init_git_repo(&tmp, "review-other-claim");
        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let base_branch = engineer_base_branch_name("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
        checkout_worktree_branch_from_main(&worktree_dir, "eng-1/42").unwrap();
        fs::write(
            worktree_dir.join("review-still-live.txt"),
            "still on review\n",
        )
        .unwrap();
        git_ok(&worktree_dir, &["add", "review-still-live.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "keep review lane current"]);

        let review = Task {
            branch: Some("eng-1/42".to_string()),
            ..review_task(42, "eng-1")
        };
        let review_board_entry = Task {
            branch: Some("eng-1/42".to_string()),
            ..review_task(42, "eng-1")
        };
        let active = Task {
            status: "in-progress".to_string(),
            branch: Some("eng-1/77".to_string()),
            ..review_task(77, "eng-1")
        };

        assert_eq!(
            classify_review_task(&repo, &review, &[review_board_entry, active]),
            ReviewQueueState::Current
        );
    }

    #[test]
    fn classify_review_task_detects_branch_mismatch() {
        let tmp = tempdir().unwrap();
        let review = Task {
            branch: Some("eng-1/task-99".to_string()),
            ..review_task(42, "eng-1")
        };

        assert_eq!(
            classify_review_task(tmp.path(), &review, std::slice::from_ref(&review)),
            ReviewQueueState::Stale(StaleReviewState {
                reason: "branch `eng-1/task-99` references task(s) #99 but assigned task is #42"
                    .to_string(),
                next_step: ReviewNormalizationStep::Rework,
            })
        );
    }
}
