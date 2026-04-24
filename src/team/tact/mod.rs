use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;

pub mod parser;
pub mod prompt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TactPrompt {
    pub board_summary: String,
    pub recent_completions: Vec<String>,
    pub roadmap_priorities: Vec<String>,
    pub idle_count: usize,
    pub dispatchable_count: usize,
}

/// A parsed task specification from the architect's planning response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSpec {
    pub title: String,
    pub body: String,
    pub priority: Option<String>,
    pub depends_on: Vec<u32>,
    pub tags: Vec<String>,
}

pub type GeneratedTask = TaskSpec;

pub use parser::{create_board_tasks, parse_planning_response};
pub use prompt::{
    PLANNING_RESPONSE_FORMAT, compose_planning_prompt, compose_planning_prompt_with_blockers,
};

pub fn dispatchable_task_count(
    board_dir: &Path,
    members: &[crate::team::hierarchy::MemberInstance],
) -> Result<usize> {
    Ok(crate::team::resolver::engineer_dispatchable_tasks(board_dir, members)?.len())
}

pub fn should_trigger_planning_cycle(idle_engineers: usize, dispatchable_tasks: usize) -> bool {
    idle_engineers > dispatchable_tasks
}

pub fn should_trigger(idle_engineers: usize, dispatchable_tasks: usize) -> bool {
    should_trigger_planning_cycle(idle_engineers, dispatchable_tasks)
}

pub fn compose_prompt(ctx: &TactPrompt) -> String {
    prompt::compose_prompt(ctx)
}

pub fn parse_task_specs(response: &str) -> Vec<TaskSpec> {
    parser::parse_task_specs(response)
}

pub fn planning_cycle_ready(
    active: bool,
    last_fired: Option<Instant>,
    cooldown: Duration,
    idle_engineers: usize,
    dispatchable_tasks: usize,
) -> bool {
    if active || !should_trigger_planning_cycle(idle_engineers, dispatchable_tasks) {
        return false;
    }

    last_fired.is_none_or(|last| last.elapsed() >= cooldown)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::config::TeamConfig;
    use crate::team::hierarchy::resolve_hierarchy;

    fn solo_members() -> Vec<crate::team::hierarchy::MemberInstance> {
        let config: TeamConfig = serde_yaml::from_str(
            r#"
name: solo
roles:
  - name: engineer
    role_type: engineer
    agent: codex
    instances: 1
"#,
        )
        .unwrap();
        resolve_hierarchy(&config).unwrap()
    }

    #[test]
    fn trigger_fires_when_idle_exceeds_dispatchable() {
        assert!(should_trigger_planning_cycle(3, 1));
    }

    #[test]
    fn trigger_does_not_fire_when_board_has_enough_tasks() {
        assert!(!should_trigger_planning_cycle(2, 2));
        assert!(!should_trigger_planning_cycle(1, 3));
    }

    #[test]
    fn cooldown_prevents_rapid_retrigger() {
        assert!(!planning_cycle_ready(
            false,
            Some(Instant::now()),
            Duration::from_secs(120),
            3,
            0,
        ));
    }

    #[test]
    fn planning_cycle_ready_when_cooldown_elapsed() {
        assert!(planning_cycle_ready(
            false,
            Some(Instant::now() - Duration::from_secs(121)),
            Duration::from_secs(120),
            3,
            0,
        ));
    }

    #[test]
    fn dispatchable_task_count_uses_workflow_resolver() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        std::fs::create_dir_all(board_dir.join("tasks")).unwrap();
        std::fs::write(
            board_dir.join("tasks/001-runnable.md"),
            "---\nid: 1\ntitle: runnable\nstatus: backlog\npriority: medium\nclass: standard\n---\n\nBody.\n",
        )
        .unwrap();
        std::fs::write(
            board_dir.join("tasks/002-blocked.md"),
            "---\nid: 2\ntitle: blocked\nstatus: blocked\npriority: medium\nclass: standard\nblocked: waiting\n---\n\nBody.\n",
        )
        .unwrap();

        let count = dispatchable_task_count(board_dir, &solo_members()).unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn dispatchable_task_count_excludes_manual_blocked_todo() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path();
        std::fs::create_dir_all(board_dir.join("tasks")).unwrap();
        std::fs::write(
            board_dir.join("tasks/001-runnable-a.md"),
            "---\nid: 1\ntitle: runnable-a\nstatus: todo\npriority: medium\nclass: standard\n---\n\nBody.\n",
        )
        .unwrap();
        std::fs::write(
            board_dir.join("tasks/002-runnable-b.md"),
            "---\nid: 2\ntitle: runnable-b\nstatus: todo\npriority: medium\nclass: standard\n---\n\nBody.\n",
        )
        .unwrap();
        std::fs::write(
            board_dir.join("tasks/003-manual.md"),
            "---\nid: 3\ntitle: manual\nstatus: todo\npriority: medium\nblocked: manual provider-console token rotation\nclass: standard\n---\n\nBody.\n",
        )
        .unwrap();

        let count = dispatchable_task_count(board_dir, &solo_members()).unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn test_should_trigger_true() {
        assert!(should_trigger(3, 1));
    }

    #[test]
    fn test_should_trigger_false() {
        assert!(!should_trigger(1, 3));
    }

    #[test]
    fn test_should_trigger_equal() {
        assert!(!should_trigger(2, 2));
    }

    #[test]
    fn prompt_request_count_uses_dispatchable_deficit() {
        let prompt = compose_prompt(&TactPrompt {
            board_summary: "todo=3, dispatchable_tasks=1, idle_engineers=2".to_string(),
            recent_completions: vec!["Finished parser".to_string()],
            roadmap_priorities: vec!["Ship tact".to_string()],
            idle_count: 2,
            dispatchable_count: 1,
        });
        assert!(prompt.contains("Please specify 1 new tasks"));
    }
}
