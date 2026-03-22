pub use super::status::{WorkflowMetrics, compute_metrics, compute_metrics_with_events};

#[cfg(test)]
use super::status::format_metrics;

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
            ..Default::default()
        });

        assert!(text.contains("Workflow Metrics"));
        assert!(text.contains("Runnable: 2"));
        assert!(text.contains("Blocked: 1"));
        assert!(text.contains("In Review: 3"));
        assert!(text.contains("In Progress: 4"));
        assert!(text.contains("Idle With Runnable: eng-1, eng-2"));
        assert!(text.contains("Oldest Review Age: 120s"));
        assert!(text.contains("Oldest Assignment Age: 360s"));
        assert!(text.contains("Review Pipeline"));
    }

    fn write_events(path: &Path, events: &[crate::team::events::TeamEvent]) {
        let mut lines = Vec::new();
        for event in events {
            lines.push(serde_json::to_string(event).unwrap());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, lines.join("\n")).unwrap();
    }

    #[test]
    fn review_metrics_count_events() {
        use crate::team::events::TeamEvent;

        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");
        let events_path = tmp.path().join("events.jsonl");
        write_task(&board_dir, 1, "t1", "done", None, None, &[]);

        write_events(
            &events_path,
            &[
                TeamEvent::task_auto_merged("eng-1", "1", 0.9, 2, 30),
                TeamEvent::task_auto_merged("eng-1", "2", 0.9, 2, 30),
                TeamEvent::task_auto_merged("eng-1", "3", 0.9, 2, 30),
                TeamEvent::task_manual_merged("4"),
                TeamEvent::task_manual_merged("5"),
                TeamEvent::task_reworked("eng-1", "6"),
                TeamEvent::review_nudge_sent("manager", "7"),
                TeamEvent::review_escalated("manager", "8"),
                TeamEvent::review_escalated("manager", "9"),
            ],
        );

        let metrics = compute_metrics_with_events(&board_dir, &[], Some(&events_path)).unwrap();

        assert_eq!(metrics.auto_merge_count, 3);
        assert_eq!(metrics.manual_merge_count, 2);
        assert_eq!(metrics.rework_count, 1);
        assert_eq!(metrics.review_nudge_count, 1);
        assert_eq!(metrics.review_escalation_count, 2);

        // auto_merge_rate = 3 / (3 + 2) = 0.6
        let rate = metrics.auto_merge_rate.unwrap();
        assert!((rate - 0.6).abs() < 0.01);

        // rework_rate = 1 / (5 + 1) ≈ 0.167
        let rework = metrics.rework_rate.unwrap();
        assert!((rework - 1.0 / 6.0).abs() < 0.01);
    }

    #[test]
    fn review_metrics_compute_latency() {
        use crate::team::events::TeamEvent;

        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");
        let events_path = tmp.path().join("events.jsonl");
        write_task(&board_dir, 1, "t1", "done", None, None, &[]);

        // task_completed marks review entry, task_auto/manual_merged marks exit
        let mut e1 = TeamEvent::task_completed("eng-1");
        e1.task = Some("10".to_string());
        e1.ts = 1000;
        let mut e2 = TeamEvent::task_auto_merged("eng-1", "10", 0.9, 2, 30);
        e2.ts = 1100; // 100s latency

        let mut e3 = TeamEvent::task_completed("eng-2");
        e3.task = Some("20".to_string());
        e3.ts = 2000;
        let mut e4 = TeamEvent::task_manual_merged("20");
        e4.ts = 2300; // 300s latency

        write_events(&events_path, &[e1, e2, e3, e4]);

        let metrics = compute_metrics_with_events(&board_dir, &[], Some(&events_path)).unwrap();

        // avg = (100 + 300) / 2 = 200
        let avg = metrics.avg_review_latency_secs.unwrap();
        assert!((avg - 200.0).abs() < 0.01);
    }

    #[test]
    fn review_metrics_handle_no_merges() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");
        let events_path = tmp.path().join("events.jsonl");
        write_task(&board_dir, 1, "t1", "done", None, None, &[]);

        // Empty event file — no merge events
        std::fs::write(&events_path, "").unwrap();

        let metrics = compute_metrics_with_events(&board_dir, &[], Some(&events_path)).unwrap();

        assert_eq!(metrics.auto_merge_count, 0);
        assert_eq!(metrics.manual_merge_count, 0);
        assert!(metrics.auto_merge_rate.is_none());
        assert!(metrics.rework_rate.is_none());
        assert!(metrics.avg_review_latency_secs.is_none());
    }

    #[test]
    fn status_includes_review_pipeline() {
        let text = format_metrics(&WorkflowMetrics {
            in_review_count: 2,
            auto_merge_count: 3,
            manual_merge_count: 2,
            auto_merge_rate: Some(0.6),
            rework_count: 1,
            rework_rate: Some(1.0 / 6.0),
            review_nudge_count: 1,
            review_escalation_count: 0,
            avg_review_latency_secs: Some(272.0),
            ..Default::default()
        });

        assert!(text.contains("Review Pipeline"));
        assert!(text.contains("Queue: 2"));
        assert!(text.contains("Auto-merge Rate: 60%"));
        assert!(text.contains("Auto: 3"));
        assert!(text.contains("Manual: 2"));
        assert!(text.contains("Rework: 1"));
        assert!(text.contains("Nudges: 1"));
        assert!(text.contains("Escalations: 0"));
    }

    #[test]
    fn retro_includes_review_section() {
        use crate::team::retrospective::{RunStats, generate_retrospective};

        let tmp = tempfile::tempdir().unwrap();
        let stats = RunStats {
            run_start: 100,
            run_end: 500,
            total_duration_secs: 400,
            task_stats: Vec::new(),
            average_cycle_time_secs: None,
            fastest_task_id: None,
            fastest_cycle_time_secs: None,
            longest_task_id: None,
            longest_cycle_time_secs: None,
            idle_time_pct: 0.0,
            escalation_count: 0,
            message_count: 0,
            auto_merge_count: 5,
            manual_merge_count: 2,
            rework_count: 1,
            review_nudge_count: 3,
            review_escalation_count: 0,
        };

        let path = generate_retrospective(tmp.path(), &stats).unwrap();
        let content = std::fs::read_to_string(path).unwrap();

        assert!(content.contains("## Review Performance"));
        assert!(content.contains("Auto-merged: 5"));
        assert!(content.contains("Manually merged: 2"));
        assert!(content.contains("Auto-merge rate: 71%"));
        assert!(content.contains("Rework: 1"));
        assert!(content.contains("Review nudges: 3"));
        assert!(content.contains("Review escalations: 0"));
    }
}
