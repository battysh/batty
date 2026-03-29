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

pub fn compose_planning_prompt(
    idle_engineer_count: usize,
    board_summary: &str,
    recent_completions: &[String],
    project_goals: &[String],
    project_name: &str,
) -> String {
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

    format!(
        "You are planning the next execution wave for the project `{project_name}`.\n\n\
Current board state:\n\
- Idle engineers available: {idle_engineer_count}\n\
- Recently completed work:\n{completions}\n\n\
Board summary:\n{board_summary}\n\n\
Project goals:\n{goals}\n\n\
Propose exactly {idle_engineer_count} task(s). Each task must be feature-sized, self-contained, \
and ready to hand directly to an engineer. Include concrete file paths and explicit acceptance \
criteria in each task body.\n\n\
Expected response format:\n{PLANNING_RESPONSE_FORMAT}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_produces_nonempty_prompt() {
        let prompt = compose_planning_prompt(2, "board text", &["Done".to_string()], &[], "Batty");
        assert!(!prompt.trim().is_empty());
    }

    #[test]
    fn compose_includes_engineer_count() {
        let prompt = compose_planning_prompt(3, "board text", &[], &[], "Batty");
        assert!(prompt.contains("Idle engineers available: 3"));
        assert!(prompt.contains("Propose exactly 3 task(s)."));
    }

    #[test]
    fn compose_includes_goals() {
        let prompt = compose_planning_prompt(
            1,
            "board text",
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
        let prompt = compose_planning_prompt(1, "board text", &[], &[], "Batty");
        assert!(prompt.contains("Recently completed work:\nNone."));
    }

    #[test]
    fn compose_handles_zero_idle() {
        let prompt = compose_planning_prompt(0, "board text", &[], &[], "Batty");
        assert!(prompt.contains("Idle engineers available: 0"));
        assert!(prompt.contains("Propose exactly 0 task(s)."));
    }
}
