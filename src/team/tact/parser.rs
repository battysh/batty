use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::warn;

use super::{GeneratedTask, TaskSpec};
use crate::task::load_tasks_from_dir;

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

fn normalize_generated_text(value: &str) -> String {
    let mut normalized = String::with_capacity(value.len());
    let mut last_was_space = false;

    for ch in value.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            normalized.push(ch);
            last_was_space = false;
        } else if !last_was_space {
            normalized.push(' ');
            last_was_space = true;
        }
    }

    normalized.trim().to_string()
}

fn normalize_generated_title(title: &str) -> String {
    normalize_generated_text(title)
}

fn normalize_generated_body(body: &str) -> String {
    normalize_generated_text(body)
}

fn normalize_reopen_title(title: &str) -> String {
    let mut normalized = String::with_capacity(title.len());
    let mut last_was_space = false;

    for ch in title.chars().flat_map(char::to_lowercase) {
        let mapped = match ch {
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            _ => ch,
        };
        if mapped.is_whitespace() {
            if !last_was_space {
                normalized.push(' ');
                last_was_space = true;
            }
        } else {
            normalized.push(mapped);
            last_was_space = false;
        }
    }

    normalized.trim().to_string()
}

fn is_reopen_title(title: &str) -> bool {
    normalize_reopen_title(title).starts_with("reopen ")
}

fn is_open_task_status(status: &str) -> bool {
    !matches!(status, "done" | "archived")
}

fn existing_open_reopen_titles(board_dir: &Path) -> std::collections::HashSet<String> {
    let tasks_dir = board_dir.join("tasks");
    if !tasks_dir.is_dir() {
        return std::collections::HashSet::new();
    }

    load_tasks_from_dir(&tasks_dir)
        .unwrap_or_default()
        .into_iter()
        .filter(|task| is_open_task_status(&task.status) && is_reopen_title(&task.title))
        .map(|task| normalize_reopen_title(&task.title))
        .collect()
}

fn generated_task_equivalence_key(task: &GeneratedTask) -> String {
    let mut tags = task
        .tags
        .iter()
        .map(|tag| normalize_generated_text(tag))
        .filter(|tag| !tag.is_empty())
        .collect::<Vec<_>>();
    tags.sort();
    tags.dedup();

    let priority = task
        .priority
        .as_deref()
        .map(normalize_generated_text)
        .unwrap_or_default();
    let depends_on = task
        .depends_on
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");

    format!(
        "{}|{}|{}|{}",
        normalize_generated_body(&task.body),
        priority,
        tags.join(","),
        depends_on
    )
}

fn looks_like_raw_test_log(body: &str) -> bool {
    let has_running_header = body.lines().any(|line| {
        let trimmed = line.trim();
        trimmed.starts_with("running ") && trimmed.ends_with(" tests")
    });
    let test_line_count = body
        .lines()
        .filter(|line| line.trim_start().starts_with("test "))
        .count();
    let has_failure_marker = body.lines().any(|line| {
        let trimmed = line.trim();
        trimmed.ends_with("FAILED")
            || trimmed.starts_with("failures:")
            || trimmed.starts_with("error:")
            || trimmed.contains("panicked at")
            || trimmed.contains("No such file or directory")
    });

    (has_running_header && has_failure_marker && test_line_count >= 1)
        || (has_running_header && test_line_count >= 3)
        || (test_line_count >= 5 && has_failure_marker)
}

fn summarize_reopen_body(body: &str) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut highlights = Vec::new();

    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("running ")
            || trimmed.ends_with("... ok")
            || trimmed.starts_with("test result: ok")
        {
            continue;
        }

        let keep = trimmed.ends_with("FAILED")
            || trimmed.starts_with("error:")
            || trimmed.starts_with("failures:")
            || trimmed.contains("panicked at")
            || trimmed.contains("No such file or directory")
            || trimmed.starts_with("called `Result::unwrap()`");
        if !keep {
            continue;
        }

        let normalized = trimmed.to_string();
        if seen.insert(normalized.clone()) {
            highlights.push(normalized);
        }
        if highlights.len() == 6 {
            break;
        }
    }

    let mut summary = String::from("Automatic reopen after failed verification.\n\nSummary:\n");
    if highlights.is_empty() {
        summary.push_str("- `cargo test` failed in the default verification environment.\n");
        return summary;
    }

    for line in highlights {
        summary.push_str("- ");
        summary.push_str(&line);
        summary.push('\n');
    }
    summary
}

fn sanitize_generated_task(spec: &GeneratedTask) -> Option<GeneratedTask> {
    let title = spec.title.trim();
    let body = spec.body.trim();

    if title.is_empty() {
        warn!("rejecting generated task with empty title");
        return None;
    }
    if body.is_empty() {
        warn!(title, "rejecting generated task with empty body");
        return None;
    }
    if looks_like_raw_test_log(body) {
        warn!(title, "rejecting generated task with raw log body");
        return None;
    }

    Some(GeneratedTask {
        title: title.to_string(),
        body: body.to_string(),
        priority: spec.priority.as_ref().map(|value| value.trim().to_string()),
        depends_on: spec.depends_on.clone(),
        tags: spec.tags.iter().map(|tag| tag.trim().to_string()).collect(),
    })
}

pub(crate) fn dedupe_generated_tasks(
    existing: &[crate::task::Task],
    proposed: Vec<GeneratedTask>,
) -> Vec<GeneratedTask> {
    let mut open_titles = existing
        .iter()
        .filter(|task| is_open_task_status(&task.status))
        .map(|task| normalize_generated_title(&task.title))
        .filter(|title| !title.is_empty())
        .collect::<std::collections::HashSet<_>>();
    let mut open_specs = existing
        .iter()
        .filter(|task| is_open_task_status(&task.status))
        .filter_map(|task| {
            let body = task.description.trim();
            if body.is_empty() {
                return None;
            }
            Some(generated_task_equivalence_key(&GeneratedTask {
                title: task.title.clone(),
                body: body.to_string(),
                priority: Some(task.priority.clone()),
                depends_on: task.depends_on.clone(),
                tags: task.tags.clone(),
            }))
        })
        .collect::<std::collections::HashSet<_>>();
    let mut seen_titles = std::collections::HashSet::new();
    let mut seen_specs = std::collections::HashSet::new();
    let mut deduped = Vec::with_capacity(proposed.len());

    for task in proposed {
        let title_key = normalize_generated_title(&task.title);
        let spec_key = generated_task_equivalence_key(&task);
        let duplicate_title = !title_key.is_empty()
            && (!seen_titles.insert(title_key.clone()) || open_titles.contains(&title_key));
        let duplicate_spec = !spec_key.is_empty()
            && (!seen_specs.insert(spec_key.clone()) || open_specs.contains(&spec_key));

        if duplicate_title || duplicate_spec {
            warn!(
                title = %task.title,
                duplicate_title,
                duplicate_spec,
                "suppressing duplicate generated task"
            );
            continue;
        }

        if !title_key.is_empty() {
            open_titles.insert(title_key);
        }
        if !spec_key.is_empty() {
            open_specs.insert(spec_key);
        }
        deduped.push(task);
    }

    deduped
}

fn create_board_tasks_with_program(
    specs: &[TaskSpec],
    board_dir: &Path,
    program: &str,
) -> Result<Vec<u32>> {
    if !board_dir.exists() {
        anyhow::bail!("board directory does not exist: {}", board_dir.display());
    }

    let existing_tasks = load_tasks_from_dir(&board_dir.join("tasks")).unwrap_or_default();
    let generated = specs
        .iter()
        .filter_map(sanitize_generated_task)
        .collect::<Vec<_>>();
    let deduped = dedupe_generated_tasks(&existing_tasks, generated);
    let mut created_ids = Vec::with_capacity(deduped.len());
    for sanitized in deduped {
        let args = build_create_task_args(&sanitized);
        let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();
        let output = crate::team::board_cmd::run_board_with_program(program, board_dir, &arg_refs)
            .with_context(|| format!("failed to create board task '{}'", sanitized.title))?;

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

/// Create board tasks from parsed specs by shelling out to kanban-md.
pub fn create_board_tasks(specs: &[TaskSpec], board_dir: &Path) -> Result<Vec<u32>> {
    create_board_tasks_with_program(specs, board_dir, "kanban-md")
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn write_task_file(board_dir: &Path, id: u32, title: &str, status: &str) {
        std::fs::write(
            board_dir.join("tasks").join(format!("{id:03}-task.md")),
            format!(
                "---\nid: {id}\ntitle: {title}\nstatus: {status}\npriority: critical\n---\n\nTask body.\n"
            ),
        )
        .unwrap();
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
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join("board");
        std::fs::create_dir_all(board_dir.join("tasks")).unwrap();
        let fake_kanban = setup_fake_kanban(&tmp).join("kanban-md");

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

        let ids =
            create_board_tasks_with_program(&specs, &board_dir, fake_kanban.to_str().unwrap())
                .unwrap();
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

    #[test]
    fn create_board_tasks_rejects_raw_log_dump() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join("board");
        std::fs::create_dir_all(board_dir.join("tasks")).unwrap();
        let fake_kanban = setup_fake_kanban(&tmp).join("kanban-md");

        let raw_body = "\
running 3144 tests
test agent::claude::tests::default_mode_is_interactive ... ok
test tmux::tests::split_window_horizontal_creates_new_pane ... FAILED

failures:

---- tmux::tests::split_window_horizontal_creates_new_pane stdout ----
thread 'tmux::tests::split_window_horizontal_creates_new_pane' panicked at src/tmux.rs:1903:71:
called `Result::unwrap()` on an `Err` value: failed to create tmux session 'batty-test-hsplit'

Caused by:
    No such file or directory (os error 2)

test result: FAILED. 3011 passed; 14 failed; 119 ignored; 0 measured; 0 filtered out;
";
        let specs = vec![TaskSpec {
            title: "Reopen tmux runtime test hardening - make cargo test green on main".into(),
            body: raw_body.into(),
            priority: Some("critical".into()),
            depends_on: vec![],
            tags: vec!["stability".into(), "tmux".into()],
        }];

        let ids =
            create_board_tasks_with_program(&specs, &board_dir, fake_kanban.to_str().unwrap())
                .unwrap();
        assert!(ids.is_empty());

        let tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks")).unwrap();
        assert!(tasks.is_empty());
    }

    #[test]
    fn create_board_tasks_skips_duplicate_open_reopen_title_variants() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join("board");
        std::fs::create_dir_all(board_dir.join("tasks")).unwrap();
        let fake_kanban = setup_fake_kanban(&tmp).join("kanban-md");

        write_task_file(
            &board_dir,
            41,
            "Reopen tmux runtime test hardening - make cargo test green on main",
            "todo",
        );

        let specs = vec![TaskSpec {
            title: "Reopen tmux runtime test hardening — make cargo test green on main".into(),
            body: "running 3144 tests\ntest tmux::tests::split_window_horizontal_creates_new_pane ... FAILED\n".into(),
            priority: Some("critical".into()),
            depends_on: vec![],
            tags: vec!["stability".into()],
        }];

        let ids =
            create_board_tasks_with_program(&specs, &board_dir, fake_kanban.to_str().unwrap())
                .unwrap();
        assert!(ids.is_empty());

        let tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks")).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, 41);
    }

    #[test]
    fn create_board_tasks_skips_duplicate_open_title() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join("board");
        std::fs::create_dir_all(board_dir.join("tasks")).unwrap();
        let fake_kanban = setup_fake_kanban(&tmp).join("kanban-md");

        write_task_file(&board_dir, 41, "Ship planning telemetry", "todo");

        let specs = vec![TaskSpec {
            title: "Ship planning telemetry".into(),
            body: "Fresh body that should still be suppressed.".into(),
            priority: Some("high".into()),
            depends_on: vec![],
            tags: vec!["tact".into()],
        }];

        let ids =
            create_board_tasks_with_program(&specs, &board_dir, fake_kanban.to_str().unwrap())
                .unwrap();
        assert!(ids.is_empty());

        let tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks")).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, 41);
    }

    #[test]
    fn create_board_tasks_skips_equivalent_generated_specs() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join("board");
        std::fs::create_dir_all(board_dir.join("tasks")).unwrap();
        let fake_kanban = setup_fake_kanban(&tmp).join("kanban-md");

        let specs = vec![
            TaskSpec {
                title: "Plan planning telemetry".into(),
                body: "Record planning cycle events in the orchestrator log.".into(),
                priority: Some("high".into()),
                depends_on: vec![],
                tags: vec!["tact".into(), "telemetry".into()],
            },
            TaskSpec {
                title: "Backfill planning telemetry".into(),
                body: "Record planning cycle events in the orchestrator log.".into(),
                priority: Some("high".into()),
                depends_on: vec![],
                tags: vec!["telemetry".into(), "tact".into()],
            },
        ];

        let ids =
            create_board_tasks_with_program(&specs, &board_dir, fake_kanban.to_str().unwrap())
                .unwrap();
        assert_eq!(ids, vec![1]);

        let tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks")).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "Plan planning telemetry");
    }

    #[test]
    fn create_board_tasks_allows_new_reopen_after_terminal_duplicate() {
        let tmp = tempfile::tempdir().unwrap();
        let board_dir = tmp.path().join("board");
        std::fs::create_dir_all(board_dir.join("tasks")).unwrap();
        let fake_kanban = setup_fake_kanban(&tmp).join("kanban-md");

        write_task_file(
            &board_dir,
            41,
            "Reopen tmux runtime test hardening - make cargo test green on main",
            "archived",
        );

        let specs = vec![TaskSpec {
            title: "Reopen tmux runtime test hardening — make cargo test green on main".into(),
            body: "Automatic reopen after failed verification.".into(),
            priority: Some("critical".into()),
            depends_on: vec![],
            tags: vec!["stability".into()],
        }];

        let ids =
            create_board_tasks_with_program(&specs, &board_dir, fake_kanban.to_str().unwrap())
                .unwrap();
        assert_eq!(ids, vec![2]);

        let tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks")).unwrap();
        assert_eq!(tasks.len(), 2);
    }
}
