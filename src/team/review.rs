#![cfg_attr(not(test), allow(dead_code))]

//! Review and merge transitions for Batty-managed workflow metadata.

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
    meta.review = Some(ReviewState {
        reviewer: reviewer.to_string(),
        packet_ref,
        disposition,
        notes,
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
        };

        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"disposition\":\"merge_ready\""));
    }
}
