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
    let requested = ctx.idle_count.saturating_sub(ctx.todo_count);

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
        idle_count: idle_engineer_count,
        todo_count: board_summary
            .split(',')
            .find_map(|part| part.trim().strip_prefix("todo="))
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0),
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

    format!(
        "You are planning the next execution wave for the project `{project_name}`.\n\n\
Tact summary:\n{}\n\n\
Current board state:\n\
- Idle engineers available: {idle_engineer_count}\n\
- Recently completed work:\n{completions}\n\n\
Board summary:\n{board_summary}\n\n\
Roadmap context:\n{roadmap}\n\n\
Project goals:\n{goals}\n\n\
Propose exactly {idle_engineer_count} task(s). Each task must be feature-sized, self-contained, \
and ready to hand directly to an engineer. Include concrete file paths and explicit acceptance \
criteria in each task body.\n\n\
Expected response format:\n{PLANNING_RESPONSE_FORMAT}"
        ,
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
    fn test_compose_prompt_includes_state() {
        let prompt = compose_prompt(&TactPrompt {
            board_summary: "3 todo, 1 in-progress, 2 idle engineers".to_string(),
            recent_completions: vec!["Finished parser".to_string()],
            roadmap_priorities: vec!["Ship tact".to_string()],
            idle_count: 2,
            todo_count: 3,
        });
        assert!(prompt.contains("3 todo, 1 in-progress, 2 idle engineers"));
        assert!(prompt.contains("Finished parser"));
        assert!(prompt.contains("Ship tact"));
    }
}
