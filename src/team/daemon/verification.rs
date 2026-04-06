use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::task::{Task, load_tasks_from_dir};
use crate::team::task_loop::run_tests_in_worktree;
use crate::team::test_results::TestResults;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerificationRunResult {
    pub passed: bool,
    pub output: String,
    pub results: TestResults,
    pub failures: Vec<String>,
    pub file_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScopeValidationResult {
    pub declared_scope: Vec<String>,
    pub changed_files: Vec<String>,
    pub out_of_scope_files: Vec<String>,
}

pub(crate) fn run_automatic_verification(
    worktree_dir: &Path,
    test_command: Option<&str>,
) -> Result<VerificationRunResult> {
    if let Some(scope_failure) = scope_validation_failure(worktree_dir)? {
        return Ok(scope_failure);
    }

    let test_run = run_tests_in_worktree(worktree_dir, test_command)?;
    let (failures, file_paths) = parse_test_output(&test_run.output, &test_run.results);
    Ok(VerificationRunResult {
        passed: test_run.passed,
        output: test_run.output,
        results: test_run.results,
        failures,
        file_paths,
    })
}

pub(crate) fn parse_scope_fence(task_text: &str) -> Vec<String> {
    task_text
        .lines()
        .find_map(|line| line.trim().strip_prefix("SCOPE FENCE:"))
        .map(|scope| {
            scope
                .split(',')
                .map(|entry| entry.trim().trim_matches('`').trim_end_matches('/'))
                .filter(|entry| !entry.is_empty())
                .map(|entry| entry.to_string())
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn changed_files_from_main(worktree_dir: &Path) -> Result<Vec<String>> {
    let output = std::process::Command::new("git")
        .args(["diff", "--name-only", "main..HEAD"])
        .current_dir(worktree_dir)
        .output()
        .with_context(|| format!("failed to run git diff in {}", worktree_dir.display()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "git diff --name-only main..HEAD failed in {}: {}",
            worktree_dir.display(),
            stderr.trim()
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| line.to_string())
        .collect())
}

pub(crate) fn validate_declared_scope(
    task_text: &str,
    changed_files: &[String],
) -> ScopeValidationResult {
    let declared_scope = parse_scope_fence(task_text);
    let out_of_scope_files = if declared_scope.is_empty() {
        Vec::new()
    } else {
        changed_files
            .iter()
            .filter(|path| !path_within_scope(path, &declared_scope))
            .cloned()
            .collect()
    };

    ScopeValidationResult {
        declared_scope,
        changed_files: changed_files.to_vec(),
        out_of_scope_files,
    }
}

fn scope_validation_failure(worktree_dir: &Path) -> Result<Option<VerificationRunResult>> {
    let Some((project_root, engineer)) = engineer_worktree_context(worktree_dir) else {
        return Ok(None);
    };
    let Some(task) = find_claimed_task_for_worktree(&project_root, &engineer, worktree_dir)? else {
        return Ok(None);
    };

    let task_text = std::fs::read_to_string(&task.source_path)
        .with_context(|| format!("failed to read {}", task.source_path.display()))?;
    let changed_files = changed_files_from_main(worktree_dir)?;
    let scope = validate_declared_scope(&task_text, &changed_files);
    if scope.declared_scope.is_empty() || scope.out_of_scope_files.is_empty() {
        return Ok(None);
    }

    let message = format!(
        "scope fence violation for task #{}: changed files outside declared scope: {}",
        task.id,
        scope.out_of_scope_files.join(", ")
    );
    Ok(Some(VerificationRunResult {
        passed: false,
        output: message.clone(),
        results: TestResults {
            framework: "scope-fence".to_string(),
            total: None,
            passed: 0,
            failed: 1,
            ignored: 0,
            failures: Vec::new(),
            summary: Some(message.clone()),
        },
        failures: vec![message],
        file_paths: scope.out_of_scope_files,
    }))
}

fn engineer_worktree_context(worktree_dir: &Path) -> Option<(PathBuf, String)> {
    let engineer = worktree_dir.file_name()?.to_str()?.to_string();
    let worktrees_dir = worktree_dir.parent()?;
    if worktrees_dir.file_name()?.to_str()? != "worktrees" {
        return None;
    }
    let batty_dir = worktrees_dir.parent()?;
    if batty_dir.file_name()?.to_str()? != ".batty" {
        return None;
    }
    Some((batty_dir.parent()?.to_path_buf(), engineer))
}

fn find_claimed_task_for_worktree(
    project_root: &Path,
    engineer: &str,
    worktree_dir: &Path,
) -> Result<Option<Task>> {
    let tasks_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    let mut tasks = load_tasks_from_dir(&tasks_dir)
        .with_context(|| format!("failed to load tasks from {}", tasks_dir.display()))?;
    tasks.retain(|task| task.claimed_by.as_deref() == Some(engineer));
    tasks.sort_by_key(task_scope_priority);

    Ok(tasks.into_iter().find(|task| {
        matches!(task.status.as_str(), "review" | "in-progress")
            && task_matches_worktree(project_root, task, worktree_dir)
    }))
}

fn task_scope_priority(task: &Task) -> (u8, u32) {
    let status_rank = match task.status.as_str() {
        "review" => 0,
        "in-progress" => 1,
        _ => 2,
    };
    (status_rank, task.id)
}

fn task_matches_worktree(project_root: &Path, task: &Task, worktree_dir: &Path) -> bool {
    task.worktree_path
        .as_deref()
        .map(|path| resolve_worktree_path(project_root, path) == worktree_dir)
        .unwrap_or(true)
}

fn resolve_worktree_path(project_root: &Path, worktree_path: &str) -> PathBuf {
    let candidate = PathBuf::from(worktree_path);
    if candidate.is_absolute() {
        candidate
    } else {
        project_root.join(candidate)
    }
}

fn parse_test_output(output: &str, results: &TestResults) -> (Vec<String>, Vec<String>) {
    let mut failures = Vec::new();
    let mut file_paths = BTreeSet::new();

    for failure in &results.failures {
        let mut detail = failure.test_name.clone();
        if let Some(message) = failure.message.as_deref().filter(|message| !message.is_empty()) {
            detail.push_str(": ");
            detail.push_str(message);
        }
        if let Some(location) = failure
            .location
            .as_deref()
            .filter(|location| !location.is_empty())
        {
            detail.push_str(" @ ");
            detail.push_str(location);

            let normalized = normalize_path_token(location);
            if looks_like_path(normalized) {
                file_paths.insert(normalized.to_string());
            }
        }
        failures.push(detail);
    }

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if failures.is_empty() && trimmed.starts_with("test ") && trimmed.ends_with("FAILED") {
            failures.push(trimmed.to_string());
        } else if failures.is_empty() && (trimmed.starts_with("error:") || trimmed.contains("panicked at")) {
            failures.push(trimmed.to_string());
        }

        for token in trimmed.split_whitespace() {
            let cleaned = token.trim_matches(|ch: char| {
                matches!(ch, '"' | '\'' | ',' | ':' | ';' | '(' | ')' | '[' | ']')
            });
            let normalized = normalize_path_token(cleaned);
            if looks_like_path(normalized) {
                file_paths.insert(normalized.to_string());
            }
        }
    }

    if failures.is_empty() && !output.trim().is_empty() {
        failures.push("test command failed without a parsed failure line".to_string());
    }

    (failures, file_paths.into_iter().collect())
}

fn normalize_path_token(token: &str) -> &str {
    let mut candidate = token;
    while let Some((head, tail)) = candidate.rsplit_once(':') {
        if tail.chars().all(|ch| ch.is_ascii_digit()) {
            candidate = head;
        } else {
            break;
        }
    }
    candidate
}

fn looks_like_path(token: &str) -> bool {
    let has_separator = token.contains('/') || token.contains('\\');
    let has_extension = token.rsplit_once('.').is_some_and(|(_, ext)| {
        !ext.is_empty() && ext.chars().all(|ch| ch.is_ascii_alphanumeric())
    });
    has_separator && has_extension
}

fn path_within_scope(path: &str, scope_entries: &[String]) -> bool {
    scope_entries.iter().any(|scope| {
        path == scope
            || path
                .strip_prefix(scope)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use crate::team::test_results::{TestFailure, TestResults};

    use super::{
        ScopeValidationResult, engineer_worktree_context, find_claimed_task_for_worktree,
        parse_scope_fence, parse_test_output, scope_validation_failure, validate_declared_scope,
    };

    #[test]
    fn parse_test_output_extracts_failures_and_paths() {
        let output = "\
test parser::it_works ... FAILED\n\
error: could not compile crate due to previous error\n\
src/parser.rs:12: failure here\n";
        let results = TestResults {
            framework: "cargo".to_string(),
            total: Some(1),
            passed: 0,
            failed: 1,
            ignored: 0,
            failures: vec![TestFailure {
                test_name: "parser::it_works".to_string(),
                message: Some("assertion failed".to_string()),
                location: Some("src/parser.rs:12:5".to_string()),
            }],
            summary: Some("test result: FAILED. 0 passed; 1 failed; 0 ignored;".to_string()),
        };
        let (failures, paths) = parse_test_output(output, &results);
        assert!(failures.iter().any(|line| line.contains("parser::it_works")));
        assert!(failures.iter().any(|line| line.contains("assertion failed")));
        assert!(paths.iter().any(|path| path == "src/parser.rs"));
    }

    #[test]
    fn parse_scope_fence_extracts_entries() {
        let task_text =
            "Task\nSCOPE FENCE: src/team/completion.rs, src/team/review.rs, src/team/daemon/\n";
        assert_eq!(
            parse_scope_fence(task_text),
            vec![
                "src/team/completion.rs".to_string(),
                "src/team/review.rs".to_string(),
                "src/team/daemon".to_string(),
            ]
        );
    }

    #[test]
    fn validate_declared_scope_reports_out_of_scope_files() {
        let result = validate_declared_scope(
            "SCOPE FENCE: src/team/completion.rs, src/team/review.rs",
            &[
                "src/team/completion.rs".to_string(),
                "src/team/daemon/poll.rs".to_string(),
            ],
        );

        assert_eq!(
            result,
            ScopeValidationResult {
                declared_scope: vec![
                    "src/team/completion.rs".to_string(),
                    "src/team/review.rs".to_string(),
                ],
                changed_files: vec![
                    "src/team/completion.rs".to_string(),
                    "src/team/daemon/poll.rs".to_string(),
                ],
                out_of_scope_files: vec!["src/team/daemon/poll.rs".to_string()],
            }
        );
    }

    #[test]
    fn engineer_worktree_context_detects_batty_worktrees() {
        let context = engineer_worktree_context(Path::new("/tmp/project/.batty/worktrees/eng-1-3"))
            .expect("batty worktree should resolve");
        assert_eq!(context.0, PathBuf::from("/tmp/project"));
        assert_eq!(context.1, "eng-1-3");
        assert!(engineer_worktree_context(Path::new("/tmp/project")).is_none());
    }

    #[test]
    fn find_claimed_task_for_worktree_prefers_matching_review_task() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let worktree_dir = tmp.path().join(".batty").join("worktrees").join("eng-1-3");
        std::fs::create_dir_all(&worktree_dir).unwrap();
        std::fs::write(
            tasks_dir.join("010-task.md"),
            "---\nid: 10\ntitle: other\nstatus: review\npriority: medium\nclaimed_by: eng-1-3\nworktree_path: .batty/worktrees/eng-1-9\n---\n\nTask body.\n",
        )
        .unwrap();
        std::fs::write(
            tasks_dir.join("011-task.md"),
            "---\nid: 11\ntitle: target\nstatus: review\npriority: medium\nclaimed_by: eng-1-3\nworktree_path: .batty/worktrees/eng-1-3\n---\n\nTask body.\n",
        )
        .unwrap();

        let task = find_claimed_task_for_worktree(tmp.path(), "eng-1-3", &worktree_dir)
            .unwrap()
            .expect("matching task should be found");
        assert_eq!(task.id, 11);
    }

    #[test]
    fn scope_fence_validation_failure_reports_out_of_scope_files() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        let worktree_dir = tmp.path().join(".batty").join("worktrees").join("eng-1-3");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::create_dir_all(worktree_dir.join("src/team")).unwrap();
        std::fs::write(
            tasks_dir.join("011-task.md"),
            "---\nid: 11\ntitle: target\nstatus: review\npriority: medium\nclaimed_by: eng-1-3\n---\n\nTask body.\nSCOPE FENCE: src/team/completion.rs, src/team/review.rs\n",
        )
        .unwrap();

        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&worktree_dir)
                .output()
                .unwrap()
        };
        assert!(git(&["init"]).status.success());
        assert!(
            git(&["config", "user.email", "test@example.com"])
                .status
                .success()
        );
        assert!(git(&["config", "user.name", "Test"]).status.success());
        std::fs::write(worktree_dir.join("src/team/completion.rs"), "base\n").unwrap();
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-m", "base"]).status.success());
        assert!(git(&["branch", "-M", "main"]).status.success());
        assert!(git(&["checkout", "-b", "eng-1-3"]).status.success());

        std::fs::write(worktree_dir.join("src/team/review.rs"), "in scope\n").unwrap();
        std::fs::write(worktree_dir.join("src/team/daemon.rs"), "out of scope\n").unwrap();
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-m", "change"]).status.success());

        let result = scope_validation_failure(&worktree_dir)
            .unwrap()
            .expect("scope violation should be reported");
        assert!(!result.passed);
        assert!(result.output.contains("scope fence violation"));
        assert_eq!(result.file_paths, vec!["src/team/daemon.rs".to_string()]);
    }
}
