//! WIP limit enforcement and per-engineer active task counting.

use std::path::Path;

use anyhow::Result;

use super::super::*;

impl TeamDaemon {
    pub(in super::super) fn engineer_active_board_item_count(
        &self,
        board_dir: &Path,
        engineer: &str,
    ) -> Result<u32> {
        let tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks"))?;
        Ok(tasks
            .into_iter()
            .filter(|task| {
                (matches!(task.status.as_str(), "todo" | "in-progress")
                    && task.claimed_by.as_deref() == Some(engineer))
                    || (task.status == "review" && task.review_owner.as_deref() == Some(engineer))
            })
            .count() as u32)
    }
}
