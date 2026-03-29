use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::warn;

use super::TaskSpec;
use crate::team::board_cmd;

#[derive(Debug, Deserialize)]
struct Frontmatter {
    title: Option<String>,
    priority: Option<String>,
    depends_on: Option<Vec<u32>>,
    tags: Option<Vec<String>>,
}

/// Parse the architect's planning response into task specifications.
pub fn parse_planning_response(response: &str) -> Vec<TaskSpec> {
    let mut specs = Vec::new();
    let trimmed = response.trim();
    if trimmed.is_empty() {
        return specs;
    }

    let mut rest = trimmed;
    loop {
        rest = rest.trim_start();
        let Some(after_open) = rest.strip_prefix("---") else {
            break;
        };
        let after_open = after_open.strip_prefix('\n').unwrap_or(after_open);
        let Some(frontmatter_end) = after_open.find("\n---") else {
            warn!("skipping tact block with unterminated frontmatter");
            break;
        };

        let frontmatter_raw = &after_open[..frontmatter_end];
        let after_frontmatter = &after_open[frontmatter_end + 4..];
        let body_start = after_frontmatter
            .strip_prefix('\n')
            .unwrap_or(after_frontmatter);

        let next_block = body_start.find("\n---");
        let (body_raw, next_rest) = match next_block {
            Some(index) => (&body_start[..index], Some(&body_start[index..])),
            None => (body_start, None),
        };

        match serde_yaml::from_str::<Frontmatter>(frontmatter_raw) {
            Ok(frontmatter) => {
                let Some(title) = frontmatter.title.map(|title| title.trim().to_string()) else {
                    warn!("skipping tact block without title");
                    rest = next_rest.unwrap_or("");
                    continue;
                };
                if title.is_empty() {
                    warn!("skipping tact block with empty title");
                    rest = next_rest.unwrap_or("");
                    continue;
                }
                let body = body_raw.trim().to_string();
                specs.push(TaskSpec {
                    title,
                    body,
                    priority: frontmatter.priority.map(|value| value.trim().to_string()),
                    depends_on: frontmatter.depends_on.unwrap_or_default(),
                    tags: frontmatter.tags.unwrap_or_default(),
                });
            }
            Err(error) => warn!(%error, "skipping tact block with malformed frontmatter"),
        }

        rest = next_rest.unwrap_or("");
        if rest.trim().is_empty() {
            break;
        }
    }

    specs
}

/// Create board tasks from parsed specs by shelling out to kanban-md.
pub fn create_board_tasks(specs: &[TaskSpec], board_dir: &Path) -> Result<Vec<u32>> {
    let mut created_ids = Vec::with_capacity(specs.len());
    for spec in specs {
        let depends_on = if spec.depends_on.is_empty() {
            None
        } else {
            Some(
                spec.depends_on
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
            )
        };
        let tags = if spec.tags.is_empty() {
            None
        } else {
            Some(spec.tags.join(","))
        };

        let task_id = board_cmd::create_task(
            board_dir,
            &spec.title,
            &spec.body,
            spec.priority.as_deref(),
            tags.as_deref(),
            depends_on.as_deref(),
        )
        .with_context(|| format!("failed to create board task '{}'", spec.title))?;

        let parsed_id = task_id
            .parse::<u32>()
            .with_context(|| format!("invalid task id returned by kanban-md: '{task_id}'"))?;
        created_ids.push(parsed_id);
    }
    Ok(created_ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_task() {
        let response = r#"---
title: "Add tact parser"
priority: high
tags: [core, tact]
---
Implement parser logic."#;

        let specs = parse_planning_response(response);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].title, "Add tact parser");
        assert_eq!(specs[0].priority.as_deref(), Some("high"));
        assert_eq!(specs[0].tags, vec!["core", "tact"]);
        assert_eq!(specs[0].body, "Implement parser logic.");
    }

    #[test]
    fn parse_multiple_tasks() {
        let response = r#"---
title: "Task one"
priority: low
---
Body one.
---
title: "Task two"
priority: medium
---
Body two.
---
title: "Task three"
priority: high
---
Body three."#;

        let specs = parse_planning_response(response);
        assert_eq!(specs.len(), 3);
        assert_eq!(specs[0].title, "Task one");
        assert_eq!(specs[1].title, "Task two");
        assert_eq!(specs[2].title, "Task three");
    }

    #[test]
    fn parse_with_dependencies() {
        let response = r#"---
title: "Dependent task"
depends_on: [1, 2, 8]
---
Body."#;

        let specs = parse_planning_response(response);
        assert_eq!(specs[0].depends_on, vec![1, 2, 8]);
    }

    #[test]
    fn parse_malformed_skips_bad_blocks() {
        let response = r#"---
title: "Good task"
priority: high
---
Good body.
---
title: [unterminated
---
Bad body.
---
title: "Second good task"
---
Second body."#;

        let specs = parse_planning_response(response);
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].title, "Good task");
        assert_eq!(specs[1].title, "Second good task");
    }

    #[test]
    fn parse_empty_response() {
        assert!(parse_planning_response("").is_empty());
        assert!(parse_planning_response("   \n\t").is_empty());
    }

    #[test]
    fn parse_no_frontmatter() {
        assert!(parse_planning_response("just some freeform planning text").is_empty());
    }

    #[test]
    fn parse_missing_title_skips_block() {
        let response = r#"---
priority: high
---
No title here."#;
        assert!(parse_planning_response(response).is_empty());
    }
}
