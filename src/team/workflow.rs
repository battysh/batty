//! Workflow state model for Batty-managed tasks.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

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
            next_action: Some("address review feedback".to_string()),
        };

        let json = serde_json::to_string(&meta).unwrap();
        assert!(json.contains("\"state\":\"in_progress\""));
        assert!(json.contains("\"review_disposition\":\"changes_requested\""));

        let decoded: WorkflowMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, meta);
    }
}
