//! Workflow state model for Batty-managed tasks.
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

#[cfg(test)]
use super::review::MergeDisposition;
use super::review::ReviewState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    #[default]
    Backlog,
    Todo,
    #[serde(rename = "in_progress")]
    InProgress,
    Review,
    Blocked,
    Done,
    Archived,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDisposition {
    Approved,
    ChangesRequested,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WorkflowMeta {
    #[serde(default)]
    pub state: TaskState,
    #[serde(default)]
    pub execution_owner: Option<String>,
    #[serde(default)]
    pub review_owner: Option<String>,
    #[serde(default)]
    pub depends_on: Vec<u32>,
    #[serde(default)]
    pub blocked_on: Option<String>,
    #[serde(default)]
    pub worktree_path: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub artifacts: Vec<String>,
    #[serde(default)]
    pub review_disposition: Option<ReviewDisposition>,
    #[serde(default)]
    pub review: Option<ReviewState>,
    #[serde(default)]
    pub next_action: Option<String>,
}

impl WorkflowMeta {
    pub fn is_runnable(&self, done_tasks: &HashSet<u32>) -> bool {
        self.state == TaskState::Todo
            && self
                .depends_on
                .iter()
                .all(|dependency| done_tasks.contains(dependency))
    }

    pub fn transition(&mut self, to: TaskState) -> Result<(), String> {
        can_transition(self.state, to)?;
        self.state = to;
        Ok(())
    }
}

pub fn can_transition(from: TaskState, to: TaskState) -> Result<(), String> {
    if from == to {
        return Ok(());
    }

    let allowed = matches!(
        (from, to),
        (TaskState::Backlog, TaskState::Todo)
            | (TaskState::Backlog, TaskState::Archived)
            | (TaskState::Todo, TaskState::Backlog)
            | (TaskState::Todo, TaskState::InProgress)
            | (TaskState::Todo, TaskState::Blocked)
            | (TaskState::Todo, TaskState::Archived)
            | (TaskState::InProgress, TaskState::Todo)
            | (TaskState::InProgress, TaskState::Review)
            | (TaskState::InProgress, TaskState::Blocked)
            | (TaskState::Review, TaskState::InProgress)
            | (TaskState::Review, TaskState::Blocked)
            | (TaskState::Review, TaskState::Done)
            | (TaskState::Review, TaskState::Archived)
            | (TaskState::Blocked, TaskState::Todo)
            | (TaskState::Blocked, TaskState::InProgress)
            | (TaskState::Blocked, TaskState::Archived)
            | (TaskState::Done, TaskState::Archived)
    );

    if allowed {
        Ok(())
    } else {
        Err(format!("illegal task state transition: {from:?} -> {to:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_workflow_meta_has_backlog_state() {
        let meta = WorkflowMeta::default();
        assert_eq!(meta.state, TaskState::Backlog);
        assert!(meta.depends_on.is_empty());
        assert!(meta.artifacts.is_empty());
    }

    #[test]
    fn is_runnable_requires_todo_state_and_completed_dependencies() {
        let mut done_tasks = HashSet::from([1, 2]);
        let runnable = WorkflowMeta {
            state: TaskState::Todo,
            depends_on: vec![1, 2],
            ..WorkflowMeta::default()
        };
        assert!(runnable.is_runnable(&done_tasks));

        done_tasks.remove(&2);
        assert!(!runnable.is_runnable(&done_tasks));

        let wrong_state = WorkflowMeta {
            state: TaskState::InProgress,
            depends_on: vec![1],
            ..WorkflowMeta::default()
        };
        assert!(!wrong_state.is_runnable(&HashSet::from([1])));
    }

    #[test]
    fn legal_transitions_pass() {
        let legal = [
            (TaskState::Backlog, TaskState::Todo),
            (TaskState::Backlog, TaskState::Archived),
            (TaskState::Todo, TaskState::InProgress),
            (TaskState::Todo, TaskState::Blocked),
            (TaskState::InProgress, TaskState::Review),
            (TaskState::InProgress, TaskState::Blocked),
            (TaskState::Review, TaskState::Done),
            (TaskState::Review, TaskState::InProgress),
            (TaskState::Review, TaskState::Archived),
            (TaskState::Blocked, TaskState::Todo),
            (TaskState::Done, TaskState::Archived),
        ];

        for (from, to) in legal {
            assert!(can_transition(from, to).is_ok());
        }
    }

    #[test]
    fn illegal_transitions_fail() {
        let illegal = [
            (TaskState::Backlog, TaskState::Done),
            (TaskState::Backlog, TaskState::Review),
            (TaskState::Todo, TaskState::Done),
            (TaskState::InProgress, TaskState::Done),
            (TaskState::Done, TaskState::Todo),
            (TaskState::Archived, TaskState::Todo),
        ];

        for (from, to) in illegal {
            assert!(can_transition(from, to).is_err());
        }
    }

    #[test]
    fn transition_updates_state_after_validation() {
        let mut meta = WorkflowMeta {
            state: TaskState::Todo,
            ..WorkflowMeta::default()
        };

        meta.transition(TaskState::InProgress).unwrap();
        assert_eq!(meta.state, TaskState::InProgress);
        assert!(meta.transition(TaskState::Archived).is_err());
        assert_eq!(meta.state, TaskState::InProgress);
    }

    #[test]
    fn serde_round_trip_preserves_workflow_meta() {
        let meta = WorkflowMeta {
            state: TaskState::InProgress,
            execution_owner: Some("eng-1-2".to_string()),
            review_owner: Some("manager-1".to_string()),
            depends_on: vec![7, 8],
            blocked_on: Some("waiting for api".to_string()),
            worktree_path: Some("/tmp/eng-1-2".to_string()),
            branch: Some("eng-1-2/task-19".to_string()),
            commit: Some("abc1234".to_string()),
            artifacts: vec!["artifacts/test.log".to_string()],
            review_disposition: Some(ReviewDisposition::ChangesRequested),
            review: Some(ReviewState {
                reviewer: "manager-1".to_string(),
                packet_ref: Some("review/packet-7.json".to_string()),
                disposition: MergeDisposition::ReworkRequired,
                notes: Some("needs another pass".to_string()),
                reviewed_at: None,
                nudge_sent: false,
            }),
            next_action: Some("address review feedback".to_string()),
        };

        let json = serde_json::to_string(&meta).unwrap();
        assert!(json.contains("\"state\":\"in_progress\""));
        assert!(json.contains("\"review_disposition\":\"changes_requested\""));
        assert!(json.contains("\"packet_ref\":\"review/packet-7.json\""));
        assert!(json.contains("\"disposition\":\"rework_required\""));

        let decoded: WorkflowMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, meta);
    }

    // --- self-transition ---

    #[test]
    fn self_transition_is_allowed() {
        assert!(can_transition(TaskState::Backlog, TaskState::Backlog).is_ok());
        assert!(can_transition(TaskState::InProgress, TaskState::InProgress).is_ok());
        assert!(can_transition(TaskState::Done, TaskState::Done).is_ok());
    }

    // --- archived is terminal ---

    #[test]
    fn archived_cannot_transition_to_any_state() {
        let targets = [
            TaskState::Backlog,
            TaskState::Todo,
            TaskState::InProgress,
            TaskState::Review,
            TaskState::Blocked,
            TaskState::Done,
        ];
        for target in targets {
            assert!(
                can_transition(TaskState::Archived, target).is_err(),
                "archived -> {target:?} should be illegal"
            );
        }
    }

    // --- transition error message ---

    #[test]
    fn transition_error_message_contains_states() {
        let err = can_transition(TaskState::Backlog, TaskState::Done).unwrap_err();
        assert!(err.contains("Backlog"));
        assert!(err.contains("Done"));
    }

    // --- is_runnable edge cases ---

    #[test]
    fn is_runnable_with_no_deps_at_todo() {
        let meta = WorkflowMeta {
            state: TaskState::Todo,
            ..WorkflowMeta::default()
        };
        assert!(meta.is_runnable(&HashSet::new()));
    }

    #[test]
    fn is_runnable_at_backlog_is_false() {
        let meta = WorkflowMeta::default(); // Backlog
        assert!(!meta.is_runnable(&HashSet::new()));
    }

    #[test]
    fn is_runnable_at_done_is_false() {
        let meta = WorkflowMeta {
            state: TaskState::Done,
            ..WorkflowMeta::default()
        };
        assert!(!meta.is_runnable(&HashSet::new()));
    }

    // --- multi-step transitions ---

    #[test]
    fn full_lifecycle_transition_chain() {
        let mut meta = WorkflowMeta::default(); // Backlog
        meta.transition(TaskState::Todo).unwrap();
        meta.transition(TaskState::InProgress).unwrap();
        meta.transition(TaskState::Review).unwrap();
        meta.transition(TaskState::Done).unwrap();
        meta.transition(TaskState::Archived).unwrap();
        assert_eq!(meta.state, TaskState::Archived);
    }

    #[test]
    fn blocked_to_todo_to_in_progress_chain() {
        let mut meta = WorkflowMeta {
            state: TaskState::Todo,
            ..WorkflowMeta::default()
        };
        meta.transition(TaskState::Blocked).unwrap();
        meta.transition(TaskState::Todo).unwrap();
        meta.transition(TaskState::InProgress).unwrap();
        assert_eq!(meta.state, TaskState::InProgress);
    }

    #[test]
    fn review_rework_cycle() {
        let mut meta = WorkflowMeta {
            state: TaskState::InProgress,
            ..WorkflowMeta::default()
        };
        meta.transition(TaskState::Review).unwrap();
        meta.transition(TaskState::InProgress).unwrap(); // rework
        meta.transition(TaskState::Review).unwrap();
        meta.transition(TaskState::Done).unwrap();
        assert_eq!(meta.state, TaskState::Done);
    }

    // --- TaskState serde ---

    #[test]
    fn task_state_default_is_backlog() {
        let state: TaskState = Default::default();
        assert_eq!(state, TaskState::Backlog);
    }

    #[test]
    fn task_state_serde_round_trip() {
        let states = [
            TaskState::Backlog,
            TaskState::Todo,
            TaskState::InProgress,
            TaskState::Review,
            TaskState::Blocked,
            TaskState::Done,
            TaskState::Archived,
        ];
        for state in states {
            let json = serde_json::to_string(&state).unwrap();
            let decoded: TaskState = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, state);
        }
    }

    // --- ReviewDisposition serde ---

    #[test]
    fn review_disposition_serde_round_trip() {
        let dispositions = [
            ReviewDisposition::Approved,
            ReviewDisposition::ChangesRequested,
            ReviewDisposition::Rejected,
        ];
        for disp in dispositions {
            let json = serde_json::to_string(&disp).unwrap();
            let decoded: ReviewDisposition = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, disp);
        }
    }
}
