//! Engineer completion handling — the daemon's entry point when an engineer
//! reports a task as done.
//!
//! Validates commits, runs tests, evaluates auto-merge policy, performs the
//! merge, and handles retries and escalation.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use crate::task::load_tasks_from_dir;
#[cfg(test)]
use crate::team::artifact::read_test_timing_log;
use crate::team::artifact::{append_test_timing_record, track_artifact};
use crate::team::auto_merge::{self, AutoMergeDecisionKind};
use crate::team::board::{WorkflowMetadata, write_workflow_metadata};
use crate::team::daemon::verification::{inspect_scope_fence, run_automatic_verification};
use crate::team::daemon::{MergeRequest, TeamDaemon};
use crate::team::task_loop::{
    checkout_worktree_branch_from_main, current_worktree_branch, engineer_base_branch_name,
    read_task_title,
};
use crate::team::telemetry_db;
use crate::team::test_results::{TestResults, TestRunOutput};
use crate::team::verification::{EvidenceKind, VerificationPhase, VerificationState};

use super::git_ops::{
    code_files_changed_from_main, commits_ahead_of_main, diff_stat_from_main,
    files_changed_from_main, now_unix, run_git_with_context,
};
use super::lock::{
    MergeLock, MergeMode, MergeOutcome, MergeSuccess, infer_merge_mode_from_failure,
};
use super::operations::merge_engineer_branch;

fn transition_verification_phase(
    daemon: &mut TeamDaemon,
    engineer: &str,
    task_id: u32,
    state: &mut VerificationState,
    next_phase: VerificationPhase,
) {
    let next_phase_name = format!("{next_phase:?}").to_ascii_lowercase();
    let previous_phase = state.transition(next_phase);
    let previous_phase_name = format!("{previous_phase:?}").to_ascii_lowercase();
    daemon.record_verification_phase_changed(
        engineer,
        task_id,
        &previous_phase_name,
        &next_phase_name,
        state.iteration,
    );
}

fn record_verification_evidence(
    daemon: &mut TeamDaemon,
    engineer: &str,
    task_id: u32,
    state: &mut VerificationState,
    kind: EvidenceKind,
    detail: impl Into<String>,
) {
    let detail = detail.into();
    let kind_name = kind_name(&kind);
    state.record_evidence(kind, detail.clone());
    daemon.record_verification_evidence_collected(engineer, task_id, &kind_name, &detail);
}

fn kind_name(kind: &EvidenceKind) -> String {
    let mut name = String::new();
    for (index, ch) in format!("{kind:?}").chars().enumerate() {
        if ch.is_ascii_uppercase() && index > 0 {
            name.push('_');
        }
        name.push(ch.to_ascii_lowercase());
    }
    name
}

fn verification_fix_message(
    state: &VerificationState,
    headline: &str,
    failures: &[String],
    file_paths: &[String],
    output: &str,
) -> String {
    let mut body = format!(
        "{headline}\nFix attempt {}/{}.\n",
        state.iteration,
        verification_retry_budget(state.max_iterations)
    );
    if !failures.is_empty() {
        body.push_str("Failures:\n");
        for failure in failures.iter().take(8) {
            body.push_str("- ");
            body.push_str(failure);
            body.push('\n');
        }
    }
    if !file_paths.is_empty() {
        body.push_str("Likely files to inspect:\n");
        for path in file_paths.iter().take(8) {
            body.push_str("- ");
            body.push_str(path);
            body.push('\n');
        }
    }
    body.push_str("Latest verification output:\n```\n");
    body.push_str(output.trim());
    body.push_str("\n```\nFix these failures, then report completion again.");
    body
}

fn verification_retry_budget(max_iterations: u32) -> u32 {
    max_iterations.clamp(1, 3)
}

fn verification_outcome_label(phase: &VerificationPhase) -> &'static str {
    match phase {
        VerificationPhase::Fixing => "verification_retry_required",
        VerificationPhase::Complete => "verification_passed",
        VerificationPhase::Failed => "verification_escalated",
        VerificationPhase::Executing | VerificationPhase::Verifying => "verification_pending",
    }
}

fn persist_verification_snapshot(
    project_root: &Path,
    board_dir: &Path,
    task_id: u32,
    engineer: &str,
    state: &VerificationState,
    verification_run: &crate::team::daemon::verification::VerificationRunResult,
) -> Result<String> {
    let attempt = state.iteration.max(1);
    let relative_path = Path::new(".batty")
        .join("reports")
        .join("verification")
        .join("completion")
        .join(format!(
            "task-{task_id:03}-{engineer}-attempt-{attempt}.json"
        ));
    let absolute_path = project_root.join(&relative_path);
    if let Some(parent) = absolute_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let snapshot = serde_json::json!({
        "task_id": task_id,
        "engineer": engineer,
        "phase": format!("{:?}", state.phase).to_ascii_lowercase(),
        "attempt": attempt,
        "max_attempts": state.max_iterations,
        "outcome": verification_outcome_label(&state.phase),
        "passed": verification_run.passed,
        "results": verification_run.results,
        "failures": verification_run.failures,
        "file_paths": verification_run.file_paths,
        "evidence": state.evidence,
        "output": verification_run.output,
    });
    let serialized =
        serde_json::to_string_pretty(&snapshot).context("failed to serialize verification data")?;
    std::fs::write(&absolute_path, serialized)
        .with_context(|| format!("failed to write {}", absolute_path.display()))?;

    let task_path = crate::team::task_cmd::find_task_path(board_dir, task_id)?;
    let mut metadata = crate::team::board::read_workflow_metadata(&task_path)?;
    metadata.tests_run = Some(true);
    metadata.tests_passed = Some(verification_run.passed);
    metadata.test_results = Some(verification_run.results.clone());
    metadata.outcome = Some(verification_outcome_label(&state.phase).to_string());
    track_artifact(&mut metadata, &relative_path.to_string_lossy());
    crate::team::board::write_workflow_metadata(&task_path, &metadata)?;

    Ok(relative_path.to_string_lossy().into_owned())
}

fn structured_failure_details(results: &TestResults) -> Vec<String> {
    results
        .failures
        .iter()
        .map(|failure| {
            let mut detail = failure.test_name.clone();
            if let Some(message) = failure
                .message
                .as_deref()
                .filter(|message| !message.is_empty())
            {
                detail.push_str(" (");
                detail.push_str(message);
                if let Some(location) = failure
                    .location
                    .as_deref()
                    .filter(|location| !location.is_empty())
                {
                    detail.push_str(" at ");
                    detail.push_str(location);
                }
                detail.push(')');
            } else if let Some(location) = failure
                .location
                .as_deref()
                .filter(|location| !location.is_empty())
            {
                detail.push_str(" (at ");
                detail.push_str(location);
                detail.push(')');
            }
            detail
        })
        .collect()
}

fn empty_test_results(framework: &str, summary: Option<String>) -> TestResults {
    TestResults {
        framework: framework.to_string(),
        total: None,
        passed: 0,
        failed: 0,
        ignored: 0,
        failures: Vec::new(),
        summary,
    }
}

fn move_task_to_review(
    daemon: &mut TeamDaemon,
    board_dir: &Path,
    task_id: u32,
    manager_name: Option<&str>,
    engineer: &str,
) -> Result<bool> {
    let worktree_dir = daemon.worktree_dir(engineer);
    let commits_ahead = if daemon.is_multi_repo {
        multi_repo_commits_ahead_of_main(&worktree_dir, &daemon.sub_repo_names)?
    } else {
        commits_ahead_of_main(&worktree_dir)?
    };

    if commits_ahead == 0 {
        warn!(
            engineer,
            task_id, "refusing to move task to review because branch has no commits ahead of main"
        );
        let msg = "Review blocked: your branch has no commits ahead of main, so Batty kept the task in progress instead of moving it to review.\n\
                   Commit the work first:\n\
                   ```\n\
                   git add -A\n\
                   git commit -m \"your task description\"\n\
                   ```";
        daemon.queue_message("batty", engineer, msg)?;
        return Ok(false);
    }

    if let Err(error) = crate::team::task_cmd::transition_task(board_dir, task_id, "review") {
        warn!(
            engineer,
            task_id,
            error = %error,
            "failed to move task to review — attempting force via in-progress first"
        );
        // If the task is in an unexpected state (e.g. blocked), try
        // transitioning through in-progress first, then to review.
        // This prevents the stuck loop where completion fires repeatedly
        // but the state transition always fails.
        let _ = crate::team::task_cmd::transition_task(board_dir, task_id, "in-progress");
        if let Err(error2) = crate::team::task_cmd::transition_task(board_dir, task_id, "review") {
            warn!(
                engineer,
                task_id,
                error = %error2,
                "force review transition also failed — leaving task in current state"
            );
        }
    }
    if let Some(manager_name) = manager_name
        && let Err(error) = crate::team::task_cmd::assign_task_owners(
            board_dir,
            task_id,
            Some(engineer),
            Some(manager_name),
        )
    {
        warn!(
            engineer,
            task_id,
            manager = manager_name,
            error = %error,
            "failed to set review owner"
        );
    }
    Ok(true)
}

pub(crate) fn handle_engineer_completion(daemon: &mut TeamDaemon, engineer: &str) -> Result<()> {
    let Some(task_id) = daemon.active_task_id(engineer) else {
        return Ok(());
    };

    if !daemon.member_uses_worktrees(engineer) {
        return Ok(());
    }

    let worktree_dir = daemon.worktree_dir(engineer);
    let board_dir = daemon.board_dir();
    let board_dir_str = board_dir.to_string_lossy().to_string();
    let manager_name = daemon.manager_name(engineer);

    let total_commits = if daemon.is_multi_repo {
        multi_repo_commits_ahead_of_main(&worktree_dir, &daemon.sub_repo_names)?
    } else {
        commits_ahead_of_main(&worktree_dir)?
    };
    let diff_stat = if daemon.is_multi_repo {
        multi_repo_diff_stat_from_main(&worktree_dir, &daemon.sub_repo_names)?
    } else {
        diff_stat_from_main(&worktree_dir)?
    };
    let files_changed = if daemon.is_multi_repo {
        multi_repo_files_changed_from_main(&worktree_dir, &daemon.sub_repo_names)?
    } else {
        files_changed_from_main(&worktree_dir)?
    };
    let code_files_changed = if daemon.is_multi_repo {
        multi_repo_code_files_changed_from_main(&worktree_dir, &daemon.sub_repo_names)?
    } else {
        code_files_changed_from_main(&worktree_dir)?
    };
    let verification_policy = daemon
        .config
        .team_config
        .workflow_policy
        .verification
        .clone();
    let test_command = verification_policy.test_command.clone().or_else(|| {
        daemon
            .config
            .team_config
            .workflow_policy
            .test_command
            .clone()
    });
    let mut verification_state = daemon
        .verification_states
        .remove(engineer)
        .unwrap_or_else(|| VerificationState::new(verification_policy.max_iterations));
    verification_state.max_iterations =
        verification_retry_budget(verification_policy.max_iterations);
    verification_state.clear_evidence();

    transition_verification_phase(
        daemon,
        engineer,
        task_id,
        &mut verification_state,
        VerificationPhase::Verifying,
    );

    let scope_fence = inspect_scope_fence(&worktree_dir)?;
    if let Some(scope_fence) = scope_fence.as_ref()
        && !scope_fence.out_of_scope_files.is_empty()
    {
        let violation_list = scope_fence.out_of_scope_files.join(", ");
        warn!(
            engineer,
            task_id,
            files = %violation_list,
            "completion rejected: engineer modified protected files outside task scope"
        );
        daemon.record_orchestrator_action(format!(
            "scope violation: {engineer} modified fenced files outside declared scope: {violation_list} — reverting out-of-scope changes"
        ));
        revert_out_of_scope_files(&worktree_dir, &scope_fence.out_of_scope_files)?;
        daemon.emit_event(crate::team::events::TeamEvent::scope_fence_violation(
            engineer,
            task_id,
            &format!(
                "reclaim_requested=true out_of_scope_files={}",
                violation_list
            ),
        ));
    }

    if total_commits > 0 {
        record_verification_evidence(
            daemon,
            engineer,
            task_id,
            &mut verification_state,
            EvidenceKind::CommitsAhead,
            format!("commits_ahead={total_commits}"),
        );
    }
    if files_changed > 0 {
        record_verification_evidence(
            daemon,
            engineer,
            task_id,
            &mut verification_state,
            EvidenceKind::FilesChanged,
            format!("files_changed={files_changed}"),
        );
    }
    if code_files_changed > 0 {
        record_verification_evidence(
            daemon,
            engineer,
            task_id,
            &mut verification_state,
            EvidenceKind::CodeFilesChanged,
            format!("code_files_changed={code_files_changed}"),
        );
    }

    let (verification_run, test_duration_ms) = if let Some(scope_fence) = scope_fence.as_ref() {
        if !scope_fence.ack_present || !scope_fence.out_of_scope_files.is_empty() {
            (
                run_automatic_verification(&worktree_dir, test_command.as_deref())?,
                0,
            )
        } else if verification_policy.auto_run_tests {
            verification_state.begin_iteration();
            let test_started = Instant::now();
            let verification_run =
                run_automatic_verification(&worktree_dir, test_command.as_deref()).with_context(
                    || {
                        format!(
                            "automatic verification failed while running tests in {}",
                            worktree_dir.display()
                        )
                    },
                )?;
            let test_duration_ms = test_started.elapsed().as_millis() as u64;
            verification_state.last_test_passed = verification_run.passed;
            verification_state.last_test_output = Some(verification_run.output.clone());
            if verification_run.passed {
                record_verification_evidence(
                    daemon,
                    engineer,
                    task_id,
                    &mut verification_state,
                    EvidenceKind::TestsPassed,
                    "tests_passed".to_string(),
                );
            } else {
                record_verification_evidence(
                    daemon,
                    engineer,
                    task_id,
                    &mut verification_state,
                    EvidenceKind::TestsFailed,
                    verification_run
                        .failures
                        .first()
                        .cloned()
                        .unwrap_or_else(|| "tests_failed".to_string()),
                );
            }
            (verification_run, test_duration_ms)
        } else {
            let output =
                "automatic verification skipped because workflow_policy.verification.auto_run_tests=false"
                    .to_string();
            verification_state.last_test_passed = true;
            verification_state.last_test_output = Some(output.clone());
            record_verification_evidence(
                daemon,
                engineer,
                task_id,
                &mut verification_state,
                EvidenceKind::TestsPassed,
                "automatic verification skipped by policy".to_string(),
            );
            (
                crate::team::daemon::verification::VerificationRunResult {
                    passed: true,
                    output,
                    results: empty_test_results(
                        "cargo",
                        Some("automatic verification skipped by policy".to_string()),
                    ),
                    failures: Vec::new(),
                    file_paths: Vec::new(),
                },
                0,
            )
        }
    } else if verification_policy.auto_run_tests {
        verification_state.begin_iteration();
        let test_started = Instant::now();
        let verification_run = run_automatic_verification(&worktree_dir, test_command.as_deref())
            .with_context(|| {
            format!(
                "automatic verification failed while running tests in {}",
                worktree_dir.display()
            )
        })?;
        let test_duration_ms = test_started.elapsed().as_millis() as u64;
        verification_state.last_test_passed = verification_run.passed;
        verification_state.last_test_output = Some(verification_run.output.clone());
        if verification_run.passed {
            record_verification_evidence(
                daemon,
                engineer,
                task_id,
                &mut verification_state,
                EvidenceKind::TestsPassed,
                "tests_passed".to_string(),
            );
        } else {
            record_verification_evidence(
                daemon,
                engineer,
                task_id,
                &mut verification_state,
                EvidenceKind::TestsFailed,
                verification_run
                    .failures
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "tests_failed".to_string()),
            );
        }
        (verification_run, test_duration_ms)
    } else {
        let output =
            "automatic verification skipped because workflow_policy.verification.auto_run_tests=false"
                .to_string();
        verification_state.last_test_passed = true;
        verification_state.last_test_output = Some(output.clone());
        record_verification_evidence(
            daemon,
            engineer,
            task_id,
            &mut verification_state,
            EvidenceKind::TestsPassed,
            "automatic verification skipped by policy".to_string(),
        );
        (
            crate::team::daemon::verification::VerificationRunResult {
                passed: true,
                output,
                results: empty_test_results(
                    "cargo",
                    Some("automatic verification skipped by policy".to_string()),
                ),
                failures: Vec::new(),
                file_paths: Vec::new(),
            },
            0,
        )
    };

    let has_required_evidence = if verification_policy.require_evidence {
        total_commits > 0 && code_files_changed > 0
    } else {
        true
    };

    if !has_required_evidence || !verification_run.passed {
        let verification_results = verification_run.results.clone();
        if !verification_run.passed
            && let Some(conn) = &daemon.telemetry_db
        {
            telemetry_db::record_test_results(conn, task_id, engineer, &verification_results, &[])?;
        }
        let is_zero_commit = !has_required_evidence && total_commits == 0;
        let is_narration_only = !has_required_evidence && code_files_changed == 0;
        let narration_count = if is_narration_only {
            let count = {
                let entry = daemon
                    .narration_rejection_counts
                    .entry(task_id)
                    .or_insert(0);
                *entry += 1;
                *entry
            };
            daemon.record_narration_rejection(engineer, task_id, count);
            count
        } else {
            0
        };
        let should_escalate = if is_narration_only {
            narration_count >= 2
        } else if is_zero_commit {
            false
        } else {
            verification_state.reached_max_iterations()
        };
        let next_phase = if should_escalate {
            VerificationPhase::Failed
        } else {
            VerificationPhase::Fixing
        };
        transition_verification_phase(
            daemon,
            engineer,
            task_id,
            &mut verification_state,
            next_phase,
        );
        persist_verification_snapshot(
            daemon.project_root(),
            &board_dir,
            task_id,
            engineer,
            &verification_state,
            &verification_run,
        )?;

        let headline = if !has_required_evidence {
            if total_commits == 0 {
                format!(
                    "Verification rejected this completion because there are no commits ahead of main. Run `git add -A`, create a real commit, and inspect `git diff --stat main..HEAD` before reporting completion again. Commits ahead of main: {total_commits}. Files changed: {files_changed}. Code files changed: {code_files_changed}. Diff stat: {}",
                    if diff_stat.is_empty() {
                        "(empty)".to_string()
                    } else {
                        diff_stat.clone()
                    }
                )
            } else if code_files_changed == 0 {
                let narration_prefix = if files_changed == 0 {
                    "Your previous attempt only produced commentary. Execute the actual commands to make the code changes."
                } else {
                    "Verification found no code changes in this completion."
                };
                format!(
                    "{narration_prefix} Inspect `git diff --stat main..HEAD` and make an actual code change before reporting completion again. Commits ahead of main: {total_commits}. Files changed: {files_changed}. Code files changed: {code_files_changed}. Diff stat: {}",
                    if diff_stat.is_empty() {
                        "(empty)".to_string()
                    } else {
                        diff_stat.clone()
                    }
                )
            } else {
                format!(
                    "Verification could not accept this completion: no sufficient task evidence was found. Commits ahead of main: {total_commits}. Files changed: {files_changed}. Code files changed: {code_files_changed}. Diff stat: {}",
                    if diff_stat.is_empty() {
                        "(empty)".to_string()
                    } else {
                        diff_stat.clone()
                    }
                )
            }
        } else {
            format!(
                "Verification failed because the test command did not pass. Summary: {}.",
                verification_results.failure_summary()
            )
        };
        let engineer_message = verification_fix_message(
            &verification_state,
            &headline,
            &structured_failure_details(&verification_results),
            &verification_run.file_paths,
            &verification_run.output,
        );
        daemon.queue_message("batty", engineer, &engineer_message)?;

        if should_escalate && let Some(ref manager_name) = manager_name {
            daemon.record_verification_max_iterations_reached(
                engineer,
                task_id,
                verification_state.iteration,
                manager_name,
            );
            let manager_message = if !has_required_evidence && code_files_changed == 0 {
                format!(
                    "[daemon] Engineer {engineer} hit the narration-only quality gate {} times on task #{task_id}.\nReason: completion diff is narration-only.\nAttempts: {}/{}\nDiff stat: {}\nLatest output:\n```\\n{}\\n```",
                    narration_count,
                    verification_state.iteration,
                    verification_state.max_iterations,
                    if diff_stat.is_empty() {
                        "(empty)".to_string()
                    } else {
                        diff_stat.clone()
                    },
                    verification_run.output.trim()
                )
            } else if has_required_evidence {
                format!(
                    "[daemon] Engineer {engineer} hit verification max iterations on task #{task_id}.\nLatest phase: failed\nAttempts: {}/{}\nSummary: {}\nRecent failures:\n{}\nLatest output:\n```\\n{}\\n```",
                    verification_state.iteration,
                    verification_state.max_iterations,
                    verification_results.failure_summary(),
                    structured_failure_details(&verification_results)
                        .iter()
                        .take(8)
                        .map(|failure| format!("- {failure}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    verification_run.output.trim()
                )
            } else {
                format!(
                    "[daemon] Engineer {engineer} hit verification max iterations on task #{task_id}.\nLatest phase: failed\nAttempts: {}/{}\nDiff stat: {}\nRecent failures:\n{}\nLatest output:\n```\\n{}\\n```",
                    verification_state.iteration,
                    verification_state.max_iterations,
                    if diff_stat.is_empty() {
                        "(empty)".to_string()
                    } else {
                        diff_stat.clone()
                    },
                    verification_run
                        .failures
                        .iter()
                        .take(8)
                        .map(|failure| format!("- {failure}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    verification_run.output.trim()
                )
            };
            daemon.queue_message("daemon", manager_name, &manager_message)?;
        }

        if should_escalate {
            let block_reason = if !has_required_evidence && code_files_changed == 0 {
                format!(
                    "verification escalation after {} attempts: narration-only completion",
                    verification_state.iteration
                )
            } else if has_required_evidence {
                format!(
                    "verification escalation after {} attempts: {}",
                    verification_state.iteration,
                    verification_results.failure_summary()
                )
            } else {
                format!(
                    "verification escalation after {} attempts: insufficient task evidence",
                    verification_state.iteration
                )
            };
            daemon.record_task_escalated(
                engineer,
                task_id.to_string(),
                Some("verification_failed"),
            );
            crate::team::task_cmd::transition_task(&board_dir, task_id, "blocked")?;
            let mut blocked_fields = std::collections::HashMap::new();
            blocked_fields.insert("blocked_on".to_string(), block_reason);
            crate::team::task_cmd::cmd_update(&board_dir, task_id, blocked_fields)?;
            daemon.retry_counts.remove(engineer);
            daemon.verification_states.remove(engineer);
            daemon.clear_active_task(engineer);
            daemon.set_member_idle(engineer);
            daemon.narration_rejection_counts.remove(&task_id);
            return Ok(());
        }

        daemon
            .verification_states
            .insert(engineer.to_string(), verification_state);
        return Ok(());
    }

    daemon.narration_rejection_counts.remove(&task_id);
    transition_verification_phase(
        daemon,
        engineer,
        task_id,
        &mut verification_state,
        VerificationPhase::Complete,
    );
    persist_verification_snapshot(
        daemon.project_root(),
        &board_dir,
        task_id,
        engineer,
        &verification_state,
        &verification_run,
    )?;
    daemon.verification_states.remove(engineer);

    let task_branch = if daemon.is_multi_repo {
        multi_repo_task_branch(&worktree_dir, &daemon.sub_repo_names)?
    } else {
        current_worktree_branch(&worktree_dir)?
    };
    let previous_results = load_previous_test_results(&board_dir, task_id)?;
    let test_run = TestRunOutput {
        passed: verification_run.passed,
        output: verification_run.output.clone(),
        results: verification_run.results.clone(),
    };
    let flaky_failures = if test_run.passed {
        detect_flaky_failures(previous_results.as_ref(), &test_run.results)
    } else {
        Vec::new()
    };
    if let Some(conn) = &daemon.telemetry_db {
        telemetry_db::record_test_results(
            conn,
            task_id,
            engineer,
            &test_run.results,
            &flaky_failures,
        )?;
    }
    let tests_passed = test_run.passed;
    if tests_passed {
        let task_title = read_task_title(&board_dir, task_id);

        // --- Confidence scoring (always runs for observability) ---
        let policy = daemon.config.team_config.workflow_policy.auto_merge.clone();
        let auto_merge_override = daemon.auto_merge_override(task_id);

        // Analyze diff and emit confidence score for every completed task
        let diff_analysis = auto_merge::analyze_diff(daemon.project_root(), "main", &task_branch);
        if let Ok(ref summary) = diff_analysis {
            let confidence = auto_merge::score_auto_merge_candidate(summary, &policy);
            let task_str = task_id.to_string();
            let info = super::super::events::MergeConfidenceInfo {
                engineer,
                task: &task_str,
                confidence,
                files_changed: summary.files_changed,
                lines_changed: summary.total_lines(),
                has_migrations: summary.has_migrations,
                has_config_changes: summary.has_config_changes,
                rename_count: summary.rename_count,
            };
            daemon.record_merge_confidence_scored(&info);
        }

        // If override explicitly disables auto-merge, route to manual review
        if auto_merge_override == Some(false) {
            let decision = auto_merge::forced_manual_review_decision(
                diff_analysis.as_ref().ok(),
                &policy,
                true,
            );
            daemon.record_auto_merge_decision(engineer, task_id, &decision);
            info!(
                engineer,
                task_id, "auto-merge disabled by per-task override, routing to manual review"
            );
            if !move_task_to_review(
                daemon,
                &board_dir,
                task_id,
                manager_name.as_deref(),
                engineer,
            )? {
                return Ok(());
            }
            if let Some(ref manager_name) = manager_name {
                let msg = format!(
                    "[{engineer}] Task #{task_id} passed tests. Auto-merge disabled by override — awaiting manual review.\nTitle: {task_title}"
                );
                daemon.queue_message(engineer, manager_name, &msg)?;
                daemon.mark_member_working(manager_name);
            }
            daemon.clear_active_task(engineer);
            daemon.record_task_completed(engineer, Some(task_id));
            daemon.set_member_idle(engineer);
            return Ok(());
        }

        // Evaluate auto-merge if policy is enabled or override forces it
        let should_try_auto_merge = auto_merge_override == Some(true) || policy.enabled;
        if should_try_auto_merge {
            match diff_analysis {
                Ok(ref summary) => {
                    let decision = if auto_merge_override == Some(true) {
                        auto_merge::forced_auto_merge_decision(Some(summary), &policy, true)
                    } else {
                        auto_merge::evaluate_auto_merge_candidate(summary, &policy, true)
                    };
                    daemon.record_auto_merge_decision(engineer, task_id, &decision);

                    match decision.decision {
                        AutoMergeDecisionKind::Accepted => {
                            if !move_task_to_review(
                                daemon,
                                &board_dir,
                                task_id,
                                manager_name.as_deref(),
                                engineer,
                            )? {
                                return Ok(());
                            }
                            info!(
                                engineer,
                                task_id,
                                confidence = decision.confidence,
                                files = summary.files_changed,
                                lines = summary.total_lines(),
                                "auto-merging task"
                            );
                            daemon.enqueue_merge_request(MergeRequest {
                                task_id,
                                engineer: engineer.to_string(),
                                branch: task_branch.clone(),
                                worktree_dir: worktree_dir.clone(),
                                queued_at: Instant::now(),
                                test_passed: true,
                                should_post_merge_verify: verification_policy.auto_run_tests,
                                test_duration_ms,
                                confidence: decision.confidence,
                                files_changed: summary.files_changed,
                                lines_changed: summary.total_lines(),
                            });
                            return Ok(());
                        }
                        AutoMergeDecisionKind::ManualReview => {
                            info!(
                                engineer,
                                task_id,
                                confidence = decision.confidence,
                                reasons = ?decision.reasons,
                                "routing to manual review"
                            );
                            if !move_task_to_review(
                                daemon,
                                &board_dir,
                                task_id,
                                manager_name.as_deref(),
                                engineer,
                            )? {
                                return Ok(());
                            }
                            if let Some(ref manager_name) = manager_name {
                                let reason_text = decision.reasons.join("; ");
                                let msg = format!(
                                    "[{engineer}] Task #{task_id} passed tests but requires manual review.\nTitle: {task_title}\nConfidence: {confidence:.2}\nReasons: {reason_text}",
                                    confidence = decision.confidence
                                );
                                daemon.queue_message(engineer, manager_name, &msg)?;
                                daemon.mark_member_working(manager_name);
                            }
                            daemon.clear_active_task(engineer);
                            daemon.record_task_completed(engineer, Some(task_id));
                            daemon.set_member_idle(engineer);
                            return Ok(());
                        }
                    }
                }
                Err(ref error) => {
                    warn!(engineer, task_id, error = %error, "auto-merge diff analysis failed, falling through to normal merge");
                }
            }
        }

        let lock =
            MergeLock::acquire(daemon.project_root()).context("failed to acquire merge lock")?;

        match if daemon.is_multi_repo {
            merge_multi_repo_engineer_branch(
                daemon.project_root(),
                engineer,
                &daemon.sub_repo_names,
            )?
        } else {
            merge_engineer_branch(daemon.project_root(), engineer)?
        } {
            MergeOutcome::Success(success) => {
                drop(lock);
                if success.mode == MergeMode::IsolatedIntegration {
                    let reason = success
                        .reason
                        .as_deref()
                        .unwrap_or("root checkout required isolation");
                    daemon.record_orchestrator_action(format!(
                        "completion merge: isolated merge for task #{task_id} ({reason})"
                    ));
                }

                if let Err(error) = record_merge_test_timing(
                    daemon,
                    task_id,
                    engineer,
                    &task_branch,
                    test_duration_ms,
                ) {
                    warn!(
                        engineer,
                        task_id,
                        error = %error,
                        "failed to record merge test timing"
                    );
                }
                daemon.record_task_manual_merged(task_id, success.mode);

                let board_update_ok = daemon.run_kanban_md_nonfatal(
                    &[
                        "move",
                        &task_id.to_string(),
                        "done",
                        "--claim",
                        engineer,
                        "--dir",
                        &board_dir_str,
                    ],
                    &format!("move task #{task_id} to done"),
                    manager_name
                        .as_deref()
                        .into_iter()
                        .chain(std::iter::once(engineer)),
                );

                if let Some(ref manager_name) = manager_name {
                    let msg = format!(
                        "[{engineer}] Task #{task_id} completed.\nTitle: {task_title}\nTests: passed\nMerge: success{}{}{}",
                        if success.mode == MergeMode::IsolatedIntegration {
                            "\nMerge mode: isolated integration checkout"
                        } else {
                            ""
                        },
                        if let Some(reason) = success.reason.as_deref() {
                            format!("\nMerge reason: {reason}")
                        } else {
                            String::new()
                        },
                        if board_update_ok {
                            ""
                        } else {
                            "\nBoard: update failed; decide next board action manually."
                        }
                    );
                    daemon.queue_message(engineer, manager_name, &msg)?;
                    daemon.mark_member_working(manager_name);
                }

                if let Some(ref manager_name) = manager_name {
                    let rollup = format!(
                        "Rollup: Task #{task_id} completed by {engineer}. Tests passed, merged to main.{}",
                        if board_update_ok {
                            ""
                        } else {
                            " Board automation failed; decide manually."
                        }
                    );
                    daemon.notify_reports_to(manager_name, &rollup)?;
                }

                if let Ok(tasks) = load_tasks_from_dir(&board_dir.join("tasks")) {
                    if let Some(task) = tasks.into_iter().find(|task| task.id == task_id) {
                        let learning_summary = format!(
                            "Task #{task_id} merged cleanly after passing verification. Title: {}",
                            task.title
                        );
                        if let Err(error) = crate::team::learnings::append_task_completion_learning(
                            daemon.project_root(),
                            &task,
                            engineer,
                            &learning_summary,
                        ) {
                            warn!(
                                engineer,
                                task_id,
                                error = %error,
                                "failed to persist task learning"
                            );
                        }
                    }
                }

                daemon.clear_active_task(engineer);
                daemon.record_task_completed(engineer, Some(task_id));
                daemon.set_member_idle(engineer);
            }
            MergeOutcome::RebaseConflict(conflict_info) => {
                drop(lock);

                let attempt = daemon.increment_retry(engineer);
                if attempt <= 2 {
                    let msg = format!(
                        "Merge conflict during rebase onto main (attempt {attempt}/2). Fix the conflicts in your worktree and try again:\n{conflict_info}"
                    );
                    daemon.queue_message("batty", engineer, &msg)?;
                    daemon.mark_member_working(engineer);
                    info!(engineer, attempt, "rebase conflict, sending back for retry");
                } else {
                    if let Some(ref manager_name) = manager_name {
                        let msg = format!(
                            "[{engineer}] task #{task_id} has unresolvable merge conflicts after 2 retries. Escalating.\n{conflict_info}"
                        );
                        daemon.queue_message(engineer, manager_name, &msg)?;
                        daemon.mark_member_working(manager_name);
                    }

                    daemon.record_task_escalated(
                        engineer,
                        task_id.to_string(),
                        Some("merge_conflict"),
                    );

                    if let Some(ref manager_name) = manager_name {
                        let escalation = format!(
                            "ESCALATION: Task #{task_id} assigned to {engineer} has unresolvable merge conflicts. Task blocked on board."
                        );
                        daemon.notify_reports_to(manager_name, &escalation)?;
                    }

                    daemon.run_kanban_md_nonfatal(
                        &[
                            "edit",
                            &task_id.to_string(),
                            "--block",
                            "merge conflicts after 2 retries",
                            "--dir",
                            &board_dir_str,
                        ],
                        &format!("block task #{task_id} after merge conflict retries"),
                        manager_name
                            .as_deref()
                            .into_iter()
                            .chain(std::iter::once(engineer)),
                    );

                    daemon.clear_active_task(engineer);
                    daemon.set_member_idle(engineer);
                }
            }
            MergeOutcome::MergeFailure(merge_info) => {
                drop(lock);
                daemon.record_task_merge_failed(
                    engineer,
                    task_id,
                    infer_merge_mode_from_failure(&merge_info),
                    &merge_info,
                );

                let manager_notice = format!(
                    "Task #{task_id} from {engineer} passed tests but could not be merged to main.\n{merge_info}\nDecide whether to retry the isolated merge path, inspect the integration-worktree failure, or redirect the engineer."
                );
                if let Some(ref manager_name) = manager_name {
                    daemon.queue_message("daemon", manager_name, &manager_notice)?;
                    daemon.mark_member_working(manager_name);
                    daemon.notify_reports_to(manager_name, &manager_notice)?;
                }

                let engineer_notice = format!(
                    "Your task passed tests, but Batty could not merge it into main.\n{merge_info}\nWait for lead direction before making more changes."
                );
                daemon.queue_message("daemon", engineer, &engineer_notice)?;

                daemon.record_task_escalated(engineer, task_id.to_string(), Some("merge_failure"));
                daemon.clear_active_task(engineer);
                daemon.set_member_idle(engineer);
                warn!(
                    engineer,
                    task_id,
                    error = %merge_info,
                    "merge into main failed after passing tests; escalated without exiting daemon"
                );
            }
        }
        return Ok(());
    }

    let attempt = daemon.increment_retry(engineer);
    if attempt <= 2 {
        let failure_summary = test_run.results.failure_summary();
        let msg = format!(
            "Tests failed (attempt {attempt}/2). {failure_summary}\nFix the failures and try again.\nLast output:\n{}",
            test_run.output
        );
        daemon.queue_message("batty", engineer, &msg)?;
        daemon.mark_member_working(engineer);
        info!(engineer, attempt, "test failure, sending back for retry");
        return Ok(());
    }

    let escalation_key = format!("tests_failed_{task_id}_{engineer}");
    let suppress_duplicate_escalation =
        daemon.suppress_recent_escalation(escalation_key, Duration::from_secs(600));

    if let Some(ref manager_name) = manager_name
        && !suppress_duplicate_escalation
    {
        let failure_summary = test_run.results.failure_summary();
        let msg = format!(
            "[{engineer}] task #{task_id} failed tests after 2 retries. Escalating.\nSummary: {failure_summary}\nLast output:\n{}",
            test_run.output
        );
        daemon.queue_message(engineer, manager_name, &msg)?;
        daemon.mark_member_working(manager_name);
    }

    daemon.record_task_escalated(engineer, task_id.to_string(), Some("tests_failed"));

    if let Some(ref manager_name) = manager_name
        && !suppress_duplicate_escalation
    {
        let escalation = format!(
            "ESCALATION: Task #{task_id} assigned to {engineer} failed tests after 2 retries. Task blocked on board."
        );
        daemon.notify_reports_to(manager_name, &escalation)?;
    }

    daemon.run_kanban_md_nonfatal(
        &[
            "edit",
            &task_id.to_string(),
            "--block",
            "tests failed after 2 retries",
            "--dir",
            &board_dir_str,
        ],
        &format!("block task #{task_id} after max test retries"),
        manager_name
            .as_deref()
            .into_iter()
            .chain(std::iter::once(engineer)),
    );

    daemon.clear_active_task(engineer);
    daemon.set_member_idle(engineer);
    info!(engineer, task_id, "escalated to manager after max retries");
    Ok(())
}

fn load_previous_test_results(board_dir: &Path, task_id: u32) -> Result<Option<TestResults>> {
    let task_path = crate::team::task_cmd::find_task_path(board_dir, task_id)?;
    let metadata = crate::team::board::read_workflow_metadata(&task_path)?;
    Ok(metadata.test_results)
}

fn path_exists_on_main(worktree_dir: &Path, file: &str) -> Result<bool> {
    let output = std::process::Command::new("git")
        .args(["ls-tree", "-r", "--name-only", "main", "--", file])
        .current_dir(worktree_dir)
        .output()
        .with_context(|| {
            format!(
                "failed to inspect whether {file} exists on main in {}",
                worktree_dir.display()
            )
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "failed to inspect whether {file} exists on main in {}: {}",
            worktree_dir.display(),
            stderr.trim()
        );
    }
    Ok(!String::from_utf8_lossy(&output.stdout).trim().is_empty())
}

fn revert_out_of_scope_files(worktree_dir: &Path, out_of_scope: &[String]) -> Result<bool> {
    if out_of_scope.is_empty() {
        return Ok(false);
    }

    for file in out_of_scope {
        if path_exists_on_main(worktree_dir, file)? {
            run_git_with_context(
                worktree_dir,
                &["checkout", "main", "--", file],
                "failed to revert out-of-scope file",
            )?;
        } else {
            run_git_with_context(
                worktree_dir,
                &["rm", "-f", "--ignore-unmatch", "--", file],
                "failed to remove out-of-scope file added outside the fence",
            )?;
        }
    }

    run_git_with_context(worktree_dir, &["add", "-A"], "failed to stage scope revert")?;
    let staged = std::process::Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .current_dir(worktree_dir)
        .status()
        .with_context(|| {
            format!(
                "failed to inspect staged diff in {}",
                worktree_dir.display()
            )
        })?;
    if staged.success() {
        return Ok(false);
    }

    run_git_with_context(
        worktree_dir,
        &[
            "commit",
            "-m",
            "Restore fenced files after automatic scope recovery",
            "-m",
            "Constraint: Scope fence violations must be reverted automatically\nConfidence: medium\nScope-risk: narrow\nDirective: This daemon-owned recovery commit keeps the branch clean after out-of-scope writes\nTested: scope fence auto-revert path\nNot-tested: multi-repo scope recovery",
        ],
        "failed to commit scope revert",
    )?;
    Ok(true)
}

fn detect_flaky_failures(
    previous_results: Option<&TestResults>,
    current_results: &TestResults,
) -> Vec<crate::team::test_results::TestFailure> {
    let Some(previous) = previous_results else {
        return Vec::new();
    };
    if previous.failed == 0 || current_results.failed != 0 {
        return Vec::new();
    }
    previous.failures.clone()
}

/// Merge engineer branches across all sub-repos in a multi-repo project.
pub(crate) fn merge_multi_repo_engineer_branch(
    project_root: &Path,
    engineer_name: &str,
    sub_repo_names: &[String],
) -> Result<MergeOutcome> {
    for repo_name in sub_repo_names {
        let repo_root = project_root.join(repo_name);
        let sub_wt = project_root
            .join(".batty")
            .join("worktrees")
            .join(engineer_name)
            .join(repo_name);
        if !sub_wt.exists() {
            continue;
        }
        // Check if there are commits to merge in this sub-repo
        if commits_ahead_of_main(&sub_wt).unwrap_or(0) == 0 {
            continue;
        }
        match merge_engineer_branch_in_repo(&repo_root, &sub_wt, engineer_name)? {
            MergeOutcome::Success(_) => {}
            other => return Ok(other),
        }
    }

    // Reset all sub-repo worktrees after successful merge
    for repo_name in sub_repo_names {
        let repo_root = project_root.join(repo_name);
        if let Err(e) = reset_engineer_worktree_in_repo(&repo_root, engineer_name, repo_name) {
            warn!(
                engineer = engineer_name,
                repo = repo_name,
                error = %e,
                "worktree reset failed after multi-repo merge"
            );
        }
    }

    Ok(MergeOutcome::Success(MergeSuccess {
        mode: MergeMode::DirectRoot,
        reason: None,
    }))
}

/// Merge a single sub-repo's engineer worktree branch into main.
fn merge_engineer_branch_in_repo(
    repo_root: &Path,
    worktree_dir: &Path,
    engineer_name: &str,
) -> Result<MergeOutcome> {
    let branch = current_worktree_branch(worktree_dir)?;
    info!(engineer = engineer_name, branch = %branch, repo = %repo_root.display(), "merging sub-repo worktree branch");

    let main_branch = current_worktree_branch(repo_root)?;
    if main_branch != "main" {
        let checkout = run_git_with_context(
            repo_root,
            &["checkout", "main"],
            "checkout main in sub-repo before merge",
        )?;
        if !checkout.status.success() {
            let stderr = String::from_utf8_lossy(&checkout.stderr).trim().to_string();
            return Ok(MergeOutcome::MergeFailure(format!(
                "sub-repo {} on '{main_branch}', checkout main failed: {stderr}",
                repo_root.display()
            )));
        }
    }

    let rebase = run_git_with_context(
        worktree_dir,
        &["rebase", "main"],
        &format!("rebase '{branch}' onto main in {}", repo_root.display()),
    )?;
    if !rebase.status.success() {
        let stderr = String::from_utf8_lossy(&rebase.stderr).trim().to_string();
        let _ = run_git_with_context(worktree_dir, &["rebase", "--abort"], "abort rebase");
        return Ok(MergeOutcome::RebaseConflict(format!(
            "rebase conflict in {}: {stderr}",
            repo_root.display()
        )));
    }

    let output = run_git_with_context(
        repo_root,
        &["merge", &branch, "--no-edit"],
        &format!("merge '{branch}' into main in {}", repo_root.display()),
    )?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Ok(MergeOutcome::MergeFailure(format!(
            "merge failed in {}: {stderr}",
            repo_root.display()
        )));
    }

    println!(
        "Merged branch '{branch}' from {engineer_name} in {}",
        repo_root.file_name().unwrap_or_default().to_string_lossy()
    );
    Ok(MergeOutcome::Success(MergeSuccess {
        mode: MergeMode::DirectRoot,
        reason: None,
    }))
}

fn reset_engineer_worktree_in_repo(
    repo_root: &Path,
    engineer_name: &str,
    repo_name: &str,
) -> Result<()> {
    let worktree_dir = repo_root
        .parent()
        .unwrap_or(repo_root)
        .join(".batty")
        .join("worktrees")
        .join(engineer_name)
        .join(repo_name);
    if !worktree_dir.exists() {
        return Ok(());
    }
    let base_branch = engineer_base_branch_name(engineer_name);
    checkout_worktree_branch_from_main(&worktree_dir, &base_branch)?;
    Ok(())
}

pub(crate) fn record_merge_test_timing(
    daemon: &mut TeamDaemon,
    task_id: u32,
    engineer: &str,
    task_branch: &str,
    test_duration_ms: u64,
) -> Result<()> {
    let log_path = daemon
        .project_root()
        .join(".batty")
        .join("test_timing.jsonl");
    let record = append_test_timing_record(
        &log_path,
        task_id,
        engineer,
        task_branch,
        now_unix(),
        test_duration_ms,
    )?;

    if record.regression_detected {
        let rolling_average_ms = record.rolling_average_ms.unwrap_or_default();
        let regression_pct = record.regression_pct.unwrap_or_default();
        let reason = format!(
            "runtime_ms={} avg_ms={} pct={}",
            record.duration_ms, rolling_average_ms, regression_pct
        );
        daemon.record_performance_regression(task_id.to_string(), &reason);
        warn!(
            engineer,
            task_id,
            runtime_ms = record.duration_ms,
            rolling_average_ms,
            regression_pct,
            "post-merge test runtime exceeded rolling average"
        );
    }

    Ok(())
}

/// For multi-repo: sum commits ahead of main across all sub-repo worktrees.
fn multi_repo_commits_ahead_of_main(worktree_dir: &Path, sub_repo_names: &[String]) -> Result<u32> {
    let mut total = 0u32;
    for name in sub_repo_names {
        let sub_wt = worktree_dir.join(name);
        if sub_wt.exists() {
            total += commits_ahead_of_main(&sub_wt).unwrap_or(0);
        }
    }
    Ok(total)
}

/// For multi-repo: get the task branch from the first sub-repo that has commits.
fn multi_repo_task_branch(worktree_dir: &Path, sub_repo_names: &[String]) -> Result<String> {
    for name in sub_repo_names {
        let sub_wt = worktree_dir.join(name);
        if sub_wt.exists() && commits_ahead_of_main(&sub_wt).unwrap_or(0) > 0 {
            return current_worktree_branch(&sub_wt);
        }
    }
    // Fall back to first existing sub-repo
    for name in sub_repo_names {
        let sub_wt = worktree_dir.join(name);
        if sub_wt.exists() {
            return current_worktree_branch(&sub_wt);
        }
    }
    bail!("no sub-repo worktrees found in {}", worktree_dir.display())
}

/// For multi-repo: sum files changed from main across all sub-repo worktrees.
fn multi_repo_files_changed_from_main(
    worktree_dir: &Path,
    sub_repo_names: &[String],
) -> Result<u32> {
    let mut total = 0u32;
    for name in sub_repo_names {
        let sub_wt = worktree_dir.join(name);
        if sub_wt.exists() {
            total += files_changed_from_main(&sub_wt).unwrap_or(0);
        }
    }
    Ok(total)
}

fn multi_repo_code_files_changed_from_main(
    worktree_dir: &Path,
    sub_repo_names: &[String],
) -> Result<u32> {
    let mut total = 0u32;
    for name in sub_repo_names {
        let sub_wt = worktree_dir.join(name);
        if sub_wt.exists() {
            total += code_files_changed_from_main(&sub_wt).unwrap_or(0);
        }
    }
    Ok(total)
}

fn multi_repo_diff_stat_from_main(
    worktree_dir: &Path,
    sub_repo_names: &[String],
) -> Result<String> {
    let mut stats = Vec::new();
    for name in sub_repo_names {
        let sub_wt = worktree_dir.join(name);
        if !sub_wt.exists() {
            continue;
        }
        let diff_stat = diff_stat_from_main(&sub_wt).unwrap_or_default();
        if !diff_stat.is_empty() {
            stats.push(format!("[{name}]\n{diff_stat}"));
        }
    }
    Ok(stats.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::config::AutoMergePolicy;
    use crate::team::events::read_events;
    use crate::team::hierarchy::MemberInstance;
    use crate::team::inbox;
    use crate::team::standup::MemberState;
    use crate::team::task_loop::setup_engineer_worktree;
    use crate::team::test_helpers::make_test_daemon;
    use crate::team::test_support::{engineer_member, git_ok, init_git_repo, manager_member};
    use std::path::{Path, PathBuf};

    fn verification_snapshot_path(
        repo: &Path,
        task_id: u32,
        engineer: &str,
        attempt: u32,
    ) -> PathBuf {
        repo.join(".batty")
            .join("reports")
            .join("verification")
            .join("completion")
            .join(format!(
                "task-{task_id:03}-{engineer}-attempt-{attempt}.json"
            ))
    }

    fn write_task_file(project_root: &Path, id: u32, title: &str) {
        write_task_file_with_body(project_root, id, title, "Task description.\n");
    }

    fn write_scope_ack_team_config(project_root: &Path) {
        let team_config_dir = project_root.join(".batty").join("team_config");
        std::fs::create_dir_all(&team_config_dir).unwrap();
        std::fs::write(
            team_config_dir.join("team.yaml"),
            "name: scope-test\nroles:\n  - name: architect\n    role_type: architect\n    agent: claude\n    instances: 1\n  - name: manager\n    role_type: manager\n    agent: claude\n    instances: 1\n  - name: engineer\n    role_type: engineer\n    agent: codex\n    instances: 1\n",
        )
        .unwrap();
    }

    fn write_task_file_with_body(project_root: &Path, id: u32, title: &str, body: &str) {
        let tasks_dir = project_root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join(format!("{id:03}-{title}.md")),
            format!(
                "---\nid: {id}\ntitle: {title}\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\nclass: standard\n---\n\n{body}"
            ),
        )
        .unwrap();
    }

    fn engineer_worktree_paths(repo: &Path, engineer: &str) -> (PathBuf, PathBuf) {
        let worktree_dir = repo.join(".batty").join("worktrees").join(engineer);
        let team_config_dir = repo.join(".batty").join("team_config");
        (worktree_dir, team_config_dir)
    }

    fn setup_completion_daemon(repo: &Path, engineer: &str) -> crate::team::daemon::TeamDaemon {
        let members = vec![
            manager_member("manager", None),
            engineer_member(engineer, Some("manager"), true),
        ];
        make_test_daemon(repo, members)
    }

    fn current_head(repo: &Path) -> String {
        String::from_utf8(
            std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(repo)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string()
    }

    fn seed_completion_packet_metadata(
        repo: &Path,
        task_id: u32,
        title: &str,
        worktree_dir: &Path,
    ) {
        let branch = current_worktree_branch(worktree_dir).unwrap();
        let commit = current_head(worktree_dir);
        let task_path = repo
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks")
            .join(format!("{task_id:03}-{title}.md"));
        write_workflow_metadata(
            &task_path,
            &WorkflowMetadata {
                branch: Some(branch),
                worktree_path: Some(worktree_dir.to_string_lossy().into_owned()),
                commit: Some(commit),
                changed_paths: vec!["note.txt".to_string()],
                tests_run: Some(true),
                tests_passed: Some(true),
                test_results: None,
                artifacts: Vec::new(),
                outcome: Some("ready_for_review".to_string()),
                review_blockers: Vec::new(),
            },
        )
        .unwrap();
    }

    fn setup_rebase_conflict_repo(
        engineer: &str,
    ) -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, engineer);

        std::fs::write(repo.join("conflict.txt"), "original\n").unwrap();
        git_ok(&repo, &["add", "conflict.txt"]);
        git_ok(&repo, &["commit", "-m", "add conflict file"]);

        setup_engineer_worktree(&repo, &worktree_dir, engineer, &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("conflict.txt"), "engineer version\n").unwrap();
        git_ok(&worktree_dir, &["add", "conflict.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer change"]);

        std::fs::write(repo.join("conflict.txt"), "main version\n").unwrap();
        git_ok(&repo, &["add", "conflict.txt"]);
        git_ok(&repo, &["commit", "-m", "main change"]);

        (tmp, repo, worktree_dir, team_config_dir)
    }

    fn setup_auto_merge_repo(engineer: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-auto-merge-test");
        write_task_file(&repo, 42, "auto-merge-task");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join(engineer);
        setup_engineer_worktree(&repo, &worktree_dir, engineer, &team_config_dir).unwrap();

        // Create a small change in the worktree
        std::fs::write(worktree_dir.join("note.txt"), "done\n").unwrap();
        git_ok(&worktree_dir, &["add", "note.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "add note"]);
        seed_completion_packet_metadata(&repo, 42, "auto-merge-task", &worktree_dir);

        (tmp, repo, worktree_dir)
    }

    fn setup_failing_test_repo(engineer: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-failing-test");
        write_task_file(&repo, 42, "failing-test-task");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join(engineer);
        setup_engineer_worktree(&repo, &worktree_dir, engineer, &team_config_dir).unwrap();

        std::fs::write(
            worktree_dir.join("src").join("lib.rs"),
            "#[cfg(test)]\nmod tests {\n    #[test]\n    fn test_dispatch_wip_guard() {\n        assert_eq!(2, 3);\n    }\n}\n",
        )
        .unwrap();
        git_ok(&worktree_dir, &["add", "src/lib.rs"]);
        git_ok(&worktree_dir, &["commit", "-m", "add failing test"]);

        (tmp, repo, worktree_dir)
    }

    fn auto_merge_daemon(repo: &Path, policy: AutoMergePolicy) -> crate::team::daemon::TeamDaemon {
        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ];
        let mut daemon = make_test_daemon(repo, members);
        daemon.config.team_config.workflow_policy.auto_merge = policy;
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);
        daemon
    }

    fn complete_task_and_process_queue(
        daemon: &mut crate::team::daemon::TeamDaemon,
        engineer: &str,
    ) {
        handle_engineer_completion(daemon, engineer).unwrap();
        daemon.process_merge_queue_for_test().unwrap();
    }

    #[test]
    fn completion_routes_engineers_with_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng-1".to_string(),
            role_type: crate::team::config::RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
            ..Default::default()
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);

        daemon.set_active_task_for_test("eng-1", 42);
        complete_task_and_process_queue(&mut daemon, "eng-1");
        assert_eq!(daemon.active_task_id("eng-1"), Some(42));
    }

    #[test]
    fn completion_gate_rejects_zero_commits_but_keeps_task_active() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        write_task_file(&repo, 42, "zero-commit-task");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
        std::fs::remove_file(worktree_dir.join("Cargo.toml")).unwrap();

        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng-1".to_string(),
            role_type: crate::team::config::RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: true,
            ..Default::default()
        };
        let mut daemon = make_test_daemon(&repo, vec![engineer]);

        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);
        complete_task_and_process_queue(&mut daemon, "eng-1");

        // Task stays active — engineer still owns it (false-done prevention)
        assert_eq!(daemon.active_task_id("eng-1"), Some(42));
        assert_eq!(daemon.retry_count_for_test("eng-1"), None);
    }

    #[test]
    fn completion_gate_passes_with_commits() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        write_task_file(&repo, 42, "commit-gate-success");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("note.txt"), "done\n").unwrap();
        git_ok(&worktree_dir, &["add", "note.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "add note"]);
        seed_completion_packet_metadata(&repo, 42, "commit-gate-success", &worktree_dir);

        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng-1".to_string(),
            role_type: crate::team::config::RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: true,
            ..Default::default()
        };
        let mut daemon = make_test_daemon(&repo, vec![engineer]);

        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);

        complete_task_and_process_queue(&mut daemon, "eng-1");

        assert_eq!(daemon.active_task_id("eng-1"), None);
        assert_eq!(
            daemon.member_state_for_test("eng-1"),
            Some(MemberState::Idle)
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("note.txt")).unwrap(),
            "done\n"
        );

        let timing_log = repo.join(".batty").join("test_timing.jsonl");
        let timings = read_test_timing_log(&timing_log).unwrap();
        assert_eq!(timings.len(), 1);
        assert_eq!(timings[0].task_id, 42);
        assert_eq!(timings[0].engineer, "eng-1");
        assert_eq!(timings[0].branch, "eng-1");
        assert!(!timings[0].regression_detected);

        let task_path = repo
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks")
            .join("042-commit-gate-success.md");
        let metadata = crate::team::board::read_workflow_metadata(&task_path).unwrap();
        assert_eq!(metadata.tests_run, Some(true));
        assert_eq!(metadata.tests_passed, Some(true));
        assert_eq!(metadata.outcome.as_deref(), Some("verification_passed"));
        let snapshot_path = verification_snapshot_path(&repo, 42, "eng-1", 1);
        assert!(
            metadata
                .artifacts
                .iter()
                .any(|artifact| artifact.ends_with("task-042-eng-1-attempt-1.json"))
        );
        let snapshot = std::fs::read_to_string(snapshot_path).unwrap();
        assert!(snapshot.contains("\"phase\": \"complete\""));
        assert!(snapshot.contains("\"passed\": true"));
    }

    #[test]
    fn completion_uses_verification_test_command_override() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        write_task_file(&repo, 42, "verification-test-command");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        std::fs::write(
            worktree_dir.join("check.sh"),
            "#!/bin/sh\necho verification override\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(worktree_dir.join("check.sh"))
                .unwrap()
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(worktree_dir.join("check.sh"), perms).unwrap();
        }

        std::fs::write(worktree_dir.join("note.txt"), "done\n").unwrap();
        git_ok(&worktree_dir, &["add", "check.sh", "note.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "add override command"]);
        seed_completion_packet_metadata(&repo, 42, "verification-test-command", &worktree_dir);

        let mut daemon = setup_completion_daemon(&repo, "eng-1");
        daemon.config.team_config.workflow_policy.test_command = Some("false".to_string());
        daemon
            .config
            .team_config
            .workflow_policy
            .verification
            .test_command = Some("./check.sh".to_string());
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);

        complete_task_and_process_queue(&mut daemon, "eng-1");

        assert_eq!(daemon.active_task_id("eng-1"), None);
        assert_eq!(
            std::fs::read_to_string(repo.join("note.txt")).unwrap(),
            "done\n"
        );
        let snapshot =
            std::fs::read_to_string(verification_snapshot_path(&repo, 42, "eng-1", 1)).unwrap();
        assert!(snapshot.contains("\"phase\": \"complete\""));
        assert!(snapshot.contains("\"passed\": true"));
        assert!(!snapshot.contains("test command failed without a parsed failure line"));
    }

    #[test]
    fn completion_skips_automatic_verification_when_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        write_task_file(&repo, 42, "verification-skipped");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        std::fs::write(
            worktree_dir.join("src").join("lib.rs"),
            "pub fn smoke() -> bool { false }\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn smoke_test() {\n        assert!(smoke());\n    }\n}\n",
        )
        .unwrap();
        git_ok(&worktree_dir, &["add", "src/lib.rs"]);
        git_ok(&worktree_dir, &["commit", "-m", "introduce failing tests"]);
        seed_completion_packet_metadata(&repo, 42, "verification-skipped", &worktree_dir);

        let mut daemon = setup_completion_daemon(&repo, "eng-1");
        daemon
            .config
            .team_config
            .workflow_policy
            .verification
            .auto_run_tests = false;
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);

        complete_task_and_process_queue(&mut daemon, "eng-1");

        assert_eq!(daemon.active_task_id("eng-1"), None);
        assert_eq!(
            daemon.member_state_for_test("eng-1"),
            Some(MemberState::Idle)
        );

        let events =
            read_events(&repo.join(".batty").join("team_config").join("events.jsonl")).unwrap();
        assert!(events.iter().any(|event| {
            event.event == "verification_evidence_collected"
                && event.reason.as_deref() == Some("automatic verification skipped by policy")
        }));
    }

    #[test]
    fn failing_verification_transitions_to_fixing_with_context() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        write_task_file(&repo, 42, "verification-fixing");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        std::fs::write(
            worktree_dir.join("src").join("lib.rs"),
            "pub fn smoke() -> bool { false }\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn smoke_test() {\n        assert!(smoke());\n    }\n}\n",
        )
        .unwrap();
        git_ok(&worktree_dir, &["add", "src/lib.rs"]);
        git_ok(&worktree_dir, &["commit", "-m", "introduce failing tests"]);

        let mut daemon = setup_completion_daemon(&repo, "eng-1");
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        assert_eq!(daemon.active_task_id("eng-1"), Some(42));
        assert_eq!(
            daemon.member_state_for_test("eng-1"),
            Some(MemberState::Working)
        );

        let verification_state = daemon.verification_states.get("eng-1").unwrap();
        assert_eq!(verification_state.phase, VerificationPhase::Fixing);
        assert_eq!(verification_state.iteration, 1);
        assert!(!verification_state.last_test_passed);
        assert!(
            verification_state
                .last_test_output
                .as_deref()
                .is_some_and(|output| output.contains("smoke_test"))
        );

        let engineer_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "eng-1").unwrap();
        assert_eq!(engineer_messages.len(), 1);
        assert!(engineer_messages[0].body.contains("Fix attempt 1/3"));
        assert!(
            engineer_messages[0]
                .body
                .contains("Verification failed because the test command did not pass.")
        );
        assert!(
            engineer_messages[0]
                .body
                .contains("Latest verification output:")
        );

        let events =
            read_events(&repo.join(".batty").join("team_config").join("events.jsonl")).unwrap();
        assert!(events.iter().any(|event| {
            event.event == "verification_phase_changed"
                && event.step.as_deref() == Some("verifying")
                && event.reason.as_deref() == Some("from=executing to=verifying iteration=0")
        }));
        assert!(events.iter().any(|event| {
            event.event == "verification_phase_changed"
                && event.step.as_deref() == Some("fixing")
                && event.reason.as_deref() == Some("from=verifying to=fixing iteration=1")
        }));
        assert!(events.iter().any(|event| {
            event.event == "verification_evidence_collected"
                && event.step.as_deref() == Some("tests_failed")
        }));

        let task_path = repo
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks")
            .join("042-verification-fixing.md");
        let metadata = crate::team::board::read_workflow_metadata(&task_path).unwrap();
        assert_eq!(
            metadata.outcome.as_deref(),
            Some("verification_retry_required")
        );
        assert_eq!(metadata.tests_passed, Some(false));
        let snapshot_path = verification_snapshot_path(&repo, 42, "eng-1", 1);
        let snapshot = std::fs::read_to_string(snapshot_path).unwrap();
        assert!(snapshot.contains("\"phase\": \"fixing\""));
        assert!(snapshot.contains("smoke_test"));
    }

    #[test]
    fn verification_max_iterations_blocks_task_and_escalates_manager() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        write_task_file(&repo, 42, "verification-failed");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        std::fs::write(
            worktree_dir.join("src").join("lib.rs"),
            "pub fn smoke() -> bool { false }\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn smoke_test() {\n        assert!(smoke());\n    }\n}\n",
        )
        .unwrap();
        git_ok(&worktree_dir, &["add", "src/lib.rs"]);
        git_ok(&worktree_dir, &["commit", "-m", "introduce failing tests"]);

        let mut daemon = setup_completion_daemon(&repo, "eng-1");
        daemon
            .config
            .team_config
            .workflow_policy
            .verification
            .max_iterations = 1;
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        assert_eq!(daemon.active_task_id("eng-1"), None);
        assert_eq!(
            daemon.member_state_for_test("eng-1"),
            Some(MemberState::Idle)
        );
        assert!(!daemon.verification_states.contains_key("eng-1"));

        let engineer_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "eng-1").unwrap();
        assert!(
            engineer_messages
                .iter()
                .any(|message| message.body.contains("Fix attempt 1/1"))
        );

        let manager_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
        assert!(manager_messages.iter().any(|message| {
            message
                .body
                .contains("hit verification max iterations on task #42")
        }));
        assert!(
            manager_messages
                .iter()
                .any(|message| message.body.contains("Latest phase: failed"))
        );

        let task_path = repo
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks")
            .join("042-verification-failed.md");
        let task = crate::task::Task::from_file(&task_path).unwrap();
        assert_eq!(task.status, "blocked");
        assert!(
            task.blocked_on
                .as_deref()
                .is_some_and(|reason| reason.contains("verification escalation after 1 attempts"))
        );

        let metadata = crate::team::board::read_workflow_metadata(&task_path).unwrap();
        assert_eq!(metadata.outcome.as_deref(), Some("verification_escalated"));
        let snapshot_path = verification_snapshot_path(&repo, 42, "eng-1", 1);
        let snapshot = std::fs::read_to_string(snapshot_path).unwrap();
        assert!(snapshot.contains("\"phase\": \"failed\""));
        assert!(snapshot.contains("\"passed\": false"));

        let events =
            read_events(&repo.join(".batty").join("team_config").join("events.jsonl")).unwrap();
        assert!(events.iter().any(|event| {
            event.event == "verification_phase_changed"
                && event.step.as_deref() == Some("failed")
                && event.reason.as_deref() == Some("from=verifying to=failed iteration=1")
        }));
        assert!(events.iter().any(|event| {
            event.event == "verification_max_iterations_reached"
                && event.task.as_deref() == Some("42")
                && event.role.as_deref() == Some("eng-1")
                && event.recipient.as_deref() == Some("manager")
        }));
        assert!(events.iter().any(|event| {
            event.event == "task_escalated"
                && event.task.as_deref() == Some("42")
                && event.reason.as_deref() == Some("verification_failed")
        }));
    }

    #[test]
    fn zero_commit_retry_message_sent() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        write_task_file(&repo, 42, "zero-commit-message");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
        std::fs::remove_file(worktree_dir.join("Cargo.toml")).unwrap();

        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: crate::team::config::RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng-1".to_string(),
            role_type: crate::team::config::RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: true,
            ..Default::default()
        };
        let mut daemon = make_test_daemon(&repo, vec![manager, engineer]);

        daemon.set_active_task_for_test("eng-1", 42);
        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        let engineer_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "eng-1").unwrap();
        assert_eq!(engineer_messages.len(), 1);
        assert_eq!(engineer_messages[0].from, "batty");
        assert!(
            engineer_messages[0]
                .body
                .contains("no commits ahead of main"),
            "rejection message should mention no commits: {:?}",
            engineer_messages[0].body
        );
        assert!(
            engineer_messages[0].body.contains("git add -A"),
            "rejection message should include commit instructions: {:?}",
            engineer_messages[0].body
        );

        // On first rejection, manager is NOT notified (only after 3 rejections)
        let manager_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
        assert!(manager_messages.is_empty());
    }

    #[test]
    fn scope_violation_completion_reverts_and_emits_event() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        write_scope_ack_team_config(&repo);
        std::fs::create_dir_all(repo.join("docs")).unwrap();
        std::fs::write(repo.join("docs").join("notes.md"), "base notes\n").unwrap();
        git_ok(&repo, &["add", "docs/notes.md"]);
        git_ok(&repo, &["commit", "-m", "add docs notes"]);

        write_task_file_with_body(
            &repo,
            42,
            "scope-violation",
            "Task description.\nSCOPE FENCE: src/lib.rs\n",
        );

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        let ack = inbox::InboxMessage::new_send("eng-1", "manager", "Scope ACK #42");
        inbox::deliver_to_inbox(&inbox::inboxes_root(&repo), &ack).unwrap();

        std::fs::write(
            worktree_dir.join("src").join("lib.rs"),
            "pub fn smoke() -> bool { true }\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn smoke_test() {\n        assert!(smoke());\n    }\n}\n",
        )
        .unwrap();
        std::fs::write(worktree_dir.join("docs").join("notes.md"), "out of scope\n").unwrap();
        git_ok(&worktree_dir, &["add", "src/lib.rs", "docs/notes.md"]);
        git_ok(&worktree_dir, &["commit", "-m", "mixed scope change"]);

        let mut daemon = setup_completion_daemon(&repo, "eng-1");
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        assert_eq!(
            std::fs::read_to_string(worktree_dir.join("docs").join("notes.md")).unwrap(),
            "base notes\n"
        );
        let revert_subject = String::from_utf8(
            std::process::Command::new("git")
                .args(["log", "-1", "--format=%s"])
                .current_dir(&worktree_dir)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        assert!(revert_subject.contains("Restore fenced files after automatic scope recovery"));

        let events =
            read_events(&repo.join(".batty").join("team_config").join("events.jsonl")).unwrap();
        assert!(events.iter().any(|event| {
            event.event == "scope_fence_violation"
                && event.task.as_deref() == Some("42")
                && event.role.as_deref() == Some("eng-1")
                && event
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("reclaim_requested=true"))
        }));
    }

    #[test]
    fn scope_violation_completion_removes_new_out_of_scope_files_not_on_main() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        write_scope_ack_team_config(&repo);
        write_task_file_with_body(
            &repo,
            42,
            "scope-violation-new-file",
            "Task description.\nSCOPE FENCE: src/lib.rs\n",
        );

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        let ack = inbox::InboxMessage::new_send("eng-1", "manager", "Scope ACK #42");
        inbox::deliver_to_inbox(&inbox::inboxes_root(&repo), &ack).unwrap();

        std::fs::write(
            worktree_dir.join("src").join("lib.rs"),
            "pub fn smoke() -> bool { true }\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn smoke_test() {\n        assert!(smoke());\n    }\n}\n",
        )
        .unwrap();
        std::fs::write(worktree_dir.join("stray.txt"), "out of scope\n").unwrap();
        git_ok(&worktree_dir, &["add", "src/lib.rs", "stray.txt"]);
        git_ok(
            &worktree_dir,
            &["commit", "-m", "mixed scope change with stray file"],
        );

        let mut daemon = setup_completion_daemon(&repo, "eng-1");
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        assert!(!worktree_dir.join("stray.txt").exists());
        let revert_subject = String::from_utf8(
            std::process::Command::new("git")
                .args(["log", "-1", "--format=%s"])
                .current_dir(&worktree_dir)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        assert!(revert_subject.contains("Restore fenced files after automatic scope recovery"));
    }

    #[test]
    fn narration_only_completion_retries_then_escalates_after_two_rejections() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        write_task_file(&repo, 42, "narration-only-task");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
        git_ok(
            &worktree_dir,
            &["commit", "--allow-empty", "-m", "commentary only"],
        );

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ];
        let mut daemon = make_test_daemon(&repo, members);
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        let engineer_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "eng-1").unwrap();
        assert_eq!(engineer_messages.len(), 1);
        assert!(engineer_messages[0]
            .body
            .contains("Your previous attempt only produced commentary. Execute the actual commands to make the code changes."));
        assert!(
            engineer_messages[0]
                .body
                .contains("git diff --stat main..HEAD")
        );

        let manager_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
        assert!(manager_messages.is_empty());
        assert_eq!(daemon.active_task_id("eng-1"), Some(42));

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        let engineer_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "eng-1").unwrap();
        assert_eq!(engineer_messages.len(), 2);
        assert!(engineer_messages[1]
            .body
            .contains("Your previous attempt only produced commentary. Execute the actual commands to make the code changes."));

        let manager_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
        assert_eq!(manager_messages.len(), 1);
        assert!(
            manager_messages[0]
                .body
                .contains("hit the narration-only quality gate 2 times")
        );
        assert!(
            manager_messages[0]
                .body
                .contains("completion diff is narration-only")
        );

        let events_path = repo.join(".batty").join("team_config").join("events.jsonl");
        let events = read_events(&events_path).unwrap();
        let narration_events: Vec<_> = events
            .iter()
            .filter(|event| event.event == "narration_rejection")
            .collect();
        assert_eq!(narration_events.len(), 2);
        assert_eq!(narration_events[0].task.as_deref(), Some("42"));
        assert_eq!(
            narration_events[1].reason.as_deref(),
            Some("rejection_count=2")
        );
    }

    #[test]
    fn narration_rejection_count_is_task_scoped() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        write_task_file(&repo, 42, "first-task");
        write_task_file(&repo, 43, "second-task");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
        git_ok(
            &worktree_dir,
            &["commit", "--allow-empty", "-m", "commentary only"],
        );

        let mut daemon = setup_completion_daemon(&repo, "eng-1");
        daemon.set_active_task_for_test("eng-1", 42);
        handle_engineer_completion(&mut daemon, "eng-1").unwrap();
        assert_eq!(daemon.narration_rejection_counts.get(&42), Some(&1));

        daemon.clear_active_task("eng-1");
        assert!(!daemon.narration_rejection_counts.contains_key(&42));

        daemon.set_active_task_for_test("eng-1", 43);
        handle_engineer_completion(&mut daemon, "eng-1").unwrap();
        assert_eq!(daemon.narration_rejection_counts.get(&43), Some(&1));
    }

    #[test]
    fn narration_only_completion_rejects_docs_only_diff() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        write_task_file(&repo, 42, "docs-only-task");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        std::fs::create_dir_all(worktree_dir.join("docs")).unwrap();
        std::fs::write(
            worktree_dir.join("docs").join("notes.md"),
            "commentary only\n",
        )
        .unwrap();
        git_ok(&worktree_dir, &["add", "docs/notes.md"]);
        git_ok(&worktree_dir, &["commit", "-m", "docs only"]);

        let mut daemon = setup_completion_daemon(&repo, "eng-1");
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        let engineer_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "eng-1").unwrap();
        assert_eq!(engineer_messages.len(), 1);
        assert!(engineer_messages[0].body.contains("found no code changes"));
        assert!(engineer_messages[0].body.contains("Code files changed: 0"));

        let events_path = repo.join(".batty").join("team_config").join("events.jsonl");
        let events = read_events(&events_path).unwrap();
        assert!(events.iter().any(|event| {
            event.event == "narration_rejection"
                && event.task.as_deref() == Some("42")
                && event.role.as_deref() == Some("eng-1")
        }));
    }

    #[test]
    fn no_commits_rejection_keeps_assignment() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        write_task_file(&repo, 42, "no-commits-keep");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
        std::fs::remove_file(worktree_dir.join("Cargo.toml")).unwrap();

        let mut daemon = setup_completion_daemon(&repo, "eng-1");

        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        // Assignment kept — engineer still owns the task (false-done prevention)
        assert_eq!(daemon.active_task_id("eng-1"), Some(42));
    }

    #[test]
    fn no_commits_rejection_does_not_retry_and_keeps_task() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        write_task_file(&repo, 42, "no-commits-no-retry");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
        std::fs::remove_file(worktree_dir.join("Cargo.toml")).unwrap();

        let mut daemon = setup_completion_daemon(&repo, "eng-1");

        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        // Retry count should not be incremented
        assert_eq!(daemon.retry_count_for_test("eng-1"), None);
        // Active task kept — engineer still owns it (false-done prevention)
        assert_eq!(daemon.active_task_id("eng-1"), Some(42));
    }

    #[test]
    fn rebase_conflict_first_retry_messages_engineer() {
        let (_tmp, repo, _worktree_dir, _team_config_dir) = setup_rebase_conflict_repo("eng-1");
        write_task_file(&repo, 42, "rebase-conflict-retry");

        let mut daemon = setup_completion_daemon(&repo, "eng-1");
        daemon.config.team_config.workflow_policy.auto_merge.enabled = false;
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        let engineer_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "eng-1").unwrap();
        assert_eq!(engineer_messages.len(), 1);
        assert_eq!(engineer_messages[0].from, "batty");
        assert!(
            engineer_messages[0]
                .body
                .contains("Merge conflict during rebase onto main")
        );
    }

    #[test]
    fn rebase_conflict_first_retry_keeps_task_active_and_counts_retry() {
        let (_tmp, repo, _worktree_dir, _team_config_dir) = setup_rebase_conflict_repo("eng-1");
        write_task_file(&repo, 42, "rebase-conflict-state");

        let mut daemon = setup_completion_daemon(&repo, "eng-1");
        daemon.config.team_config.workflow_policy.auto_merge.enabled = false;
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        assert_eq!(daemon.active_task_id("eng-1"), Some(42));
        assert_eq!(daemon.retry_count_for_test("eng-1"), Some(1));
        assert_eq!(
            daemon.member_state_for_test("eng-1"),
            Some(MemberState::Working)
        );
    }

    #[test]
    fn rebase_conflict_third_attempt_escalates_to_manager() {
        let (_tmp, repo, _worktree_dir, _team_config_dir) = setup_rebase_conflict_repo("eng-1");
        write_task_file(&repo, 42, "rebase-conflict-escalation");

        let mut daemon = setup_completion_daemon(&repo, "eng-1");
        daemon.config.team_config.workflow_policy.auto_merge.enabled = false;
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);
        daemon.increment_retry("eng-1");
        daemon.increment_retry("eng-1");

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        let manager_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
        assert!(manager_messages.iter().any(|msg| {
            msg.from == "eng-1"
                && msg
                    .body
                    .contains("unresolvable merge conflicts after 2 retries")
        }));
    }

    #[test]
    fn rebase_conflict_third_attempt_clears_task_and_sets_idle() {
        let (_tmp, repo, _worktree_dir, _team_config_dir) = setup_rebase_conflict_repo("eng-1");
        write_task_file(&repo, 42, "rebase-conflict-reset");

        let mut daemon = setup_completion_daemon(&repo, "eng-1");
        daemon.config.team_config.workflow_policy.auto_merge.enabled = false;
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);
        daemon.increment_retry("eng-1");
        daemon.increment_retry("eng-1");

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        assert_eq!(daemon.active_task_id("eng-1"), None);
        assert_eq!(daemon.retry_count_for_test("eng-1"), None);
        assert_eq!(
            daemon.member_state_for_test("eng-1"),
            Some(MemberState::Idle)
        );
    }

    #[test]
    fn rebase_conflict_third_attempt_records_escalation_event() {
        let (_tmp, repo, _worktree_dir, _team_config_dir) = setup_rebase_conflict_repo("eng-1");
        write_task_file(&repo, 42, "rebase-conflict-event");

        let mut daemon = setup_completion_daemon(&repo, "eng-1");
        daemon.config.team_config.workflow_policy.auto_merge.enabled = false;
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.increment_retry("eng-1");
        daemon.increment_retry("eng-1");

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        let events = crate::team::events::read_events(
            &repo.join(".batty").join("team_config").join("events.jsonl"),
        )
        .unwrap();
        assert!(events.iter().any(|event| {
            event.event == "task_escalated"
                && event.role.as_deref() == Some("eng-1")
                && event.task.as_deref() == Some("42")
        }));
    }

    #[test]
    fn handle_engineer_completion_escalates_isolated_merge_prep_failures_without_crashing() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        write_task_file(&repo, 42, "merge-blocked-task");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("feature.rs"), "fn engineer() {}\n").unwrap();
        git_ok(&worktree_dir, &["add", "feature.rs"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer update"]);
        seed_completion_packet_metadata(&repo, 42, "merge-blocked-task", &worktree_dir);

        std::fs::write(repo.join("journal.rs"), "fn dirty_main() {}\n").unwrap();
        std::fs::create_dir_all(repo.join(".batty")).unwrap();
        std::fs::write(
            repo.join(".batty").join("integration-worktrees"),
            "not a directory\n",
        )
        .unwrap();

        let members = vec![
            MemberInstance {
                name: "manager".to_string(),
                role_name: "manager".to_string(),
                role_type: crate::team::config::RoleType::Manager,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: None,
                use_worktrees: false,
                ..Default::default()
            },
            MemberInstance {
                name: "eng-1".to_string(),
                role_name: "eng-1".to_string(),
                role_type: crate::team::config::RoleType::Engineer,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: Some("manager".to_string()),
                use_worktrees: true,
                ..Default::default()
            },
        ];

        let mut daemon = make_test_daemon(&repo, members);
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);

        complete_task_and_process_queue(&mut daemon, "eng-1");

        assert_eq!(daemon.active_task_id("eng-1"), None);
        assert_eq!(
            daemon.member_state_for_test("eng-1"),
            Some(MemberState::Idle)
        );

        let manager_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
        assert_eq!(manager_messages.len(), 1);
        assert_eq!(manager_messages[0].from, "daemon");
        assert!(
            manager_messages[0]
                .body
                .contains("could not be merged to main")
        );
        assert!(
            manager_messages[0]
                .body
                .contains("isolated merge path failed")
        );

        let engineer_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "eng-1").unwrap();
        assert_eq!(engineer_messages.len(), 1);
        assert_eq!(engineer_messages[0].from, "daemon");
        assert!(
            engineer_messages[0]
                .body
                .contains("could not merge it into main")
        );
    }

    #[test]
    fn handle_engineer_completion_emits_performance_regression_event() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        write_task_file(&repo, 42, "runtime-regression-task");

        let timing_log = repo.join(".batty").join("test_timing.jsonl");
        for task_id in 1..=5 {
            crate::team::artifact::record_test_timing(
                &timing_log,
                &crate::team::artifact::TestTimingRecord {
                    task_id,
                    engineer: "eng-1".to_string(),
                    branch: format!("eng-1/task-{task_id}"),
                    measured_at: 1_777_000_000 + task_id as u64,
                    duration_ms: 1,
                    rolling_average_ms: Some(1),
                    regression_pct: Some(0),
                    regression_detected: false,
                },
            )
            .unwrap();
        }

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("note.txt"), "done\n").unwrap();
        git_ok(&worktree_dir, &["add", "note.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "add note"]);
        seed_completion_packet_metadata(&repo, 42, "runtime-regression-task", &worktree_dir);

        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng-1".to_string(),
            role_type: crate::team::config::RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: true,
            ..Default::default()
        };
        let mut daemon = make_test_daemon(&repo, vec![engineer]);
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);

        complete_task_and_process_queue(&mut daemon, "eng-1");

        let events = crate::team::events::read_events(
            &repo.join(".batty").join("team_config").join("events.jsonl"),
        )
        .unwrap();
        assert!(events.iter().any(|event| {
            event.event == "performance_regression"
                && event.task.as_deref() == Some("42")
                && event
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("runtime_ms="))
        }));

        let timings = read_test_timing_log(&timing_log).unwrap();
        assert_eq!(timings.len(), 6);
        assert!(timings.last().unwrap().regression_detected);
    }

    #[test]
    fn test_failure_retry_message_includes_structured_summary() {
        let (_tmp, repo, _worktree_dir) = setup_failing_test_repo("eng-1");
        let mut daemon = setup_completion_daemon(&repo, "eng-1");
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        let engineer_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "eng-1").unwrap();
        assert_eq!(engineer_messages.len(), 1);
        assert!(
            engineer_messages[0]
                .body
                .contains("1 tests failed: tests::test_dispatch_wip_guard")
        );
        assert!(engineer_messages[0].body.contains("src/lib.rs"));

        let task_path = repo
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks")
            .join("042-failing-test-task.md");
        let metadata = crate::team::board::read_workflow_metadata(&task_path).unwrap();
        assert_eq!(metadata.tests_run, Some(true));
        assert_eq!(metadata.tests_passed, Some(false));
        assert_eq!(
            metadata.outcome.as_deref(),
            Some("verification_retry_required")
        );
        assert_eq!(
            metadata.test_results.as_ref().map(|results| results.failed),
            Some(1)
        );
        assert!(
            metadata
                .artifacts
                .iter()
                .any(|artifact| artifact.ends_with("task-042-eng-1-attempt-1.json"))
        );
    }

    #[test]
    fn test_failure_escalation_includes_structured_summary() {
        let (_tmp, repo, _worktree_dir) = setup_failing_test_repo("eng-1");
        let mut daemon = setup_completion_daemon(&repo, "eng-1");
        daemon
            .config
            .team_config
            .workflow_policy
            .verification
            .max_iterations = 1;
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        let manager_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
        assert!(manager_messages.iter().any(|message| {
            message.body.contains("Summary: 1 tests failed")
                && message.body.contains("tests::test_dispatch_wip_guard")
        }));
    }

    #[test]
    fn test_failure_escalation_is_suppressed_when_recently_sent() {
        let (_tmp, repo, _worktree_dir) = setup_failing_test_repo("eng-1");
        let mut daemon = setup_completion_daemon(&repo, "eng-1");
        daemon
            .config
            .team_config
            .workflow_policy
            .verification
            .max_iterations = 1;
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);
        daemon
            .recent_escalations
            .insert("tests_failed_42_eng-1".to_string(), Instant::now());

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        let manager_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
        assert!(
            manager_messages
                .iter()
                .all(|message| !message.body.contains("failed tests after 2 retries")),
            "recent test failure escalation should be suppressed"
        );
    }

    #[test]
    fn completion_auto_merges_small_clean_diff() {
        let (_tmp, repo, _worktree_dir) = setup_auto_merge_repo("eng-1");

        let policy = AutoMergePolicy {
            enabled: true,
            ..AutoMergePolicy::default()
        };
        let mut daemon = auto_merge_daemon(&repo, policy);

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        assert_eq!(daemon.queued_merge_count_for_test(), 1);
        assert_eq!(daemon.active_task_id("eng-1"), Some(42));
        assert_eq!(
            daemon.member_state_for_test("eng-1"),
            Some(MemberState::Working)
        );

        let task = crate::task::Task::from_file(
            &repo
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks")
                .join("042-auto-merge-task.md"),
        )
        .unwrap();
        assert_eq!(task.status, "review");

        daemon.process_merge_queue_for_test().unwrap();

        assert_eq!(daemon.queued_merge_count_for_test(), 0);
        assert_eq!(daemon.active_task_id("eng-1"), None);
        assert_eq!(
            daemon.member_state_for_test("eng-1"),
            Some(MemberState::Idle)
        );

        // note.txt should be merged into main
        assert_eq!(
            std::fs::read_to_string(repo.join("note.txt")).unwrap(),
            "done\n"
        );

        // Verify auto-merge event was emitted
        let events_path = repo.join(".batty").join("team_config").join("events.jsonl");
        let events = read_events(&events_path).unwrap();
        let auto_merge_events: Vec<_> = events
            .iter()
            .filter(|e| e.event == "task_auto_merged")
            .collect();
        assert_eq!(auto_merge_events.len(), 1);
        assert_eq!(auto_merge_events[0].role.as_deref(), Some("eng-1"));
        assert_eq!(auto_merge_events[0].task.as_deref(), Some("42"));
        let decision_event = events
            .iter()
            .find(|e| e.event == "auto_merge_decision_recorded")
            .expect("should record auto-merge decision");
        assert_eq!(decision_event.action_type.as_deref(), Some("accepted"));
        assert!(
            decision_event
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("accepted for auto-merge")),
            "decision reason should explain acceptance: {:?}",
            decision_event.reason
        );
        let post_verify_event = events
            .iter()
            .find(|e| e.event == "auto_merge_post_verify_result")
            .expect("should record post-merge verification result");
        assert_eq!(post_verify_event.success, Some(true));
        assert_eq!(post_verify_event.reason.as_deref(), Some("passed"));
    }

    #[test]
    fn completion_routes_large_diff_to_review() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-auto-merge-test");
        write_task_file(&repo, 42, "large-diff-task");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        // Create a large diff: many files across multiple modules
        for i in 0..10 {
            let dir = worktree_dir.join(format!("module_{i}"));
            std::fs::create_dir_all(&dir).unwrap();
            let content: String = (0..50).map(|j| format!("line {j}\n")).collect();
            std::fs::write(dir.join("file.rs"), content).unwrap();
        }
        git_ok(&worktree_dir, &["add", "."]);
        git_ok(&worktree_dir, &["commit", "-m", "large change"]);

        let policy = AutoMergePolicy {
            enabled: true,
            max_files_changed: 5,
            max_diff_lines: 200,
            ..AutoMergePolicy::default()
        };
        let mut daemon = auto_merge_daemon(&repo, policy);

        complete_task_and_process_queue(&mut daemon, "eng-1");

        let task = crate::task::Task::from_file(
            &repo
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks")
                .join("042-large-diff-task.md"),
        )
        .unwrap();
        assert_eq!(task.status, "review");
        assert_eq!(task.review_owner.as_deref(), Some("manager"));
        assert_eq!(daemon.active_task_id("eng-1"), None);
        assert_eq!(
            daemon.member_state_for_test("eng-1"),
            Some(MemberState::Idle)
        );

        let manager_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
        assert!(
            manager_messages
                .iter()
                .any(|m| m.body.contains("manual review")),
            "manager should receive manual review message: {:?}",
            manager_messages
        );
        let events_path = repo.join(".batty").join("team_config").join("events.jsonl");
        let events = read_events(&events_path).unwrap();
        let decision_event = events
            .iter()
            .find(|e| e.event == "auto_merge_decision_recorded")
            .expect("should record manual review decision");
        assert_eq!(decision_event.action_type.as_deref(), Some("manual_review"));
        assert!(
            decision_event
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("manual review")),
            "decision reason should explain manual review: {:?}",
            decision_event.reason
        );
    }

    #[test]
    fn move_task_to_review_requires_commits_ahead_of_main() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-review-gate-test");
        write_task_file(&repo, 42, "review-gate-task");

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        let mut daemon = make_test_daemon(
            &repo,
            vec![
                manager_member("manager", None),
                engineer_member("eng-1", Some("manager"), true),
            ],
        );

        let moved = move_task_to_review(
            &mut daemon,
            &repo.join(".batty").join("team_config").join("board"),
            42,
            Some("manager"),
            "eng-1",
        )
        .unwrap();

        assert!(
            !moved,
            "review transition should be blocked without commits"
        );

        let task = crate::task::Task::from_file(
            &repo
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks")
                .join("042-review-gate-task.md"),
        )
        .unwrap();
        assert_eq!(task.status, "in-progress");
        assert!(task.review_owner.is_none());

        let engineer_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "eng-1").unwrap();
        assert!(
            engineer_messages
                .iter()
                .any(|m| m.body.contains("no commits ahead of main")),
            "engineer should receive review-blocked message: {:?}",
            engineer_messages
        );

        let manager_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
        assert!(
            manager_messages.is_empty(),
            "manager should not be paged for a blocked false-review: {:?}",
            manager_messages
        );
    }

    #[test]
    fn completion_respects_disabled_policy() {
        let (_tmp, repo, _worktree_dir) = setup_auto_merge_repo("eng-1");

        let policy = AutoMergePolicy {
            enabled: false,
            ..AutoMergePolicy::default()
        };
        assert!(!policy.enabled);

        let mut daemon = auto_merge_daemon(&repo, policy);

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        // With auto-merge disabled, should fall through to normal merge (no review gate)
        assert_eq!(daemon.active_task_id("eng-1"), None);
        assert_eq!(
            daemon.member_state_for_test("eng-1"),
            Some(MemberState::Idle)
        );
        // note.txt merged into main
        assert_eq!(
            std::fs::read_to_string(repo.join("note.txt")).unwrap(),
            "done\n"
        );

        // No auto-merge event should be emitted
        let events_path = repo.join(".batty").join("team_config").join("events.jsonl");
        let events = read_events(&events_path).unwrap();
        assert!(
            !events.iter().any(|e| e.event == "task_auto_merged"),
            "no auto-merge event should be emitted when policy is disabled"
        );
    }

    #[test]
    fn completion_respects_per_task_override() {
        let (_tmp, repo, _worktree_dir) = setup_auto_merge_repo("eng-1");

        let policy = AutoMergePolicy {
            enabled: true,
            ..AutoMergePolicy::default()
        };
        let mut daemon = auto_merge_daemon(&repo, policy);
        daemon.set_auto_merge_override(42, false); // Force manual review

        handle_engineer_completion(&mut daemon, "eng-1").unwrap();

        // Manager should have received a message about override
        let manager_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
        assert!(
            manager_messages
                .iter()
                .any(|m| m.body.contains("Auto-merge disabled by override")),
            "manager should receive override message: {:?}",
            manager_messages
        );
    }

    #[test]
    fn auto_merge_emits_event() {
        let (_tmp, repo, _worktree_dir) = setup_auto_merge_repo("eng-1");

        let policy = AutoMergePolicy {
            enabled: true,
            ..AutoMergePolicy::default()
        };
        let mut daemon = auto_merge_daemon(&repo, policy);

        complete_task_and_process_queue(&mut daemon, "eng-1");

        let events_path = repo.join(".batty").join("team_config").join("events.jsonl");
        let events = read_events(&events_path).unwrap();
        let auto_event = events
            .iter()
            .find(|e| e.event == "task_auto_merged")
            .expect("should have task_auto_merged event");

        assert_eq!(auto_event.role.as_deref(), Some("eng-1"));
        assert_eq!(auto_event.task.as_deref(), Some("42"));
        // Confidence should be stored in load field
        assert!(auto_event.load.is_some());
        let confidence = auto_event.load.unwrap();
        assert!(
            confidence > 0.0 && confidence <= 1.0,
            "confidence should be between 0 and 1, got {}",
            confidence
        );
        // Reason should contain files and lines info
        assert!(
            auto_event
                .reason
                .as_ref()
                .is_some_and(|r| r.contains("files=") && r.contains("lines=")),
            "reason should contain diff stats: {:?}",
            auto_event.reason
        );
    }

    fn production_unwrap_expect_count(source: &str) -> usize {
        let prod = if let Some(pos) = source.find("\n#[cfg(test)]\nmod tests") {
            &source[..pos]
        } else {
            source
        };
        prod.lines()
            .filter(|line| {
                let trimmed = line.trim();
                !trimmed.starts_with("#[cfg(test)]")
                    && (trimmed.contains(".unwrap(") || trimmed.contains(".expect("))
            })
            .count()
    }

    #[test]
    fn production_merge_completion_has_no_unwrap_or_expect_calls() {
        let src = include_str!("completion.rs");
        assert_eq!(
            production_unwrap_expect_count(src),
            0,
            "production completion.rs should avoid unwrap/expect"
        );
    }
}
