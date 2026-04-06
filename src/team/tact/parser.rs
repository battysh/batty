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

pub fn parse_task_specs(response: &str) -> Vec<TaskSpec> {
    parse_planning_response(response)
}

fn build_create_task_args(spec: &TaskSpec) -> Vec<String> {
    let mut args = vec![
        "create".to_string(),
        spec.title.clone(),
        "--body".to_string(),
        spec.body.clone(),
    ];
    if let Some(priority) = spec.priority.as_deref() {
        args.push("--priority".to_string());
        args.push(priority.to_string());
    }
    if !spec.tags.is_empty() {
        args.push("--tags".to_string());
        args.push(spec.tags.join(","));
    }
    if !spec.depends_on.is_empty() {
        args.push("--depends-on".to_string());
        args.push(
            spec.depends_on
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(","),
        );
    }
    args
}

/// Create board tasks from parsed specs by shelling out to kanban-md.
pub fn create_board_tasks(specs: &[TaskSpec], board_dir: &Path) -> Result<Vec<u32>> {
    if !board_dir.exists() {
        anyhow::bail!("board directory does not exist: {}", board_dir.display());
    }

    let mut created_ids = Vec::with_capacity(specs.len());
    for spec in specs {
        let args = build_create_task_args(spec);
        let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();
        let output = board_cmd::run_board(board_dir, &arg_refs)
            .with_context(|| format!("failed to create board task '{}'", spec.title))?;

        let task_id = output
            .stdout
            .trim()
            .strip_prefix("Created task #")
            .unwrap_or(output.stdout.trim());
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
    use crate::team::test_support::{EnvVarGuard, PATH_LOCK};

    fn setup_fake_kanban(tmp: &tempfile::TempDir) -> std::path::PathBuf {
        let fake_bin = tmp.path().join("fake-bin");
        std::fs::create_dir_all(&fake_bin).unwrap();
        let script = fake_bin.join("kanban-md");
        std::fs::write(
            &script,
            "#!/bin/bash\nset -euo pipefail\nif [ \"$1\" != \"create\" ]; then exit 1; fi\nshift\ntitle=\"$1\"\nshift\nbody=\"\"\npriority=\"high\"\ntags=\"\"\ndepends_on=\"\"\nwhile [ $# -gt 0 ]; do\n  case \"$1\" in\n    --body) body=\"$2\"; shift 2 ;;\n    --priority) priority=\"$2\"; shift 2 ;;\n    --tags) tags=\"$2\"; shift 2 ;;\n    --depends-on) depends_on=\"$2\"; shift 2 ;;\n    --dir) board_dir=\"$2\"; shift 2 ;;\n    *) shift ;;\n  esac\ndone\nmkdir -p \"$board_dir/tasks\"\ncount=$(find \"$board_dir/tasks\" -maxdepth 1 -name '*.md' | wc -l | tr -d ' ')\nid=$((count + 1))\nprintf -- '---\\nid: %s\\ntitle: %s\\nstatus: todo\\npriority: %s\\n' \"$id\" \"$title\" \"$priority\" > \"$board_dir/tasks/$(printf '%03d' \"$id\")-task.md\"\nif [ -n \"$tags\" ]; then printf 'tags: [%s]\\n' \"$tags\" >> \"$board_dir/tasks/$(printf '%03d' \"$id\")-task.md\"; fi\nif [ -n \"$depends_on\" ]; then printf 'depends_on: [%s]\\n' \"$depends_on\" >> \"$board_dir/tasks/$(printf '%03d' \"$id\")-task.md\"; fi\nprintf -- '---\\n\\n%s\\n' \"$body\" >> \"$board_dir/tasks/$(printf '%03d' \"$id\")-task.md\"\nprintf 'Created task #%s\\n' \"$id\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        fake_bin
    }

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
    fn test_parse_task_specs_single() {
        let response = r#"---
title: "Add tact parser"
priority: high
---
Implement parser logic."#;

        let specs = parse_task_specs(response);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].title, "Add tact parser");
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
    fn test_parse_task_specs_multiple() {
        let response = r#"---
title: "Task one"
priority: low
---
Body one.
---
title: "Task two"
priority: medium
---
Body two."#;

        let specs = parse_task_specs(response);
        assert_eq!(specs.len(), 2);
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
    fn test_parse_task_specs_with_depends() {
        let response = r#"---
title: "Dependent task"
depends_on: [42]
---
Body."#;

        let specs = parse_task_specs(response);
        assert_eq!(specs[0].depends_on, vec![42]);
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
    fn test_parse_task_specs_empty() {
        assert!(parse_task_specs("").is_empty());
        assert!(parse_task_specs("garbage").is_empty());
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

    #[test]
    fn create_board_tasks_round_trip_creates_tasks_with_metadata() {
        let _path_lock = PATH_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join("board");
        std::fs::create_dir_all(board_dir.join("tasks")).unwrap();
        let fake_bin = setup_fake_kanban(&tmp);
        let path = format!(
            "{}:{}",
            fake_bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let _path_guard = EnvVarGuard::set("PATH", &path);

        let specs = vec![
            TaskSpec {
                title: "Task one".into(),
                body: "Body one".into(),
                priority: Some("high".into()),
                depends_on: vec![],
                tags: vec!["tact".into()],
            },
            TaskSpec {
                title: "Task two".into(),
                body: "Body two".into(),
                priority: Some("medium".into()),
                depends_on: vec![1],
                tags: vec!["integration".into()],
            },
        ];

        let ids = create_board_tasks(&specs, &board_dir).unwrap();
        assert_eq!(ids, vec![1, 2]);
        let tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks")).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[1].depends_on, vec![1]);
        assert_eq!(tasks[1].tags, vec!["integration"]);
    }

    #[test]
    fn create_board_tasks_missing_board_dir_returns_clear_error() {
        let specs = vec![TaskSpec {
            title: "Task one".into(),
            body: "Body one".into(),
            priority: None,
            depends_on: vec![],
            tags: vec![],
        }];
        let tmp = tempfile::tempdir().unwrap();
        let error = create_board_tasks(&specs, &tmp.path().join("missing")).unwrap_err();
        assert!(error.to_string().contains("board directory does not exist"),);
    }

    #[test]
    fn test_create_board_tasks_formats_command() {
        let args = build_create_task_args(&TaskSpec {
            title: "Plan tact".into(),
            body: "Create the daemon prompt.".into(),
            priority: Some("high".into()),
            depends_on: vec![17],
            tags: vec!["tact".into(), "daemon".into()],
        });
        assert_eq!(
            args,
            vec![
                "create",
                "Plan tact",
                "--body",
                "Create the daemon prompt.",
                "--priority",
                "high",
                "--tags",
                "tact,daemon",
                "--depends-on",
                "17",
            ]
        );
    }
}
