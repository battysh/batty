#![cfg_attr(not(test), allow(dead_code))]

//! Review and merge transitions for Batty-managed workflow metadata.

use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde::{Deserialize, Serialize};

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
}
