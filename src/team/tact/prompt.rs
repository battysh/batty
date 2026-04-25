use super::TactPrompt;

pub const PLANNING_RESPONSE_FORMAT: &str = r#"Return exactly one markdown task block per proposed task using this format:

---
title: "Short task title"
priority: high
depends_on: [12, 15]
tags: [area-tag, feature-tag]
---
Task body:
- concrete file paths to change
- clear acceptance criteria
- enough detail for an engineer to execute without more planning
"#;

pub fn compose_prompt(ctx: &TactPrompt) -> String {
    let completions = if ctx.recent_completions.is_empty() {
        "none".to_string()
    } else {
        ctx.recent_completions.join(", ")
    };
    let roadmap = if ctx.roadmap_priorities.is_empty() {
        "none".to_string()
    } else {
        ctx.roadmap_priorities.join(", ")
    };
    let requested = ctx.idle_count.saturating_sub(ctx.dispatchable_count);

    format!(
        "Board state: {}. Recent completions: {}. Roadmap priorities: {}. Please specify {} new tasks as structured specs with title, body, priority, and optional depends_on.\n\nExpected response format:\n{}",
        ctx.board_summary, completions, roadmap, requested, PLANNING_RESPONSE_FORMAT
    )
}

pub fn compose_planning_prompt(
    idle_engineer_count: usize,
    board_summary: &str,
    recent_completions: &[String],
    roadmap_context: &[String],
    project_goals: &[String],
    project_name: &str,
) -> String {
    compose_planning_prompt_with_blockers(
        idle_engineer_count,
        board_summary,
        recent_completions,
        roadmap_context,
        project_goals,
        project_name,
        &[],
    )
}

pub fn compose_planning_prompt_with_blockers(
    idle_engineer_count: usize,
    board_summary: &str,
    recent_completions: &[String],
    roadmap_context: &[String],
    project_goals: &[String],
    project_name: &str,
    blocked_task_summaries: &[String],
) -> String {
    compose_planning_prompt_with_recovery_context(PlanningPromptRecoveryContext {
        idle_engineer_count,
        board_summary,
        recent_completions,
        roadmap_context,
        project_goals,
        project_name,
        blocked_task_summaries,
        review_task_summaries: &[],
    })
}

pub struct PlanningPromptRecoveryContext<'a> {
    pub idle_engineer_count: usize,
    pub board_summary: &'a str,
    pub recent_completions: &'a [String],
    pub roadmap_context: &'a [String],
    pub project_goals: &'a [String],
    pub project_name: &'a str,
    pub blocked_task_summaries: &'a [String],
    pub review_task_summaries: &'a [String],
}

pub fn compose_planning_prompt_with_recovery_context(
    ctx: PlanningPromptRecoveryContext<'_>,
) -> String {
    let PlanningPromptRecoveryContext {
        idle_engineer_count,
        board_summary,
        recent_completions,
        roadmap_context,
        project_goals,
        project_name,
        blocked_task_summaries,
        review_task_summaries,
    } = ctx;

    let dispatchable_count = board_summary
        .split(',')
        .find_map(|part| part.trim().strip_prefix("dispatchable_tasks="))
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let review_drain_mode = !review_task_summaries.is_empty();
    let requested_count = if review_drain_mode {
        1
    } else {
        idle_engineer_count.saturating_sub(dispatchable_count)
    };
    let tact_prompt = TactPrompt {
        board_summary: format!(
            "{board_summary}, idle_engineers={idle_engineer_count}, dispatchable_tasks={}",
            board_summary
                .split(',')
                .find_map(|part| part.trim().strip_prefix("dispatchable_tasks="))
                .unwrap_or("unknown")
        ),
        recent_completions: recent_completions.to_vec(),
        roadmap_priorities: roadmap_context.to_vec(),
        idle_count: dispatchable_count + requested_count,
        dispatchable_count,
    };
    let completions = if recent_completions.is_empty() {
        "None.".to_string()
    } else {
        recent_completions
            .iter()
            .map(|item| format!("- {item}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let goals = if project_goals.is_empty() {
        "- No explicit project goals were provided.".to_string()
    } else {
        project_goals
            .iter()
            .map(|goal| format!("- {goal}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let roadmap = if roadmap_context.is_empty() {
        "- No roadmap context was available.".to_string()
    } else {
        roadmap_context
            .iter()
            .map(|item| format!("- {item}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let blocker_section = if blocked_task_summaries.is_empty() {
        String::new()
    } else {
        let blocked_tasks = blocked_task_summaries
            .iter()
            .map(|task| format!("- {task}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "Blocked work requiring unblock tasks:\n{blocked_tasks}\n\n\
Unblock task directive:\n\
- Propose executable unblock tasks, not generic feature work.\n\
- Each proposed task must name the blocked task id it unblocks or advances.\n\
- Prefer concrete dependency-clearing, missing-artifact, failing-test, decision, or investigation work.\n\n"
        )
    };
    let review_section = if review_task_summaries.is_empty() {
        String::new()
    } else {
        let review_tasks = review_task_summaries
            .iter()
            .map(|task| format!("- {task}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "Review backlog requiring drain/unblock tasks:\n{review_tasks}\n\n\
Review-drain directive:\n\
- Propose exactly one concrete review-disposition or unblock task for the top review item.\n\
- The returned task must be a normal high-priority todo/backlog task with depends_on set to the review task id.\n\
- Do not mark the returned task blocked solely because the source board is review-gated.\n\
- Include the review artifact, branch, commit, next_action, and held task gate in the task body.\n\n"
        )
    };
    let task_kind = if review_drain_mode {
        "review-drain unblock task"
    } else if blocked_task_summaries.is_empty() {
        "task(s)"
    } else {
        "executable unblock task(s)"
    };

    format!(
        "You are planning the next execution wave for the project `{project_name}`.\n\n\
Tact summary:\n{}\n\n\
Current board state:\n\
- Idle engineers available: {idle_engineer_count}\n\
- Recently completed work:\n{completions}\n\n\
Board summary:\n{board_summary}\n\n\
{blocker_section}\
{review_section}\
Roadmap context:\n{roadmap}\n\n\
Project goals:\n{goals}\n\n\
Propose exactly {requested_count} {task_kind}. Each task must be feature-sized, self-contained, \
and ready to hand directly to an engineer. Include concrete file paths and explicit acceptance \
criteria in each task body.\n\n\
Expected response format:\n{PLANNING_RESPONSE_FORMAT}",
        compose_prompt(&tact_prompt)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_produces_nonempty_prompt() {
        let prompt =
            compose_planning_prompt(2, "board text", &["Done".to_string()], &[], &[], "Batty");
        assert!(!prompt.trim().is_empty());
    }

    #[test]
    fn compose_includes_engineer_count() {
        let prompt = compose_planning_prompt(3, "board text", &[], &[], &[], "Batty");
        assert!(prompt.contains("Idle engineers available: 3"));
        assert!(prompt.contains("Propose exactly 3 task(s)."));
    }

    #[test]
    fn compose_sizes_request_by_dispatchable_deficit() {
        let prompt = compose_planning_prompt(
            3,
            "todo=4 backlog=1 in-progress=0 review=0 done=0 idle_engineers=3, dispatchable_tasks=1",
            &[],
            &[],
            &[],
            "Batty",
        );
        assert!(prompt.contains("Propose exactly 2 task(s)."));
        assert!(prompt.contains("Please specify 2 new tasks"));
    }

    #[test]
    fn compose_includes_roadmap_context() {
        let prompt = compose_planning_prompt(
            1,
            "board text",
            &[],
            &[
                "Phase 2: Deliver tact planning loop".to_string(),
                "Milestone: auto-dispatch created work".to_string(),
            ],
            &[],
            "Batty",
        );
        assert!(prompt.contains("- Phase 2: Deliver tact planning loop"));
        assert!(prompt.contains("- Milestone: auto-dispatch created work"));
    }

    #[test]
    fn compose_includes_goals() {
        let prompt = compose_planning_prompt(
            1,
            "board text",
            &[],
            &[],
            &[
                "Ship tact".to_string(),
                "Reduce manual planning".to_string(),
            ],
            "Batty",
        );
        assert!(prompt.contains("- Ship tact"));
        assert!(prompt.contains("- Reduce manual planning"));
    }

    #[test]
    fn compose_handles_empty_completions() {
        let prompt = compose_planning_prompt(1, "board text", &[], &[], &[], "Batty");
        assert!(prompt.contains("Recently completed work:\nNone."));
    }

    #[test]
    fn compose_handles_empty_roadmap_context() {
        let prompt = compose_planning_prompt(1, "board text", &[], &[], &[], "Batty");
        assert!(prompt.contains("Roadmap context:\n- No roadmap context was available."));
    }

    #[test]
    fn compose_handles_zero_idle() {
        let prompt = compose_planning_prompt(0, "board text", &[], &[], &[], "Batty");
        assert!(prompt.contains("Idle engineers available: 0"));
        assert!(prompt.contains("Propose exactly 0 task(s)."));
    }

    #[test]
    fn compose_with_blockers_requests_unblock_tasks() {
        let prompt = compose_planning_prompt_with_blockers(
            2,
            "todo=0, blocked=2, dispatchable_tasks=0, planning_state=blocked-only-board",
            &[],
            &[],
            &[],
            "Batty",
            &[
                "#737 Convert blocked-only planning cycles: waiting on executable unblock work"
                    .to_string(),
                "#740 Restore dispatch: unmet dependency #739 (blocked)".to_string(),
            ],
        );

        assert!(prompt.contains("Blocked work requiring unblock tasks:"));
        assert!(prompt.contains("#737 Convert blocked-only planning cycles"));
        assert!(prompt.contains("Propose exactly 2 executable unblock task(s)."));
        assert!(prompt.contains("Each proposed task must name the blocked task id"));
    }

    #[test]
    fn compose_with_review_backlog_requests_one_review_drain_task() {
        let prompt = compose_planning_prompt_with_recovery_context(
            PlanningPromptRecoveryContext {
            idle_engineer_count: 3,
            board_summary: "todo=0, backlog=0, review=1, actionable_review=1, blocked=2, dispatchable_tasks=0, planning_state=review-backlog-gated",
            recent_completions: &[],
            roadmap_context: &[],
            project_goals: &[],
            project_name: "Batty",
            blocked_task_summaries: &[
                "#738 Ship dependent release: waiting on #736 review disposition".to_string(),
                "#739 Restore downstream dispatch: waiting on #736 review disposition".to_string(),
            ],
            review_task_summaries: &[
                "#736 Drain completion review: review_owner=manager; branch=eng-1-2/736; commit=abc1234; artifacts=.batty/reports/verification/completion/task-736-eng-1-2-attempt-1.json; next_action=review commit; gates held tasks: #738, #739".to_string(),
            ],
        });

        assert!(prompt.contains("Review backlog requiring drain/unblock tasks:"));
        assert!(prompt.contains("#736 Drain completion review"));
        assert!(prompt.contains("commit=abc1234"));
        assert!(prompt.contains("task-736-eng-1-2-attempt-1.json"));
        assert!(prompt.contains("Propose exactly 1 review-drain unblock task."));
        assert!(prompt.contains("depends_on set to the review task id"));
        assert!(prompt.contains("Do not mark the returned task blocked"));
        assert!(prompt.contains("Please specify 1 new tasks"));
    }

    #[test]
    fn test_compose_prompt_includes_state() {
        let prompt = compose_prompt(&TactPrompt {
            board_summary: "3 todo, 1 in-progress, 2 idle engineers".to_string(),
            recent_completions: vec!["Finished parser".to_string()],
            roadmap_priorities: vec!["Ship tact".to_string()],
            idle_count: 2,
            dispatchable_count: 1,
        });
        assert!(prompt.contains("3 todo, 1 in-progress, 2 idle engineers"));
        assert!(prompt.contains("Finished parser"));
        assert!(prompt.contains("Ship tact"));
        assert!(prompt.contains("Please specify 1 new tasks"));
    }
}
