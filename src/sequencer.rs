//! Phase sequencing primitives for `batty work all`.
//!
//! This module discovers runnable phase boards, sorts them deterministically
//! by numeric phase order, skips already-complete phases, and provides
//! stop/continue policy helpers for multi-phase execution loops.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::log::{ExecutionLog, LogEvent};
use crate::task;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseCandidate {
    pub name: String,
    pub directory: PathBuf,
    pub order_key: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseSelectionDecision {
    pub phase: String,
    pub order_key: Vec<u32>,
    pub selected: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseDiscovery {
    pub selected: Vec<PhaseCandidate>,
    pub decisions: Vec<PhaseSelectionDecision>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequencerFailurePolicy {
    StopOnFailure,
    ContinueOnFailure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseRunOutcome {
    Merged,
    Failed,
    Escalated,
}

#[derive(Debug, Clone)]
struct ParsedPhaseEntry {
    name: String,
    directory: PathBuf,
    order_key: Vec<u32>,
}

/// Discover phase directories and produce a deterministic run plan.
///
/// Selection rules:
/// - include only directories matching `phase-<numeric>[.<numeric>...]`
/// - sort by numeric phase order, then by phase name as a tie-breaker
/// - skip phases that are already complete (all non-archived tasks are `done`)
pub fn discover_phases_for_sequencing(project_root: &Path) -> Result<PhaseDiscovery> {
    let kanban_root = crate::paths::resolve_kanban_root(project_root);
    let mut parsed = Vec::new();

    for entry in std::fs::read_dir(&kanban_root)
        .with_context(|| format!("failed to read kanban root {}", kanban_root.display()))?
    {
        let entry = entry?;
        let directory = entry.path();
        if !directory.is_dir() {
            continue;
        }

        let name = entry.file_name().to_string_lossy().to_string();
        let Some(order_key) = parse_phase_order(&name) else {
            continue;
        };

        parsed.push(ParsedPhaseEntry {
            name,
            directory,
            order_key,
        });
    }

    parsed.sort_by(|a, b| {
        a.order_key
            .cmp(&b.order_key)
            .then_with(|| a.name.cmp(&b.name))
    });

    let mut selected = Vec::new();
    let mut decisions = Vec::new();
    for phase in parsed {
        let complete = phase_is_complete(&phase.directory).with_context(|| {
            format!(
                "failed to determine completion state for phase {}",
                phase.name
            )
        })?;

        if complete {
            decisions.push(PhaseSelectionDecision {
                phase: phase.name,
                order_key: phase.order_key,
                selected: false,
                reason: "phase already complete (all active tasks are done)".to_string(),
            });
            continue;
        }

        decisions.push(PhaseSelectionDecision {
            phase: phase.name.clone(),
            order_key: phase.order_key.clone(),
            selected: true,
            reason: "phase selected for execution".to_string(),
        });
        selected.push(PhaseCandidate {
            name: phase.name,
            directory: phase.directory,
            order_key: phase.order_key,
        });
    }

    Ok(PhaseDiscovery {
        selected,
        decisions,
    })
}

/// Parse a phase name into sortable numeric segments.
///
/// Examples:
/// - `phase-1` -> `[1]`
/// - `phase-2.5` -> `[2, 5]`
/// - `phase-3b` -> `None`
pub fn parse_phase_order(phase: &str) -> Option<Vec<u32>> {
    let suffix = phase.strip_prefix("phase-")?;
    if suffix.is_empty() {
        return None;
    }

    let mut segments = Vec::new();
    for piece in suffix.split('.') {
        if piece.is_empty() {
            return None;
        }
        if !piece.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let value = piece.parse::<u32>().ok()?;
        segments.push(value);
    }

    Some(segments)
}

fn phase_is_complete(phase_dir: &Path) -> Result<bool> {
    let tasks_dir = phase_dir.join("tasks");
    if !tasks_dir.is_dir() {
        return Ok(false);
    }

    let tasks = task::load_tasks_from_dir(&tasks_dir)
        .with_context(|| format!("failed to load tasks from {}", tasks_dir.display()))?;
    let mut active_count = 0usize;
    for task in tasks {
        if task.status == "archived" {
            continue;
        }
        active_count += 1;
        if task.status != "done" {
            return Ok(false);
        }
    }

    Ok(active_count > 0)
}

/// Decide whether sequencer should continue after a phase outcome.
///
/// Default behavior is fail-fast: stop on `failed` or `escalated`.
pub fn should_continue_after_phase(
    outcome: PhaseRunOutcome,
    policy: SequencerFailurePolicy,
) -> bool {
    match outcome {
        PhaseRunOutcome::Merged => true,
        PhaseRunOutcome::Failed | PhaseRunOutcome::Escalated => {
            matches!(policy, SequencerFailurePolicy::ContinueOnFailure)
        }
    }
}

/// Write all phase-selection decisions to the structured execution log.
pub fn log_phase_selection_decisions(
    execution_log: &ExecutionLog,
    decisions: &[PhaseSelectionDecision],
) -> Result<()> {
    for decision in decisions {
        execution_log.log(LogEvent::PhaseSelectionDecision {
            phase: decision.phase.clone(),
            order_key: format_order_key(&decision.order_key),
            selected: decision.selected,
            reason: decision.reason.clone(),
        })?;
    }
    Ok(())
}

fn format_order_key(order_key: &[u32]) -> String {
    order_key
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(".")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_task(tasks_dir: &Path, id: u32, title: &str, status: &str) {
        let file = tasks_dir.join(format!("{id:03}-{title}.md"));
        let content = format!(
            "---\nid: {id}\ntitle: {title}\nstatus: {status}\npriority: high\ntags: []\ndepends_on: []\nclass: standard\n---\n\nTask {id}\n"
        );
        fs::write(file, content).unwrap();
    }

    fn setup_phase(project_root: &Path, phase: &str, statuses: &[&str]) -> PathBuf {
        let dir = project_root
            .join(".batty")
            .join("kanban")
            .join(phase)
            .join("tasks");
        fs::create_dir_all(&dir).unwrap();
        for (idx, status) in statuses.iter().enumerate() {
            write_task(&dir, (idx + 1) as u32, &format!("task-{}", idx + 1), status);
        }
        dir.parent().unwrap().to_path_buf()
    }

    #[test]
    fn parse_phase_order_accepts_numeric_formats() {
        assert_eq!(parse_phase_order("phase-1"), Some(vec![1]));
        assert_eq!(parse_phase_order("phase-2.5"), Some(vec![2, 5]));
        assert_eq!(parse_phase_order("phase-10.2.3"), Some(vec![10, 2, 3]));
    }

    #[test]
    fn parse_phase_order_rejects_non_numeric_formats() {
        assert_eq!(parse_phase_order("phase-"), None);
        assert_eq!(parse_phase_order("phase-3b"), None);
        assert_eq!(parse_phase_order("phase-a"), None);
        assert_eq!(parse_phase_order("docs-update"), None);
    }

    #[test]
    fn discovery_sorts_deterministically_and_skips_completed_phases() {
        let tmp = tempfile::tempdir().unwrap();
        setup_phase(tmp.path(), "phase-2.10", &["backlog"]);
        setup_phase(tmp.path(), "phase-1", &["done"]);
        setup_phase(tmp.path(), "phase-2", &["backlog"]);
        setup_phase(tmp.path(), "phase-2.4", &["in-progress"]);
        setup_phase(tmp.path(), "phase-3", &["todo"]);
        fs::create_dir_all(tmp.path().join(".batty").join("kanban").join("phase-3b")).unwrap();

        let discovery = discover_phases_for_sequencing(tmp.path()).unwrap();

        let selected: Vec<_> = discovery.selected.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            selected,
            vec!["phase-2", "phase-2.4", "phase-2.10", "phase-3"]
        );

        let skipped_complete = discovery
            .decisions
            .iter()
            .find(|d| d.phase == "phase-1")
            .unwrap();
        assert!(!skipped_complete.selected);
        assert!(skipped_complete.reason.contains("already complete"));
    }

    #[test]
    fn stop_policy_is_fail_fast_by_default() {
        assert!(should_continue_after_phase(
            PhaseRunOutcome::Merged,
            SequencerFailurePolicy::StopOnFailure
        ));
        assert!(!should_continue_after_phase(
            PhaseRunOutcome::Failed,
            SequencerFailurePolicy::StopOnFailure
        ));
        assert!(!should_continue_after_phase(
            PhaseRunOutcome::Escalated,
            SequencerFailurePolicy::StopOnFailure
        ));
    }

    #[test]
    fn continue_policy_allows_progress_after_failures() {
        assert!(should_continue_after_phase(
            PhaseRunOutcome::Merged,
            SequencerFailurePolicy::ContinueOnFailure
        ));
        assert!(should_continue_after_phase(
            PhaseRunOutcome::Failed,
            SequencerFailurePolicy::ContinueOnFailure
        ));
        assert!(should_continue_after_phase(
            PhaseRunOutcome::Escalated,
            SequencerFailurePolicy::ContinueOnFailure
        ));
    }

    #[test]
    fn logs_phase_selection_decisions_for_auditability() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("execution.jsonl");
        let log = ExecutionLog::new(&log_path).unwrap();
        let decisions = vec![
            PhaseSelectionDecision {
                phase: "phase-2".to_string(),
                order_key: vec![2],
                selected: true,
                reason: "phase selected for execution".to_string(),
            },
            PhaseSelectionDecision {
                phase: "phase-1".to_string(),
                order_key: vec![1],
                selected: false,
                reason: "phase already complete (all active tasks are done)".to_string(),
            },
        ];

        log_phase_selection_decisions(&log, &decisions).unwrap();

        let content = fs::read_to_string(log_path).unwrap();
        let lines: Vec<_> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"event\":\"phase_selection_decision\""));
        assert!(lines[0].contains("\"phase\":\"phase-2\""));
        assert!(lines[0].contains("\"order_key\":\"2\""));
        assert!(lines[1].contains("\"selected\":false"));
    }
}
