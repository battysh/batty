//! WIP limit enforcement and per-engineer active task counting.

use std::path::Path;

use anyhow::Result;

use super::super::*;

impl TeamDaemon {
    pub(in super::super) fn engineer_active_board_task_ids(
        &self,
        board_dir: &Path,
        engineer: &str,
    ) -> Result<Vec<u32>> {
        let mut ids: Vec<u32> = crate::task::load_tasks_from_dir(&board_dir.join("tasks"))?
            .into_iter()
            .filter(|task| {
                (matches!(task.status.as_str(), "todo" | "in-progress")
                    && task.claimed_by.as_deref() == Some(engineer))
                    || (task.status == "review" && task.review_owner.as_deref() == Some(engineer))
            })
            .map(|task| task.id)
            .collect();
        ids.sort_unstable();
        Ok(ids)
    }

    pub(in super::super) fn engineer_active_board_item_count(
        &self,
        board_dir: &Path,
        engineer: &str,
    ) -> Result<u32> {
        Ok(self
            .engineer_active_board_task_ids(board_dir, engineer)?
            .len() as u32)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::team::test_support::{TestDaemonBuilder, write_owned_task_file};

    fn write_review_task(project_root: &Path, id: u32, review_owner: &str) {
        let tasks_dir = project_root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join(format!("{id:03}-review-task.md")),
            format!(
                "---\nid: {id}\ntitle: review-task-{id}\nstatus: review\npriority: high\nclaimed_by: someone\nreview_owner: {review_owner}\nclass: standard\n---\n\nTask.\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn counts_todo_claimed_by_engineer() {
        let tmp = tempfile::tempdir().unwrap();
        write_owned_task_file(tmp.path(), 1, "task-a", "todo", "eng-1");
        let daemon = TestDaemonBuilder::new(tmp.path()).build();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");

        assert_eq!(
            daemon
                .engineer_active_board_item_count(&board_dir, "eng-1")
                .unwrap(),
            1
        );
    }

    #[test]
    fn counts_in_progress_claimed_by_engineer() {
        let tmp = tempfile::tempdir().unwrap();
        write_owned_task_file(tmp.path(), 1, "task-a", "in-progress", "eng-1");
        let daemon = TestDaemonBuilder::new(tmp.path()).build();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");

        assert_eq!(
            daemon
                .engineer_active_board_item_count(&board_dir, "eng-1")
                .unwrap(),
            1
        );
    }

    #[test]
    fn counts_review_by_review_owner() {
        let tmp = tempfile::tempdir().unwrap();
        write_review_task(tmp.path(), 1, "eng-1");
        let daemon = TestDaemonBuilder::new(tmp.path()).build();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");

        assert_eq!(
            daemon
                .engineer_active_board_item_count(&board_dir, "eng-1")
                .unwrap(),
            1
        );
    }

    #[test]
    fn ignores_done_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        write_owned_task_file(tmp.path(), 1, "done-task", "done", "eng-1");
        let daemon = TestDaemonBuilder::new(tmp.path()).build();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");

        assert_eq!(
            daemon
                .engineer_active_board_item_count(&board_dir, "eng-1")
                .unwrap(),
            0
        );
    }

    #[test]
    fn ignores_backlog_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        write_owned_task_file(tmp.path(), 1, "backlog-task", "backlog", "eng-1");
        let daemon = TestDaemonBuilder::new(tmp.path()).build();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");

        assert_eq!(
            daemon
                .engineer_active_board_item_count(&board_dir, "eng-1")
                .unwrap(),
            0
        );
    }

    #[test]
    fn ignores_tasks_claimed_by_other_engineer() {
        let tmp = tempfile::tempdir().unwrap();
        write_owned_task_file(tmp.path(), 1, "other-task", "in-progress", "eng-2");
        let daemon = TestDaemonBuilder::new(tmp.path()).build();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");

        assert_eq!(
            daemon
                .engineer_active_board_item_count(&board_dir, "eng-1")
                .unwrap(),
            0
        );
    }

    #[test]
    fn counts_multiple_active_items() {
        let tmp = tempfile::tempdir().unwrap();
        write_owned_task_file(tmp.path(), 1, "todo-task", "todo", "eng-1");
        write_owned_task_file(tmp.path(), 2, "wip-task", "in-progress", "eng-1");
        write_review_task(tmp.path(), 3, "eng-1");
        let daemon = TestDaemonBuilder::new(tmp.path()).build();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");

        assert_eq!(
            daemon
                .engineer_active_board_item_count(&board_dir, "eng-1")
                .unwrap(),
            3
        );
        assert_eq!(
            daemon
                .engineer_active_board_task_ids(&board_dir, "eng-1")
                .unwrap(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn zero_when_no_tasks_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = TestDaemonBuilder::new(tmp.path()).build();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");
        // Ensure the tasks directory exists but is empty
        std::fs::create_dir_all(board_dir.join("tasks")).unwrap();

        assert_eq!(
            daemon
                .engineer_active_board_item_count(&board_dir, "eng-1")
                .unwrap(),
            0
        );
    }
}
