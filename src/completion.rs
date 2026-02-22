//! Phase completion contract evaluation.
//!
//! A phase run is considered complete only when all required checks pass:
//! - all non-archived tasks are done
//! - milestone tasks are done
//! - phase summary artifact exists
//! - DoD/test command passes
//! - executor reached a stable completed state

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::ProjectConfig;
use crate::dod::{self, DodConfig};
use crate::orchestrator::OrchestratorResult;
use crate::task;

const DEFAULT_DOD_COMMAND: &str = "cargo test";

#[derive(Debug, Clone)]
pub struct CompletionDecision {
    pub is_complete: bool,
    pub board_all_done: bool,
    pub milestone_done: bool,
    pub summary_exists: bool,
    pub dod_passed: bool,
    pub executor_stable: bool,
    pub reasons: Vec<String>,
    pub summary_path: Option<PathBuf>,
    pub dod_command: String,
    pub dod_executed: bool,
    pub dod_exit_code: Option<i32>,
    pub dod_output_lines: usize,
}

impl CompletionDecision {
    pub fn failure_summary(&self) -> String {
        if self.is_complete {
            "completion contract passed".to_string()
        } else {
            format!("completion contract failed: {}", self.reasons.join("; "))
        }
    }
}

pub fn evaluate_phase_completion(
    phase: &str,
    execution_root: &Path,
    project_config: &ProjectConfig,
    orchestrator_result: &OrchestratorResult,
) -> Result<CompletionDecision> {
    let tasks_dir = execution_root.join("kanban").join(phase).join("tasks");
    let tasks = task::load_tasks_from_dir(&tasks_dir)
        .with_context(|| format!("failed to reload tasks from {}", tasks_dir.display()))?;

    let active_tasks: Vec<_> = tasks.iter().filter(|t| t.status != "archived").collect();

    let pending_tasks: Vec<_> = active_tasks
        .iter()
        .filter(|t| t.status != "done")
        .map(|t| format!("#{} ({})", t.id, t.status))
        .collect();
    let board_all_done = pending_tasks.is_empty();

    let milestones: Vec<_> = active_tasks
        .iter()
        .filter(|t| t.tags.iter().any(|tag| tag == "milestone"))
        .collect();
    let milestone_done = !milestones.is_empty() && milestones.iter().all(|t| t.status == "done");

    let summary_path = locate_phase_summary(execution_root, phase);
    let summary_exists = summary_path.is_some();

    let executor_stable = matches!(orchestrator_result, OrchestratorResult::Completed);

    let mut reasons = Vec::new();
    if !board_all_done {
        reasons.push(format!(
            "board incomplete; non-done tasks: {}",
            pending_tasks.join(", ")
        ));
    }
    if milestones.is_empty() {
        reasons.push("no milestone task found (expected a task tagged 'milestone')".to_string());
    } else if !milestone_done {
        let incomplete_milestones = milestones
            .iter()
            .filter(|t| t.status != "done")
            .map(|t| format!("#{} ({})", t.id, t.status))
            .collect::<Vec<_>>()
            .join(", ");
        reasons.push(format!(
            "milestone task not done; pending milestones: {incomplete_milestones}"
        ));
    }
    if !summary_exists {
        reasons.push(format!(
            "phase summary artifact missing; expected one of: {}",
            expected_summary_paths(execution_root, phase)
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !executor_stable {
        reasons.push(format!(
            "executor not in stable completed state ({})",
            describe_orchestrator_result(orchestrator_result)
        ));
    }

    let dod_command = project_config
        .defaults
        .dod
        .clone()
        .unwrap_or_else(|| DEFAULT_DOD_COMMAND.to_string());
    let should_run_dod = board_all_done && milestone_done && summary_exists && executor_stable;

    let (dod_passed, dod_executed, dod_exit_code, dod_output_lines) = if should_run_dod {
        let dod_config = DodConfig {
            command: dod_command.clone(),
            max_retries: 0,
            work_dir: execution_root.display().to_string(),
        };
        let result = dod::run_dod_command(&dod_config).with_context(|| {
            format!(
                "failed to execute completion DoD command '{}' in {}",
                dod_command,
                execution_root.display()
            )
        })?;
        let output_lines = result.output.lines().count();
        if !result.passed {
            reasons.push(format!(
                "DoD command failed: '{}' (exit code: {})",
                dod_command,
                result
                    .exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            ));
        }
        (result.passed, true, result.exit_code, output_lines)
    } else {
        (false, false, None, 0)
    };

    let is_complete =
        board_all_done && milestone_done && summary_exists && dod_passed && executor_stable;

    Ok(CompletionDecision {
        is_complete,
        board_all_done,
        milestone_done,
        summary_exists,
        dod_passed,
        executor_stable,
        reasons,
        summary_path,
        dod_command,
        dod_executed,
        dod_exit_code,
        dod_output_lines,
    })
}

fn expected_summary_paths(execution_root: &Path, phase: &str) -> Vec<PathBuf> {
    vec![
        execution_root.join("phase-summary.md"),
        execution_root
            .join("kanban")
            .join(phase)
            .join("phase-summary.md"),
    ]
}

fn locate_phase_summary(execution_root: &Path, phase: &str) -> Option<PathBuf> {
    expected_summary_paths(execution_root, phase)
        .into_iter()
        .find(|p| p.is_file())
}

fn describe_orchestrator_result(result: &OrchestratorResult) -> String {
    match result {
        OrchestratorResult::Completed => "completed".to_string(),
        OrchestratorResult::Detached => "detached".to_string(),
        OrchestratorResult::Error { detail } => format!("error: {detail}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_task_file(
        tasks_dir: &Path,
        id: u32,
        title: &str,
        status: &str,
        tags: &[&str],
    ) -> PathBuf {
        let path = tasks_dir.join(format!("{id:03}-{title}.md"));
        let tags_yaml = if tags.is_empty() {
            "[]".to_string()
        } else {
            let lines = tags
                .iter()
                .map(|tag| format!("  - {tag}"))
                .collect::<Vec<_>>()
                .join("\n");
            format!("\n{lines}")
        };
        let content = format!(
            "---\nid: {id}\ntitle: {title}\nstatus: {status}\npriority: high\ntags: {tags_yaml}\ndepends_on: []\nclass: standard\n---\n\nTask {id}\n"
        );
        fs::write(&path, content).unwrap();
        path
    }

    fn setup_phase(tmp: &Path, phase: &str) -> PathBuf {
        let tasks_dir = tmp.join("kanban").join(phase).join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        tasks_dir
    }

    #[test]
    fn completion_passes_when_all_checks_pass() {
        let tmp = tempfile::tempdir().unwrap();
        let phase = "phase-2.5";
        let tasks_dir = setup_phase(tmp.path(), phase);

        write_task_file(&tasks_dir, 1, "core", "done", &[]);
        write_task_file(&tasks_dir, 2, "exit", "done", &["milestone"]);
        fs::write(tmp.path().join("phase-summary.md"), "summary").unwrap();

        let mut config = ProjectConfig::default();
        config.defaults.dod = Some("true".to_string());

        let decision =
            evaluate_phase_completion(phase, tmp.path(), &config, &OrchestratorResult::Completed)
                .unwrap();

        assert!(decision.is_complete);
        assert!(decision.board_all_done);
        assert!(decision.milestone_done);
        assert!(decision.summary_exists);
        assert!(decision.dod_passed);
        assert!(decision.executor_stable);
        assert!(decision.reasons.is_empty());
    }

    #[test]
    fn completion_fails_for_incomplete_board() {
        let tmp = tempfile::tempdir().unwrap();
        let phase = "phase-2.5";
        let tasks_dir = setup_phase(tmp.path(), phase);

        write_task_file(&tasks_dir, 1, "core", "backlog", &[]);
        write_task_file(&tasks_dir, 2, "exit", "done", &["milestone"]);
        fs::write(tmp.path().join("phase-summary.md"), "summary").unwrap();

        let mut config = ProjectConfig::default();
        config.defaults.dod = Some("true".to_string());

        let decision =
            evaluate_phase_completion(phase, tmp.path(), &config, &OrchestratorResult::Completed)
                .unwrap();

        assert!(!decision.is_complete);
        assert!(!decision.board_all_done);
        assert!(!decision.dod_executed);
        assert!(
            decision
                .failure_summary()
                .contains("board incomplete; non-done tasks")
        );
    }

    #[test]
    fn completion_fails_when_milestone_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let phase = "phase-2.5";
        let tasks_dir = setup_phase(tmp.path(), phase);

        write_task_file(&tasks_dir, 1, "core", "done", &[]);
        fs::write(tmp.path().join("phase-summary.md"), "summary").unwrap();

        let mut config = ProjectConfig::default();
        config.defaults.dod = Some("true".to_string());

        let decision =
            evaluate_phase_completion(phase, tmp.path(), &config, &OrchestratorResult::Completed)
                .unwrap();

        assert!(!decision.is_complete);
        assert!(!decision.milestone_done);
        assert!(
            decision
                .failure_summary()
                .contains("no milestone task found")
        );
    }

    #[test]
    fn completion_fails_when_dod_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let phase = "phase-2.5";
        let tasks_dir = setup_phase(tmp.path(), phase);

        write_task_file(&tasks_dir, 1, "core", "done", &[]);
        write_task_file(&tasks_dir, 2, "exit", "done", &["milestone"]);
        fs::write(tmp.path().join("phase-summary.md"), "summary").unwrap();

        let mut config = ProjectConfig::default();
        config.defaults.dod = Some("false".to_string());

        let decision =
            evaluate_phase_completion(phase, tmp.path(), &config, &OrchestratorResult::Completed)
                .unwrap();

        assert!(!decision.is_complete);
        assert!(decision.dod_executed);
        assert!(!decision.dod_passed);
        assert!(decision.failure_summary().contains("DoD command failed"));
    }

    #[test]
    fn completion_fails_when_executor_not_stable() {
        let tmp = tempfile::tempdir().unwrap();
        let phase = "phase-2.5";
        let tasks_dir = setup_phase(tmp.path(), phase);

        write_task_file(&tasks_dir, 1, "core", "done", &[]);
        write_task_file(&tasks_dir, 2, "exit", "done", &["milestone"]);
        fs::write(tmp.path().join("phase-summary.md"), "summary").unwrap();

        let mut config = ProjectConfig::default();
        config.defaults.dod = Some("true".to_string());

        let decision =
            evaluate_phase_completion(phase, tmp.path(), &config, &OrchestratorResult::Detached)
                .unwrap();

        assert!(!decision.is_complete);
        assert!(!decision.executor_stable);
        assert!(!decision.dod_executed);
        assert!(
            decision
                .failure_summary()
                .contains("executor not in stable completed state")
        );
    }
}
