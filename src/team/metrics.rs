pub use super::status::{WorkflowMetrics, compute_metrics, format_metrics};

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::team::config::RoleType;
    use crate::team::hierarchy::MemberInstance;

    fn make_member(name: &str, role_type: RoleType) -> MemberInstance {
        MemberInstance {
            name: name.to_string(),
            role_name: name.to_string(),
            role_type,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        }
    }

    fn write_task(
        board_dir: &Path,
        id: u32,
        title: &str,
        status: &str,
        claimed_by: Option<&str>,
        blocked: Option<&str>,
        depends_on: &[u32],
    ) {
        let tasks_dir = board_dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let mut content =
            format!("---\nid: {id}\ntitle: {title}\nstatus: {status}\npriority: medium\n");
        if let Some(claimed_by) = claimed_by {
            content.push_str(&format!("claimed_by: {claimed_by}\n"));
        }
        if let Some(blocked) = blocked {
            content.push_str(&format!("blocked: {blocked}\n"));
        }
        if !depends_on.is_empty() {
            content.push_str("depends_on:\n");
            for dep in depends_on {
                content.push_str(&format!("  - {dep}\n"));
            }
        }
        content.push_str("class: standard\n---\n\nTask body.\n");
        std::fs::write(tasks_dir.join(format!("{id:03}-{title}.md")), content).unwrap();
    }

    #[test]
    fn compute_metrics_handles_empty_board() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");
        std::fs::create_dir_all(board_dir.join("tasks")).unwrap();

        let metrics = compute_metrics(&board_dir, &[]).unwrap();
        assert_eq!(metrics, WorkflowMetrics::default());
    }

    #[test]
    fn compute_metrics_counts_mixed_workflow_states() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");
        write_task(&board_dir, 1, "done-dep", "done", None, None, &[]);
        write_task(&board_dir, 2, "runnable", "todo", None, None, &[1]);
        write_task(
            &board_dir,
            3,
            "blocked",
            "blocked",
            Some("eng-1"),
            Some("waiting"),
            &[],
        );
        write_task(&board_dir, 4, "review", "review", Some("eng-2"), None, &[]);
        write_task(
            &board_dir,
            5,
            "active",
            "in-progress",
            Some("eng-1"),
            None,
            &[],
        );

        let members = vec![
            make_member("eng-1", RoleType::Engineer),
            make_member("eng-2", RoleType::Engineer),
            make_member("eng-3", RoleType::Engineer),
        ];
        let metrics = compute_metrics(&board_dir, &members).unwrap();

        assert_eq!(metrics.runnable_count, 1);
        assert_eq!(metrics.blocked_count, 1);
        assert_eq!(metrics.in_review_count, 1);
        assert_eq!(metrics.in_progress_count, 1);
        assert_eq!(metrics.idle_with_runnable, vec!["eng-3"]);
        assert!(metrics.oldest_review_age_secs.is_some());
        assert!(metrics.oldest_assignment_age_secs.is_some());
    }

    #[test]
    fn format_metrics_produces_readable_summary() {
        let text = format_metrics(&WorkflowMetrics {
            runnable_count: 2,
            blocked_count: 1,
            in_review_count: 3,
            in_progress_count: 4,
            idle_with_runnable: vec!["eng-1".to_string(), "eng-2".to_string()],
            oldest_review_age_secs: Some(120),
            oldest_assignment_age_secs: Some(360),
        });

        assert!(text.contains("Workflow Metrics"));
        assert!(text.contains("Runnable: 2"));
        assert!(text.contains("Blocked: 1"));
        assert!(text.contains("In Review: 3"));
        assert!(text.contains("In Progress: 4"));
        assert!(text.contains("Idle With Runnable: eng-1, eng-2"));
        assert!(text.contains("Oldest Review Age: 120s"));
        assert!(text.contains("Oldest Assignment Age: 360s"));
    }
}
