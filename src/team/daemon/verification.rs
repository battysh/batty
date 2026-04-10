use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::task::{Task, load_tasks_from_dir};
use crate::team::config::TeamConfig;
use crate::team::hierarchy::resolve_hierarchy;
use crate::team::inbox;
use crate::team::review::task_reference_mismatch_blockers;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScopeFenceCheck {
    pub task_id: u32,
    pub declared_scope: Vec<String>,
    pub out_of_scope_files: Vec<String>,
    pub ack_present: bool,
}

pub(crate) fn run_automatic_verification(
    worktree_dir: &Path,
    test_command: Option<&str>,
) -> Result<VerificationRunResult> {
    if let Some(conflict_failure) = active_claim_conflict_failure(worktree_dir)? {
        return Ok(conflict_failure);
    }
    if let Some(task_mismatch_failure) = task_mismatch_validation_failure(worktree_dir)? {
        return Ok(task_mismatch_failure);
    }
    if let Some(scope_failure) = scope_validation_failure(worktree_dir)? {
        return Ok(scope_failure);
    }

    let test_run = run_tests_in_worktree(worktree_dir, test_command)?;
    let (failures, _failure_paths) =
        parse_test_output(&test_run.output, &test_run.results, test_run.passed);
    let file_paths = changed_files_from_main(worktree_dir)?;
    Ok(VerificationRunResult {
        passed: test_run.passed,
        output: test_run.output,
        results: test_run.results,
        failures,
        file_paths,
    })
}

fn active_claim_conflict_failure(worktree_dir: &Path) -> Result<Option<VerificationRunResult>> {
    let Some((project_root, engineer)) = engineer_worktree_context(worktree_dir) else {
        return Ok(None);
    };

    let tasks_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    let active_tasks: Vec<Task> = load_tasks_from_dir(&tasks_dir)
        .with_context(|| format!("failed to load tasks from {}", tasks_dir.display()))?
        .into_iter()
        .filter(|task| {
            task.claimed_by.as_deref() == Some(engineer.as_str())
                && matches!(task.status.as_str(), "review" | "in-progress")
        })
        .collect();
    if active_tasks.len() <= 1 {
        return Ok(None);
    }

    let current_branch = current_branch_name(worktree_dir).ok();
    let explicit_worktree_matches = active_tasks
        .iter()
        .filter(|task| {
            task.worktree_path
                .as_deref()
                .is_some_and(|path| resolve_worktree_path(&project_root, path) == worktree_dir)
        })
        .count();
    let branch_matches = current_branch
        .as_deref()
        .map(|branch| {
            active_tasks
                .iter()
                .filter(|task| {
                    task.branch
                        .as_deref()
                        .map(|task_branch| task_branch == branch)
                        .unwrap_or_else(|| format!("{engineer}/{}", task.id) == branch)
                })
                .count()
        })
        .unwrap_or(0);

    if explicit_worktree_matches == 1 || branch_matches == 1 {
        return Ok(None);
    }

    let task_list = active_tasks
        .iter()
        .map(|task| format!("#{}", task.id))
        .collect::<Vec<_>>()
        .join(", ");
    let message = format!(
        "ambiguous active task ownership for {engineer}: claimed tasks {task_list} do not resolve to a single worktree/task branch"
    );
    Ok(Some(VerificationRunResult {
        passed: false,
        output: message.clone(),
        results: TestResults {
            framework: "claim-conflict".to_string(),
            total: None,
            passed: 0,
            failed: 1,
            ignored: 0,
            failures: Vec::new(),
            summary: Some(message.clone()),
        },
        failures: vec![message],
        file_paths: Vec::new(),
    }))
}

fn task_mismatch_validation_failure(worktree_dir: &Path) -> Result<Option<VerificationRunResult>> {
    let Some((project_root, engineer)) = engineer_worktree_context(worktree_dir) else {
        return Ok(None);
    };
    let Some(task) = find_claimed_task_for_worktree(&project_root, &engineer, worktree_dir)? else {
        return Ok(None);
    };

    let branch_name = current_branch_name(worktree_dir)?;
    let commit_messages = commit_subjects_since_main(worktree_dir)?;
    let blockers = task_reference_mismatch_blockers(task.id, &branch_name, &commit_messages);
    if blockers.is_empty() {
        return Ok(None);
    }

    let message = format!(
        "task reference mismatch for task #{}: {}",
        task.id,
        blockers.join("; ")
    );
    Ok(Some(VerificationRunResult {
        passed: false,
        output: message.clone(),
        results: TestResults {
            framework: "task-mismatch".to_string(),
            total: None,
            passed: 0,
            failed: 1,
            ignored: 0,
            failures: Vec::new(),
            summary: Some(message.clone()),
        },
        failures: vec![message],
        file_paths: Vec::new(),
    }))
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

pub(crate) fn scope_ack_token(task_id: u32) -> String {
    format!("Scope ACK #{task_id}")
}

pub(crate) fn is_scope_ack_message(body: &str, task_id: u32) -> bool {
    let normalized = body.split_whitespace().collect::<Vec<_>>().join(" ");
    normalized
        .to_ascii_lowercase()
        .contains(&scope_ack_token(task_id).to_ascii_lowercase())
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

fn current_branch_name(worktree_dir: &Path) -> Result<String> {
    let output = std::process::Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(worktree_dir)
        .output()
        .with_context(|| format!("failed to read git branch in {}", worktree_dir.display()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "git branch --show-current failed in {}: {}",
            worktree_dir.display(),
            stderr.trim()
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn commit_subjects_since_main(worktree_dir: &Path) -> Result<Vec<String>> {
    let output = std::process::Command::new("git")
        .args(["log", "--format=%s", "main..HEAD"])
        .current_dir(worktree_dir)
        .output()
        .with_context(|| {
            format!(
                "failed to read commit subjects in {}",
                worktree_dir.display()
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "git log --format=%s main..HEAD failed in {}: {}",
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

fn scope_acknowledged(
    project_root: &Path,
    engineer: &str,
    task_id: u32,
    claimed_at: Option<&str>,
) -> Result<bool> {
    let inbox_root = project_root.join(".batty").join("inboxes");
    if !inbox_root.is_dir() {
        return Ok(false);
    }
    let Some(ack_recipient) = scope_ack_recipient(project_root, engineer)? else {
        return Ok(false);
    };
    if !inbox_root.join(&ack_recipient).is_dir() {
        return Ok(false);
    }

    let claimed_after = claimed_at
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.timestamp().max(0) as u64);

    for (message, _) in inbox::all_messages(&inbox_root, &ack_recipient)? {
        if message.from != engineer {
            continue;
        }
        if claimed_after.is_some_and(|cutoff| message.timestamp < cutoff) {
            continue;
        }
        if is_scope_ack_message(&message.body, task_id) {
            return Ok(true);
        }
    }

    Ok(false)
}

fn scope_ack_recipient(project_root: &Path, engineer: &str) -> Result<Option<String>> {
    let team_config_path = project_root
        .join(".batty")
        .join("team_config")
        .join("team.yaml");
    let team_config = TeamConfig::load(&team_config_path)
        .with_context(|| format!("failed to load {}", team_config_path.display()))?;
    Ok(resolve_hierarchy(&team_config)?
        .into_iter()
        .find(|member| member.name == engineer)
        .and_then(|member| member.reports_to))
}

pub(crate) fn inspect_scope_fence(worktree_dir: &Path) -> Result<Option<ScopeFenceCheck>> {
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
    if scope.declared_scope.is_empty() {
        return Ok(None);
    }

    let ack_present = scope_acknowledged(
        &project_root,
        &engineer,
        task.id,
        task.claimed_at.as_deref(),
    )?;

    Ok(Some(ScopeFenceCheck {
        task_id: task.id,
        declared_scope: scope.declared_scope,
        out_of_scope_files: scope.out_of_scope_files,
        ack_present,
    }))
}

fn scope_validation_failure(worktree_dir: &Path) -> Result<Option<VerificationRunResult>> {
    let Some(scope) = inspect_scope_fence(worktree_dir)? else {
        return Ok(None);
    };

    if !scope.ack_present {
        let token = scope_ack_token(scope.task_id);
        let message = format!(
            "scope ack missing for task #{}: send `{token}` to the assigning manager or architect before writing files inside the task fence",
            scope.task_id
        );
        return Ok(Some(VerificationRunResult {
            passed: false,
            output: message.clone(),
            results: TestResults {
                framework: "scope-ack".to_string(),
                total: None,
                passed: 0,
                failed: 1,
                ignored: 0,
                failures: Vec::new(),
                summary: Some(message.clone()),
            },
            failures: vec![message],
            file_paths: scope.declared_scope,
        }));
    }

    if scope.out_of_scope_files.is_empty() {
        return Ok(None);
    }

    let message = format!(
        "scope fence violation for task #{}: changed files outside declared scope: {}",
        scope.task_id,
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

fn parse_test_output(
    output: &str,
    results: &TestResults,
    passed: bool,
) -> (Vec<String>, Vec<String>) {
    let mut failures = Vec::new();
    let mut file_paths = BTreeSet::new();

    for failure in &results.failures {
        let mut detail = failure.test_name.clone();
        if let Some(message) = failure
            .message
            .as_deref()
            .filter(|message| !message.is_empty())
        {
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

        if failures.is_empty()
            && (trimmed.starts_with("test ") && trimmed.ends_with("FAILED")
                || trimmed.starts_with("error:")
                || trimmed.contains("panicked at"))
        {
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

    if failures.is_empty() && !passed && !output.trim().is_empty() {
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

    use crate::team::inbox;
    use crate::team::test_results::{TestFailure, TestResults};

    use super::{
        ScopeValidationResult, active_claim_conflict_failure, commit_subjects_since_main,
        current_branch_name, engineer_worktree_context, find_claimed_task_for_worktree,
        inspect_scope_fence, is_scope_ack_message, parse_scope_fence, parse_test_output,
        run_automatic_verification, scope_validation_failure, task_mismatch_validation_failure,
        validate_declared_scope,
    };

    fn write_scope_ack_team_config(project_root: &Path) {
        let team_config_dir = project_root.join(".batty").join("team_config");
        std::fs::create_dir_all(&team_config_dir).unwrap();
        std::fs::write(
            team_config_dir.join("team.yaml"),
            "name: scope-test\nroles:\n  - name: architect\n    role_type: architect\n    agent: claude\n    instances: 1\n  - name: manager\n    role_type: manager\n    agent: claude\n    instances: 1\n  - name: engineer\n    role_type: engineer\n    agent: codex\n    instances: 3\n",
        )
        .unwrap();
    }

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
        let (failures, paths) = parse_test_output(output, &results, false);
        assert!(
            failures
                .iter()
                .any(|line| line.contains("parser::it_works"))
        );
        assert!(
            failures
                .iter()
                .any(|line| line.contains("assertion failed"))
        );
        assert!(paths.iter().any(|path| path == "src/parser.rs"));
    }

    #[test]
    fn parse_test_output_ignores_non_failure_output_for_passing_runs() {
        let output = "Finished verification override successfully\n";
        let results = TestResults {
            framework: "cargo".to_string(),
            total: Some(1),
            passed: 1,
            failed: 0,
            ignored: 0,
            failures: Vec::new(),
            summary: Some("test result: ok. 1 passed; 0 failed; 0 ignored;".to_string()),
        };

        let (failures, paths) = parse_test_output(output, &results, true);

        assert!(failures.is_empty());
        assert!(paths.is_empty());
    }

    #[test]
    fn parse_test_output_keeps_fallback_for_unparsed_failed_runs() {
        let output = "verification override failed\n";
        let results = TestResults {
            framework: "cargo".to_string(),
            total: None,
            passed: 0,
            failed: 1,
            ignored: 0,
            failures: Vec::new(),
            summary: None,
        };

        let (failures, _paths) = parse_test_output(output, &results, false);

        assert_eq!(
            failures,
            vec!["test command failed without a parsed failure line".to_string()]
        );
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
    fn active_claim_conflict_failure_reports_ambiguous_claimed_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        let worktree_dir = tmp.path().join(".batty").join("worktrees").join("eng-1-3");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::create_dir_all(&worktree_dir).unwrap();
        std::fs::write(
            tasks_dir.join("010-task.md"),
            "---\nid: 10\ntitle: first\nstatus: in-progress\npriority: medium\nclaimed_by: eng-1-3\n---\n\nTask body.\n",
        )
        .unwrap();
        std::fs::write(
            tasks_dir.join("011-task.md"),
            "---\nid: 11\ntitle: second\nstatus: review\npriority: medium\nclaimed_by: eng-1-3\n---\n\nTask body.\n",
        )
        .unwrap();

        let result = active_claim_conflict_failure(&worktree_dir)
            .unwrap()
            .expect("ambiguous claimed tasks should fail verification");
        assert!(!result.passed);
        assert!(
            result
                .output
                .contains("ambiguous active task ownership for eng-1-3")
        );
    }

    #[test]
    fn scope_fence_validation_failure_reports_out_of_scope_files() {
        let tmp = tempfile::tempdir().unwrap();
        write_scope_ack_team_config(tmp.path());
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        let inbox_root = tmp.path().join(".batty").join("inboxes");
        let worktree_dir = tmp.path().join(".batty").join("worktrees").join("eng-1-3");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::create_dir_all(worktree_dir.join("src/team")).unwrap();
        inbox::init_inbox(&inbox_root, "manager").unwrap();
        std::fs::write(
            tasks_dir.join("011-task.md"),
            "---\nid: 11\ntitle: target\nstatus: review\npriority: medium\nclaimed_by: eng-1-3\nclaimed_at: 2026-04-10T12:00:00Z\n---\n\nTask body.\nSCOPE FENCE: src/team/completion.rs, src/team/review.rs\n",
        )
        .unwrap();
        let ack = inbox::InboxMessage::new_send("eng-1-3", "manager", "Scope ACK #11");
        inbox::deliver_to_inbox(&inbox_root, &ack).unwrap();

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

    #[test]
    fn scope_ack_message_detects_expected_token() {
        assert!(is_scope_ack_message("Scope ACK #587", 587));
        assert!(is_scope_ack_message(
            "batty send manager \"Scope ACK #587\"",
            587
        ));
        assert!(!is_scope_ack_message("acknowledged", 587));
    }

    #[test]
    fn inspect_scope_fence_reports_missing_ack() {
        let tmp = tempfile::tempdir().unwrap();
        write_scope_ack_team_config(tmp.path());
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
            "---\nid: 11\ntitle: target\nstatus: review\npriority: medium\nclaimed_by: eng-1-3\nclaimed_at: 2026-04-10T12:00:00Z\n---\n\nTask body.\nSCOPE FENCE: src/team/completion.rs\n",
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
        std::fs::write(worktree_dir.join("src/team/completion.rs"), "changed\n").unwrap();
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-m", "change"]).status.success());

        let result = inspect_scope_fence(&worktree_dir)
            .unwrap()
            .expect("scope fence should be present");
        assert_eq!(result.task_id, 11);
        assert!(!result.ack_present);
        assert!(result.out_of_scope_files.is_empty());

        let failure = scope_validation_failure(&worktree_dir)
            .unwrap()
            .expect("missing ack should fail");
        assert!(failure.output.contains("scope ack missing"));
    }

    #[test]
    fn inspect_scope_fence_accepts_matching_ack() {
        let tmp = tempfile::tempdir().unwrap();
        write_scope_ack_team_config(tmp.path());
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        let inbox_root = tmp.path().join(".batty").join("inboxes");
        let worktree_dir = tmp.path().join(".batty").join("worktrees").join("eng-1-3");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::create_dir_all(worktree_dir.join("src/team")).unwrap();
        inbox::init_inbox(&inbox_root, "manager").unwrap();
        std::fs::write(
            tasks_dir.join("011-task.md"),
            "---\nid: 11\ntitle: target\nstatus: review\npriority: medium\nclaimed_by: eng-1-3\nclaimed_at: 2026-04-10T12:00:00Z\n---\n\nTask body.\nSCOPE FENCE: src/team/completion.rs\n",
        )
        .unwrap();
        let ack = inbox::InboxMessage::new_send("eng-1-3", "manager", "Scope ACK #11");
        inbox::deliver_to_inbox(&inbox_root, &ack).unwrap();

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
        std::fs::write(worktree_dir.join("src/team/completion.rs"), "changed\n").unwrap();
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-m", "change"]).status.success());

        let result = inspect_scope_fence(&worktree_dir)
            .unwrap()
            .expect("scope fence should be present");
        assert!(result.ack_present);
        assert!(result.out_of_scope_files.is_empty());
    }

    #[test]
    fn inspect_scope_fence_rejects_ack_sent_to_wrong_inbox() {
        let tmp = tempfile::tempdir().unwrap();
        write_scope_ack_team_config(tmp.path());
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        let inbox_root = tmp.path().join(".batty").join("inboxes");
        let worktree_dir = tmp.path().join(".batty").join("worktrees").join("eng-1-3");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::create_dir_all(worktree_dir.join("src/team")).unwrap();
        inbox::init_inbox(&inbox_root, "manager").unwrap();
        inbox::init_inbox(&inbox_root, "architect").unwrap();
        std::fs::write(
            tasks_dir.join("011-task.md"),
            "---\nid: 11\ntitle: target\nstatus: review\npriority: medium\nclaimed_by: eng-1-3\nclaimed_at: 2026-04-10T12:00:00Z\n---\n\nTask body.\nSCOPE FENCE: src/team/completion.rs\n",
        )
        .unwrap();
        let ack = inbox::InboxMessage::new_send("eng-1-3", "architect", "Scope ACK #11");
        inbox::deliver_to_inbox(&inbox_root, &ack).unwrap();

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
        std::fs::write(worktree_dir.join("src/team/completion.rs"), "changed\n").unwrap();
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-m", "change"]).status.success());

        let result = inspect_scope_fence(&worktree_dir)
            .unwrap()
            .expect("scope fence should be present");
        assert!(!result.ack_present);
    }

    #[test]
    fn task_mismatch_validation_failure_reports_wrong_branch_and_commit_task_ids() {
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
            "---\nid: 11\ntitle: target\nstatus: review\npriority: medium\nclaimed_by: eng-1-3\n---\n\nTask body.\n",
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
        std::fs::write(worktree_dir.join("src/team/review.rs"), "base\n").unwrap();
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-m", "base"]).status.success());
        assert!(git(&["branch", "-M", "main"]).status.success());
        assert!(
            git(&["checkout", "-b", "eng-1-3/task-449"])
                .status
                .success()
        );

        std::fs::write(worktree_dir.join("src/team/review.rs"), "changed\n").unwrap();
        assert!(git(&["add", "."]).status.success());
        assert!(
            git(&["commit", "-m", "Task #449: implement wrong task"])
                .status
                .success()
        );

        let result = task_mismatch_validation_failure(&worktree_dir)
            .unwrap()
            .expect("task mismatch should be reported");
        assert!(!result.passed);
        assert!(result.output.contains("task reference mismatch"));
        assert!(result.output.contains("assigned task is #11"));
        assert!(result.output.contains("#449"));
    }

    #[test]
    fn task_mismatch_validation_failure_allows_expected_task_references() {
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
            "---\nid: 11\ntitle: target\nstatus: review\npriority: medium\nclaimed_by: eng-1-3\n---\n\nTask body.\n",
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
        std::fs::write(worktree_dir.join("src/team/review.rs"), "base\n").unwrap();
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-m", "base"]).status.success());
        assert!(git(&["branch", "-M", "main"]).status.success());
        assert!(git(&["checkout", "-b", "eng-1-3/task-11"]).status.success());

        std::fs::write(worktree_dir.join("src/team/review.rs"), "changed\n").unwrap();
        assert!(git(&["add", "."]).status.success());
        assert!(
            git(&["commit", "-m", "Task #11: implement expected task"])
                .status
                .success()
        );

        assert!(
            task_mismatch_validation_failure(&worktree_dir)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            current_branch_name(&worktree_dir).unwrap(),
            "eng-1-3/task-11"
        );
        assert_eq!(
            commit_subjects_since_main(&worktree_dir).unwrap(),
            vec!["Task #11: implement expected task".to_string()]
        );
    }

    #[test]
    fn run_automatic_verification_uses_git_diff_paths_not_test_output_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        let worktree_dir = tmp.path().join(".batty").join("worktrees").join("eng-1-3");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::create_dir_all(worktree_dir.join("src")).unwrap();
        std::fs::write(
            tasks_dir.join("011-task.md"),
            "---\nid: 11\ntitle: target\nstatus: in-progress\npriority: medium\nclaimed_by: eng-1-3\n---\n\nTask body.\n",
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
        std::fs::write(
            worktree_dir.join("src").join("owned.rs"),
            "pub fn owned() {}\n",
        )
        .unwrap();
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-m", "base"]).status.success());
        assert!(git(&["branch", "-M", "main"]).status.success());
        assert!(git(&["checkout", "-b", "eng-1-3/task-11"]).status.success());

        std::fs::write(
            worktree_dir.join("src").join("owned.rs"),
            "pub fn owned() -> bool { true }\n",
        )
        .unwrap();
        assert!(git(&["add", "src/owned.rs"]).status.success());
        assert!(
            git(&["commit", "-m", "Task #11: change owned"])
                .status
                .success()
        );

        let result = run_automatic_verification(
            &worktree_dir,
            Some("printf 'error: src/noisy.rs:9:1 boom\\n'; exit 1"),
        )
        .unwrap();
        assert_eq!(result.file_paths, vec!["src/owned.rs".to_string()]);
    }
}
