//! Minimal planning-cycle prompt/parse support for tact daemon wiring.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSpec {
    pub title: String,
    pub body: String,
    pub priority: Option<String>,
    pub depends_on: Vec<u32>,
    pub tags: Vec<String>,
}

pub fn compose_planning_prompt(
    team_name: &str,
    board_summary: &str,
    recent_completions: &[String],
) -> String {
    let completions = if recent_completions.is_empty() {
        "Recent completions: none recorded.".to_string()
    } else {
        format!("Recent completions:\n- {}", recent_completions.join("\n- "))
    };

    format!(
        "Planning cycle for team `{team_name}`.\n\
         The pipeline is starved and needs new executable work.\n\n\
         Board summary:\n{board_summary}\n\n\
         {completions}\n\n\
         Respond with one or more task specs in this exact format:\n\
         TASK: <title>\n\
         PRIORITY: <critical|high|medium|low>\n\
         DEPENDS_ON: <comma-separated task ids or blank>\n\
         TAGS: <comma-separated tags or blank>\n\
         BODY:\n\
         <multi-line task body>\n\
         ---"
    )
}

pub fn parse_planning_response(response: &str) -> Vec<TaskSpec> {
    response
        .split("\n---")
        .filter_map(parse_task_spec_block)
        .collect()
}

fn parse_task_spec_block(block: &str) -> Option<TaskSpec> {
    let trimmed = block.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut title = None;
    let mut priority = None;
    let mut depends_on = Vec::new();
    let mut tags = Vec::new();
    let mut body_lines = Vec::new();
    let mut in_body = false;

    for line in trimmed.lines() {
        if let Some(rest) = line.strip_prefix("TASK:") {
            title = Some(rest.trim().to_string());
            in_body = false;
            continue;
        }
        if let Some(rest) = line.strip_prefix("PRIORITY:") {
            let value = rest.trim();
            priority = (!value.is_empty()).then(|| value.to_string());
            in_body = false;
            continue;
        }
        if let Some(rest) = line.strip_prefix("DEPENDS_ON:") {
            depends_on = rest
                .split(',')
                .filter_map(|item| item.trim().parse::<u32>().ok())
                .collect();
            in_body = false;
            continue;
        }
        if let Some(rest) = line.strip_prefix("TAGS:") {
            tags = rest
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(ToString::to_string)
                .collect();
            in_body = false;
            continue;
        }
        if line.trim() == "BODY:" {
            in_body = true;
            continue;
        }
        if in_body {
            body_lines.push(line);
        }
    }

    let title = title?;
    if title.is_empty() {
        return None;
    }

    Some(TaskSpec {
        title,
        body: body_lines.join("\n").trim().to_string(),
        priority,
        depends_on,
        tags,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_planning_response_extracts_task_specs() {
        let specs = parse_planning_response(
            "TASK: Add queue metrics\n\
             PRIORITY: high\n\
             DEPENDS_ON: 12, 13\n\
             TAGS: metrics,daemon\n\
             BODY:\n\
             Track queue depth and expose it in status.\n\
             ---\n\
             TASK: Backfill tests\n\
             PRIORITY: medium\n\
             DEPENDS_ON:\n\
             TAGS: tests\n\
             BODY:\n\
             Add coverage for the new planning flow.\n",
        );

        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].title, "Add queue metrics");
        assert_eq!(specs[0].priority.as_deref(), Some("high"));
        assert_eq!(specs[0].depends_on, vec![12, 13]);
        assert_eq!(specs[0].tags, vec!["metrics", "daemon"]);
        assert!(specs[0].body.contains("Track queue depth"));
        assert_eq!(specs[1].title, "Backfill tests");
    }

    #[test]
    fn compose_planning_prompt_includes_context() {
        let prompt = compose_planning_prompt(
            "batty",
            "todo=0 backlog=1 in-progress=2",
            &["task #17 completed".into()],
        );

        assert!(prompt.contains("Planning cycle for team `batty`"));
        assert!(prompt.contains("todo=0 backlog=1 in-progress=2"));
        assert!(prompt.contains("task #17 completed"));
        assert!(prompt.contains("TASK: <title>"));
    }
}
