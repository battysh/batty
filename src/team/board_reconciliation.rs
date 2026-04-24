use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::task::{Task, load_tasks_from_dir};

pub(crate) const DEFAULT_BOARD_RECONCILIATION_INTERVAL_SECS: u64 = 300;
pub(crate) const DEFAULT_STUCK_TASK_THRESHOLD_SECS: u64 = 7200;
pub(crate) const DEFAULT_DONE_TASK_ARCHIVE_AFTER_SECS: u64 = 86400;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReconciliationOptions {
    pub now: DateTime<Utc>,
    pub active_members: Option<HashSet<String>>,
    pub stuck_task_threshold_secs: u64,
    pub done_task_archive_after_secs: u64,
    pub git_available: bool,
}

impl Default for ReconciliationOptions {
    fn default() -> Self {
        Self {
            now: Utc::now(),
            active_members: None,
            stuck_task_threshold_secs: DEFAULT_STUCK_TASK_THRESHOLD_SECS,
            done_task_archive_after_secs: DEFAULT_DONE_TASK_ARCHIVE_AFTER_SECS,
            git_available: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BoardFinding {
    ReviewTaskAlreadyMerged {
        task_id: u32,
        title: String,
        reason: String,
    },
    ReviewTaskMissingMetadata {
        task_id: u32,
        title: String,
        reasons: Vec<String>,
    },
    DoneTaskHasUnmergedBranch {
        task_id: u32,
        title: String,
        branch: String,
    },
    CrossTaskMetadataDrift {
        task_id: u32,
        title: String,
        reasons: Vec<String>,
    },
    BlockedTaskResolved {
        task_id: u32,
        title: String,
        dependencies: Vec<u32>,
    },
    OrphanedInProgressTask {
        task_id: u32,
        title: String,
        owner: Option<String>,
    },
    StuckTaskNoCommits {
        task_id: u32,
        title: String,
        owner: Option<String>,
        age_secs: u64,
    },
    DoneTaskReadyToArchive {
        task_id: u32,
        title: String,
        age_secs: u64,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ReconciliationSummary {
    pub orphan_count: usize,
    pub stuck_count: usize,
    pub auto_fixable_count: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ReconciliationReport {
    pub findings: Vec<BoardFinding>,
    pub summary: ReconciliationSummary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AppliedFix {
    RequeuedReview {
        task_id: u32,
        title: String,
        reasons: Vec<String>,
    },
    CompletedMergedReview {
        task_id: u32,
        title: String,
        reason: String,
    },
    Unblocked {
        task_id: u32,
        title: String,
    },
    RequeuedOrphaned {
        task_id: u32,
        title: String,
        owner: Option<String>,
    },
    ArchivedDone {
        task_id: u32,
        title: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ApplyReport {
    pub fixes: Vec<AppliedFix>,
}

pub(crate) fn scan_board_health(
    project_root: &Path,
    board_dir: &Path,
    options: &ReconciliationOptions,
) -> Result<ReconciliationReport> {
    let tasks_dir = board_dir.join("tasks");
    if !tasks_dir.is_dir() {
        return Ok(ReconciliationReport::default());
    }

    let tasks = load_tasks_from_dir(&tasks_dir)?;
    let mut findings = Vec::new();
    let done_task_ids: HashSet<u32> = tasks
        .iter()
        .filter(|task| matches!(task.status.as_str(), "done" | "archived"))
        .map(|task| task.id)
        .collect();

    let archive_candidates =
        archive_candidate_ids(board_dir, options.done_task_archive_after_secs)?;
    let archive_candidate_ids: HashSet<u32> =
        archive_candidates.iter().map(|task| task.id).collect();

    for task in &tasks {
        if task.status == "review" {
            let review_meta = crate::team::workflow::WorkflowMeta {
                state: crate::team::workflow::TaskState::Review,
                worktree_path: task.worktree_path.clone(),
                branch: task.branch.clone(),
                commit: task.commit.clone(),
                ..crate::team::workflow::WorkflowMeta::default()
            };
            match crate::team::review::validate_review_candidate(project_root, &review_meta)? {
                crate::team::review::ReviewEligibility::Eligible => {}
                crate::team::review::ReviewEligibility::AlreadyMerged { reason } => {
                    findings.push(BoardFinding::ReviewTaskAlreadyMerged {
                        task_id: task.id,
                        title: task.title.clone(),
                        reason,
                    });
                }
                crate::team::review::ReviewEligibility::MissingMetadata { reasons } => {
                    findings.push(BoardFinding::ReviewTaskMissingMetadata {
                        task_id: task.id,
                        title: task.title.clone(),
                        reasons,
                    });
                }
            }
        }

        let drift_reasons = crate::team::completion::completion_metadata_drift_reasons(
            project_root,
            &task.source_path,
            task.id,
            task.branch.as_deref(),
            &task.artifacts,
        )?;
        if !drift_reasons.is_empty() {
            findings.push(BoardFinding::CrossTaskMetadataDrift {
                task_id: task.id,
                title: task.title.clone(),
                reasons: drift_reasons,
            });
        }

        if dependency_recovery_candidate(task, &done_task_ids) {
            findings.push(BoardFinding::BlockedTaskResolved {
                task_id: task.id,
                title: task.title.clone(),
                dependencies: task.depends_on.clone(),
            });
        }

        if task.status == "in-progress"
            && is_orphaned_in_progress_task(task, options.active_members.as_ref())
        {
            findings.push(BoardFinding::OrphanedInProgressTask {
                task_id: task.id,
                title: task.title.clone(),
                owner: task.claimed_by.clone(),
            });
        }

        if task.status == "in-progress"
            && task_age_secs(task, options.now) >= options.stuck_task_threshold_secs
            && commits_ahead_of_main(project_root, task, options.git_available)? == 0
        {
            findings.push(BoardFinding::StuckTaskNoCommits {
                task_id: task.id,
                title: task.title.clone(),
                owner: task.claimed_by.clone(),
                age_secs: task_age_secs(task, options.now),
            });
        }

        if task.status == "done" && archive_candidate_ids.contains(&task.id) {
            findings.push(BoardFinding::DoneTaskReadyToArchive {
                task_id: task.id,
                title: task.title.clone(),
                age_secs: task_age_secs(task, options.now),
            });
        }

        if task.status == "done"
            && options.git_available
            && let Some(branch) = task.branch.as_deref()
            && !branch.is_empty()
            && !crate::team::task_loop::branch_is_merged_into(project_root, branch, "main")?
        {
            findings.push(BoardFinding::DoneTaskHasUnmergedBranch {
                task_id: task.id,
                title: task.title.clone(),
                branch: branch.to_string(),
            });
        }
    }

    Ok(ReconciliationReport {
        summary: ReconciliationSummary {
            orphan_count: findings
                .iter()
                .filter(|finding| matches!(finding, BoardFinding::OrphanedInProgressTask { .. }))
                .count(),
            stuck_count: findings
                .iter()
                .filter(|finding| matches!(finding, BoardFinding::StuckTaskNoCommits { .. }))
                .count(),
            auto_fixable_count: findings
                .iter()
                .filter(|finding| {
                    matches!(
                        finding,
                        BoardFinding::ReviewTaskAlreadyMerged { .. }
                            | BoardFinding::ReviewTaskMissingMetadata { .. }
                            | BoardFinding::BlockedTaskResolved { .. }
                            | BoardFinding::OrphanedInProgressTask { .. }
                            | BoardFinding::DoneTaskReadyToArchive { .. }
                    )
                })
                .count(),
        },
        findings,
    })
}

fn dependency_recovery_candidate(task: &Task, done_task_ids: &HashSet<u32>) -> bool {
    if task.depends_on.is_empty()
        || !task
            .depends_on
            .iter()
            .all(|dependency| done_task_ids.contains(dependency))
    {
        return false;
    }

    match task.status.as_str() {
        "blocked" => true,
        "todo" | "backlog" | "runnable" => {
            task.blocked.is_some() || task.blocked_on.is_some() || task.claimed_by.is_some()
        }
        _ => false,
    }
}

pub(crate) fn apply_safe_fixes(
    board_dir: &Path,
    report: &ReconciliationReport,
) -> Result<ApplyReport> {
    let mut fixes = Vec::new();
    let tasks_dir = board_dir.join("tasks");
    let current_tasks = if tasks_dir.is_dir() {
        load_tasks_from_dir(&tasks_dir)?
    } else {
        Vec::new()
    };

    let archive_task_ids: HashSet<u32> = report
        .findings
        .iter()
        .filter_map(|finding| match finding {
            BoardFinding::DoneTaskReadyToArchive { task_id, .. } => Some(*task_id),
            _ => None,
        })
        .collect();
    if !archive_task_ids.is_empty() {
        let to_archive: Vec<Task> = current_tasks
            .into_iter()
            .filter(|task| archive_task_ids.contains(&task.id))
            .collect();
        let summary = crate::team::board::archive_tasks(board_dir, &to_archive, false)?;
        if summary.archived_count > 0 {
            for task in &to_archive {
                fixes.push(AppliedFix::ArchivedDone {
                    task_id: task.id,
                    title: task.title.clone(),
                });
            }
        }
    }

    for finding in &report.findings {
        match finding {
            BoardFinding::ReviewTaskAlreadyMerged {
                task_id,
                title,
                reason,
            } => {
                if crate::team::task_cmd::find_task_path(board_dir, *task_id).is_ok() {
                    crate::team::task_cmd::transition_task_with_attribution(
                        board_dir,
                        *task_id,
                        "done",
                        crate::team::task_cmd::StatusTransitionAttribution::daemon(
                            "daemon.board_reconciliation.already_merged",
                        ),
                    )?;
                    fixes.push(AppliedFix::CompletedMergedReview {
                        task_id: *task_id,
                        title: title.clone(),
                        reason: reason.clone(),
                    });
                }
            }
            BoardFinding::ReviewTaskMissingMetadata {
                task_id,
                title,
                reasons,
            } => {
                if crate::team::task_cmd::find_task_path(board_dir, *task_id).is_ok() {
                    let _ = crate::team::task_cmd::transition_task_with_attribution(
                        board_dir,
                        *task_id,
                        "in-progress",
                        crate::team::task_cmd::StatusTransitionAttribution::daemon(
                            "daemon.board_reconciliation.review_metadata",
                        ),
                    );
                    crate::team::task_cmd::transition_task_with_attribution(
                        board_dir,
                        *task_id,
                        "todo",
                        crate::team::task_cmd::StatusTransitionAttribution::daemon(
                            "daemon.board_reconciliation.review_metadata",
                        ),
                    )?;
                    crate::team::task_cmd::unclaim_task(board_dir, *task_id)?;
                    fixes.push(AppliedFix::RequeuedReview {
                        task_id: *task_id,
                        title: title.clone(),
                        reasons: reasons.clone(),
                    });
                }
            }
            BoardFinding::BlockedTaskResolved { task_id, title, .. } => {
                if crate::team::task_cmd::find_task_path(board_dir, *task_id).is_ok() {
                    let current_task = crate::task::load_task_by_id(&tasks_dir, *task_id)?;
                    if current_task.status == "blocked" {
                        crate::team::task_cmd::transition_task_with_attribution(
                            board_dir,
                            *task_id,
                            "todo",
                            crate::team::task_cmd::StatusTransitionAttribution::daemon(
                                "daemon.board_reconciliation.unblocked",
                            ),
                        )?;
                    } else {
                        crate::team::task_cmd::clear_blocked_fields(board_dir, *task_id)?;
                    }
                    let current_task = crate::task::load_task_by_id(&tasks_dir, *task_id)?;
                    if current_task.status != "in-progress" {
                        crate::team::task_cmd::unclaim_task(board_dir, *task_id)?;
                    }
                    fixes.push(AppliedFix::Unblocked {
                        task_id: *task_id,
                        title: title.clone(),
                    });
                }
            }
            BoardFinding::OrphanedInProgressTask {
                task_id,
                title,
                owner,
            } => {
                if crate::team::task_cmd::find_task_path(board_dir, *task_id).is_ok() {
                    crate::team::task_cmd::transition_task_with_attribution(
                        board_dir,
                        *task_id,
                        "todo",
                        crate::team::task_cmd::StatusTransitionAttribution::daemon(
                            "daemon.board_reconciliation.orphaned",
                        ),
                    )?;
                    crate::team::task_cmd::unclaim_task(board_dir, *task_id)?;
                    fixes.push(AppliedFix::RequeuedOrphaned {
                        task_id: *task_id,
                        title: title.clone(),
                        owner: owner.clone(),
                    });
                }
            }
            BoardFinding::DoneTaskHasUnmergedBranch { .. }
            | BoardFinding::CrossTaskMetadataDrift { .. }
            | BoardFinding::StuckTaskNoCommits { .. }
            | BoardFinding::DoneTaskReadyToArchive { .. } => {}
        }
    }

    Ok(ApplyReport { fixes })
}

pub(crate) fn render_report(report: &ReconciliationReport) -> String {
    let mut out = String::new();
    out.push_str("== Board Reconciliation ==\n");
    if report.findings.is_empty() {
        out.push_str("PASS: no board health reconciliation issues detected\n");
        return out;
    }

    for finding in &report.findings {
        match finding {
            BoardFinding::ReviewTaskAlreadyMerged {
                task_id,
                title,
                reason,
            } => {
                out.push_str(&format!(
                    "WARN: task #{task_id} ({title}) is parked in review even though it is already merged ({reason})\n"
                ));
            }
            BoardFinding::ReviewTaskMissingMetadata {
                task_id,
                title,
                reasons,
            } => {
                out.push_str(&format!(
                    "WARN: task #{task_id} ({title}) is parked in review without actionable workflow metadata ({})\n",
                    reasons.join("; ")
                ));
            }
            BoardFinding::DoneTaskHasUnmergedBranch {
                task_id,
                title,
                branch,
            } => {
                out.push_str(&format!(
                    "WARN: task #{task_id} ({title}) is done but branch '{branch}' is not merged into main\n"
                ));
            }
            BoardFinding::CrossTaskMetadataDrift {
                task_id,
                title,
                reasons,
            } => {
                out.push_str(&format!(
                    "WARN: task #{task_id} ({title}) has cross-task completion metadata drift ({})\n",
                    reasons.join("; ")
                ));
            }
            BoardFinding::BlockedTaskResolved {
                task_id,
                title,
                dependencies,
            } => {
                out.push_str(&format!(
                    "WARN: task #{task_id} ({title}) is blocked even though dependencies {:?} are done\n",
                    dependencies
                ));
            }
            BoardFinding::OrphanedInProgressTask {
                task_id,
                title,
                owner,
            } => {
                out.push_str(&format!(
                    "WARN: task #{task_id} ({title}) is in-progress with no active owner ({})\n",
                    owner.as_deref().unwrap_or("unclaimed")
                ));
            }
            BoardFinding::StuckTaskNoCommits {
                task_id,
                title,
                owner,
                age_secs,
            } => {
                out.push_str(&format!(
                    "WARN: task #{task_id} ({title}) has been in-progress for {} with no commits ahead of main ({})\n",
                    format_duration(*age_secs),
                    owner.as_deref().unwrap_or("unclaimed")
                ));
            }
            BoardFinding::DoneTaskReadyToArchive {
                task_id,
                title,
                age_secs,
            } => {
                out.push_str(&format!(
                    "WARN: task #{task_id} ({title}) is done and older than archive threshold ({})\n",
                    format_duration(*age_secs)
                ));
            }
        }
    }

    out.push_str(&format!(
        "Summary: orphaned={}, stuck={}, auto-fixable={}\n",
        report.summary.orphan_count, report.summary.stuck_count, report.summary.auto_fixable_count
    ));
    out
}

pub(crate) fn render_apply_report(report: &ApplyReport) -> String {
    let mut out = String::new();
    if report.fixes.is_empty() {
        out.push_str("No safe board reconciliation fixes were applied.\n");
        return out;
    }

    out.push_str("Applied board reconciliation fixes:\n");
    for fix in &report.fixes {
        match fix {
            AppliedFix::Unblocked { task_id, title } => {
                out.push_str(&format!("  - unblocked task #{task_id} ({title})\n"));
            }
            AppliedFix::RequeuedOrphaned {
                task_id,
                title,
                owner,
            } => {
                out.push_str(&format!(
                    "  - requeued orphaned task #{task_id} ({title}) from {}\n",
                    owner.as_deref().unwrap_or("unclaimed state")
                ));
            }
            AppliedFix::ArchivedDone { task_id, title } => {
                out.push_str(&format!("  - archived task #{task_id} ({title})\n"));
            }
            AppliedFix::CompletedMergedReview {
                task_id,
                title,
                reason,
            } => {
                out.push_str(&format!(
                    "  - completed merged review task #{task_id} ({title}): {reason}\n"
                ));
            }
            AppliedFix::RequeuedReview {
                task_id,
                title,
                reasons,
            } => {
                out.push_str(&format!(
                    "  - requeued review task #{task_id} ({title}): {}\n",
                    reasons.join("; ")
                ));
            }
        }
    }
    out
}

fn archive_candidate_ids(board_dir: &Path, threshold_secs: u64) -> Result<Vec<Task>> {
    crate::team::board::done_tasks_older_than(board_dir, Duration::from_secs(threshold_secs))
}

fn is_orphaned_in_progress_task(task: &Task, active_members: Option<&HashSet<String>>) -> bool {
    match task.claimed_by.as_deref() {
        None => true,
        Some(owner) => active_members.is_some_and(|active| !active.contains(owner)),
    }
}

fn commits_ahead_of_main(project_root: &Path, task: &Task, git_available: bool) -> Result<u32> {
    if !git_available {
        return Ok(0);
    }

    if let Some(worktree_path) = task.worktree_path.as_deref() {
        let worktree_dir = PathBuf::from(worktree_path);
        if worktree_dir.exists() {
            return crate::team::git_cmd::rev_list_count(&worktree_dir, "main..HEAD")
                .map_err(Into::into);
        }
    }

    if let Some(branch) = task.branch.as_deref() {
        if !branch.is_empty() {
            return crate::team::git_cmd::rev_list_count(project_root, &format!("main..{branch}"))
                .map_err(Into::into);
        }
    }

    Ok(0)
}

fn task_age_secs(task: &Task, now: DateTime<Utc>) -> u64 {
    let frontmatter_timestamp = task
        .last_progress_at
        .as_deref()
        .or(task.claimed_at.as_deref())
        .or(task.completed.as_deref())
        .and_then(crate::task::parse_frontmatter_timestamp_compat);
    if let Some(timestamp) = frontmatter_timestamp {
        return now.signed_duration_since(timestamp).num_seconds().max(0) as u64;
    }

    std::fs::metadata(&task.source_path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .map(|mtime| {
            let modified_at: DateTime<Utc> = mtime.into();
            now.signed_duration_since(modified_at).num_seconds().max(0) as u64
        })
        .unwrap_or(0)
}

fn format_duration(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::test_support::{git_ok, init_git_repo};
    use std::fs;
    use std::process::Command;
    use tempfile::tempdir;

    fn write_task(
        project_root: &Path,
        id: u32,
        title: &str,
        status: &str,
        claimed_by: Option<&str>,
        depends_on: &[u32],
        branch: Option<&str>,
        commit: Option<&str>,
        worktree_path: Option<&Path>,
        completed: Option<&str>,
    ) {
        let tasks_dir = project_root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        let depends = if depends_on.is_empty() {
            "depends_on: []\n".to_string()
        } else {
            format!(
                "depends_on:\n{}\n",
                depends_on
                    .iter()
                    .map(|dep| format!("  - {dep}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };
        let claimed_by = claimed_by
            .map(|owner| format!("claimed_by: {owner}\n"))
            .unwrap_or_default();
        let progress_timestamps = if claimed_by.is_empty() {
            String::new()
        } else {
            "claimed_at: 2026-04-06T09:00:00Z\nlast_progress_at: 2026-04-06T09:00:00Z\n".to_string()
        };
        let branch = branch
            .map(|value| format!("branch: {value}\n"))
            .unwrap_or_default();
        let commit = commit
            .map(|value| format!("commit: {value}\n"))
            .unwrap_or_default();
        let worktree_path = worktree_path
            .map(|path| format!("worktree_path: {}\n", path.display()))
            .unwrap_or_default();
        let completed = completed
            .map(|value| format!("completed: {value}\n"))
            .unwrap_or_default();
        fs::write(
            tasks_dir.join(format!("{id:03}-{title}.md")),
            format!(
                "---\nid: {id}\ntitle: {title}\nstatus: {status}\npriority: high\n{claimed_by}{progress_timestamps}{depends}{branch}{commit}{worktree_path}{completed}---\n\nTask body.\n"
            ),
        )
        .unwrap();
    }

    fn write_stale_dependency_task(
        project_root: &Path,
        id: u32,
        title: &str,
        status: &str,
        claimed_by: Option<&str>,
        depends_on: &[u32],
        blocked_on: Option<&str>,
    ) {
        let tasks_dir = project_root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        let depends = format!(
            "depends_on:\n{}\n",
            depends_on
                .iter()
                .map(|dep| format!("  - {dep}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let claimed_by = claimed_by
            .map(|owner| format!("claimed_by: {owner}\n"))
            .unwrap_or_default();
        let blocked_on = blocked_on
            .map(|reason| format!("blocked: true\nblock_reason: {reason}\nblocked_on: {reason}\n"))
            .unwrap_or_default();
        fs::write(
            tasks_dir.join(format!("{id:03}-{title}.md")),
            format!(
                "---\nid: {id}\ntitle: {title}\nstatus: {status}\npriority: high\n{claimed_by}{depends}{blocked_on}---\n\nTask body.\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn scan_detects_resolved_blocked_task() {
        let tmp = tempdir().unwrap();
        let repo = init_git_repo(&tmp, "reconcile_blocked");
        write_task(&repo, 1, "dep", "done", None, &[], None, None, None, None);
        write_task(
            &repo,
            2,
            "blocked",
            "blocked",
            None,
            &[1],
            None,
            None,
            None,
            None,
        );

        let report = scan_board_health(
            &repo,
            &repo.join(".batty").join("team_config").join("board"),
            &ReconciliationOptions::default(),
        )
        .unwrap();

        assert!(report.findings.iter().any(|finding| matches!(
            finding,
            BoardFinding::BlockedTaskResolved { task_id: 2, .. }
        )));
    }

    #[test]
    fn scan_keeps_dependency_task_blocked_when_parent_is_not_done() {
        let tmp = tempdir().unwrap();
        let repo = init_git_repo(&tmp, "reconcile_parent_todo");
        write_task(&repo, 1, "dep", "todo", None, &[], None, None, None, None);
        write_stale_dependency_task(
            &repo,
            2,
            "child",
            "todo",
            Some("eng-1"),
            &[1],
            Some("waiting on dependencies"),
        );

        let report = scan_board_health(
            &repo,
            &repo.join(".batty").join("team_config").join("board"),
            &ReconciliationOptions::default(),
        )
        .unwrap();

        assert!(!report.findings.iter().any(|finding| matches!(
            finding,
            BoardFinding::BlockedTaskResolved { task_id: 2, .. }
        )));
    }

    #[test]
    fn apply_safe_fixes_clears_stale_todo_dependency_claim_when_parent_done() {
        let tmp = tempdir().unwrap();
        let repo = init_git_repo(&tmp, "reconcile_stale_todo_claim");
        let board_dir = repo.join(".batty").join("team_config").join("board");
        write_task(&repo, 1, "dep", "done", None, &[], None, None, None, None);
        write_stale_dependency_task(
            &repo,
            2,
            "child",
            "todo",
            Some("eng-1"),
            &[1],
            Some("waiting on dependencies"),
        );

        let report =
            scan_board_health(&repo, &board_dir, &ReconciliationOptions::default()).unwrap();
        assert!(report.findings.iter().any(|finding| matches!(
            finding,
            BoardFinding::BlockedTaskResolved { task_id: 2, .. }
        )));

        apply_safe_fixes(&board_dir, &report).unwrap();

        let task = crate::task::load_task_by_id(&board_dir.join("tasks"), 2).unwrap();
        assert_eq!(task.status, "todo");
        assert!(task.claimed_by.is_none());
        assert!(task.blocked.is_none());
        assert!(task.blocked_on.is_none());
    }

    #[test]
    fn apply_safe_fixes_does_not_clear_active_in_progress_claim() {
        let tmp = tempdir().unwrap();
        let repo = init_git_repo(&tmp, "reconcile_preserve_active_claim");
        let board_dir = repo.join(".batty").join("team_config").join("board");
        write_task(&repo, 1, "dep", "done", None, &[], None, None, None, None);
        write_stale_dependency_task(
            &repo,
            2,
            "child",
            "in-progress",
            Some("eng-1"),
            &[1],
            Some("waiting on dependencies"),
        );
        let report = ReconciliationReport {
            findings: vec![BoardFinding::BlockedTaskResolved {
                task_id: 2,
                title: "child".to_string(),
                dependencies: vec![1],
            }],
            summary: ReconciliationSummary::default(),
        };

        apply_safe_fixes(&board_dir, &report).unwrap();

        let task = crate::task::load_task_by_id(&board_dir.join("tasks"), 2).unwrap();
        assert_eq!(task.status, "in-progress");
        assert_eq!(task.claimed_by.as_deref(), Some("eng-1"));
        assert!(task.blocked.is_none());
        assert!(task.blocked_on.is_none());
    }

    #[test]
    fn scan_detects_done_task_with_unmerged_branch() {
        let tmp = tempdir().unwrap();
        let repo = init_git_repo(&tmp, "reconcile_unmerged");
        git_ok(&repo, &["checkout", "-b", "eng-1/task-2"]);
        fs::write(repo.join("src").join("feature.rs"), "pub fn feature() {}\n").unwrap();
        git_ok(&repo, &["add", "."]);
        git_ok(&repo, &["commit", "-m", "feature"]);
        git_ok(&repo, &["checkout", "main"]);
        write_task(
            &repo,
            2,
            "done-task",
            "done",
            Some("eng-1"),
            &[],
            Some("eng-1/task-2"),
            None,
            None,
            None,
        );

        let report = scan_board_health(
            &repo,
            &repo.join(".batty").join("team_config").join("board"),
            &ReconciliationOptions::default(),
        )
        .unwrap();

        assert!(report.findings.iter().any(|finding| matches!(
            finding,
            BoardFinding::DoneTaskHasUnmergedBranch { task_id: 2, .. }
        )));
    }

    #[test]
    fn scan_reports_cross_task_metadata_drift() {
        let tmp = tempdir().unwrap();
        let repo = init_git_repo(&tmp, "reconcile_metadata_drift");
        write_task(
            &repo,
            687,
            "wrong-metadata",
            "review",
            Some("eng-1"),
            &[],
            Some("eng-1-1/699"),
            Some("abc1234"),
            None,
            None,
        );

        let report = scan_board_health(
            &repo,
            &repo.join(".batty").join("team_config").join("board"),
            &ReconciliationOptions {
                git_available: false,
                ..ReconciliationOptions::default()
            },
        )
        .unwrap();

        assert!(report.findings.iter().any(|finding| matches!(
            finding,
            BoardFinding::CrossTaskMetadataDrift { task_id: 687, reasons, .. }
                if reasons.iter().any(|reason| reason.contains("#699"))
        )));
        let rendered = render_report(&report);
        assert!(rendered.contains("cross-task completion metadata drift"));
        assert!(rendered.contains("#699"));
    }

    #[test]
    fn scan_detects_orphaned_in_progress_task() {
        let tmp = tempdir().unwrap();
        let repo = init_git_repo(&tmp, "reconcile_orphan");
        write_task(
            &repo,
            7,
            "orphaned",
            "in-progress",
            Some("eng-1"),
            &[],
            None,
            None,
            None,
            None,
        );
        let options = ReconciliationOptions {
            active_members: Some(HashSet::from(["eng-2".to_string()])),
            ..ReconciliationOptions::default()
        };

        let report = scan_board_health(
            &repo,
            &repo.join(".batty").join("team_config").join("board"),
            &options,
        )
        .unwrap();

        assert!(report.findings.iter().any(|finding| matches!(
            finding,
            BoardFinding::OrphanedInProgressTask { task_id: 7, .. }
        )));
    }

    #[test]
    fn scan_detects_stuck_task_with_no_commits() {
        let tmp = tempdir().unwrap();
        let repo = init_git_repo(&tmp, "reconcile_stuck");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        fs::create_dir_all(&worktree_dir).unwrap();
        git_ok(
            &repo,
            &[
                "worktree",
                "add",
                worktree_dir.to_string_lossy().as_ref(),
                "-b",
                "eng-1/task-9",
                "main",
            ],
        );
        write_task(
            &repo,
            9,
            "stuck",
            "in-progress",
            Some("eng-1"),
            &[],
            Some("eng-1/task-9"),
            None,
            Some(&worktree_dir),
            None,
        );
        let report = scan_board_health(
            &repo,
            &repo.join(".batty").join("team_config").join("board"),
            &ReconciliationOptions {
                now: chrono::DateTime::parse_from_rfc3339("2026-04-06T12:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
                active_members: Some(HashSet::from(["eng-1".to_string()])),
                stuck_task_threshold_secs: 7200,
                ..ReconciliationOptions::default()
            },
        )
        .unwrap();

        assert!(
            report.findings.iter().any(|finding| matches!(
                finding,
                BoardFinding::StuckTaskNoCommits { task_id: 9, .. }
            ))
        );
    }

    #[test]
    fn scan_detects_done_task_ready_to_archive() {
        let tmp = tempdir().unwrap();
        let repo = init_git_repo(&tmp, "reconcile_archive");
        write_task(
            &repo,
            11,
            "archive-me",
            "done",
            None,
            &[],
            None,
            None,
            None,
            Some("2026-04-01T00:00:00Z"),
        );

        let report = scan_board_health(
            &repo,
            &repo.join(".batty").join("team_config").join("board"),
            &ReconciliationOptions {
                now: chrono::DateTime::parse_from_rfc3339("2026-04-05T12:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
                done_task_archive_after_secs: 86400,
                ..ReconciliationOptions::default()
            },
        )
        .unwrap();

        assert!(report.findings.iter().any(|finding| matches!(
            finding,
            BoardFinding::DoneTaskReadyToArchive { task_id: 11, .. }
        )));
    }

    #[test]
    fn scan_detects_review_task_already_merged_and_safe_fix_marks_done() {
        let tmp = tempdir().unwrap();
        let repo = init_git_repo(&tmp, "reconcile_review_merged");
        let board_dir = repo.join(".batty").join("team_config").join("board");

        git_ok(&repo, &["checkout", "-b", "eng-1/task-12"]);
        fs::write(
            repo.join("src").join("reviewed.rs"),
            "pub fn reviewed() {}\n",
        )
        .unwrap();
        git_ok(&repo, &["add", "."]);
        git_ok(&repo, &["commit", "-m", "review candidate"]);
        let commit = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&repo)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        git_ok(&repo, &["checkout", "main"]);
        git_ok(
            &repo,
            &[
                "merge",
                "--no-ff",
                "eng-1/task-12",
                "-m",
                "merge review candidate",
            ],
        );

        write_task(
            &repo,
            12,
            "review-merged",
            "review",
            Some("eng-1"),
            &[],
            Some("eng-1/task-12"),
            Some(&commit),
            None,
            None,
        );

        let report =
            scan_board_health(&repo, &board_dir, &ReconciliationOptions::default()).unwrap();
        assert!(report.findings.iter().any(|finding| matches!(
            finding,
            BoardFinding::ReviewTaskAlreadyMerged { task_id: 12, .. }
        )));

        let applied = apply_safe_fixes(&board_dir, &report).unwrap();
        assert!(
            applied
                .fixes
                .iter()
                .any(|fix| matches!(fix, AppliedFix::CompletedMergedReview { task_id: 12, .. }))
        );

        let tasks = load_tasks_from_dir(&board_dir.join("tasks")).unwrap();
        assert_eq!(
            tasks.iter().find(|task| task.id == 12).unwrap().status,
            "done"
        );
    }

    #[test]
    fn scan_detects_review_task_missing_metadata_and_requeues_it() {
        let tmp = tempdir().unwrap();
        let repo = init_git_repo(&tmp, "reconcile_review_missing");
        let board_dir = repo.join(".batty").join("team_config").join("board");

        write_task(
            &repo,
            13,
            "review-missing-meta",
            "review",
            Some("eng-1"),
            &[],
            None,
            None,
            None,
            None,
        );

        let report =
            scan_board_health(&repo, &board_dir, &ReconciliationOptions::default()).unwrap();
        assert!(report.findings.iter().any(|finding| matches!(
            finding,
            BoardFinding::ReviewTaskMissingMetadata { task_id: 13, .. }
        )));

        let applied = apply_safe_fixes(&board_dir, &report).unwrap();
        assert!(
            applied
                .fixes
                .iter()
                .any(|fix| matches!(fix, AppliedFix::RequeuedReview { task_id: 13, .. }))
        );

        let tasks = load_tasks_from_dir(&board_dir.join("tasks")).unwrap();
        let task = tasks.iter().find(|task| task.id == 13).unwrap();
        assert_eq!(task.status, "todo");
        assert!(task.claimed_by.is_none());
        assert!(task.review_owner.is_none());
    }

    #[test]
    fn apply_safe_fixes_unblocks_requeues_and_archives() {
        let tmp = tempdir().unwrap();
        let repo = init_git_repo(&tmp, "reconcile_apply");
        let board_dir = repo.join(".batty").join("team_config").join("board");
        write_task(&repo, 1, "dep", "done", None, &[], None, None, None, None);
        write_task(
            &repo,
            2,
            "blocked",
            "blocked",
            None,
            &[1],
            None,
            None,
            None,
            None,
        );
        write_task(
            &repo,
            3,
            "orphan",
            "in-progress",
            Some("eng-1"),
            &[],
            None,
            None,
            None,
            None,
        );
        write_task(
            &repo,
            4,
            "done-old",
            "done",
            None,
            &[],
            None,
            None,
            None,
            Some("2026-04-01T00:00:00Z"),
        );

        let report = scan_board_health(
            &repo,
            &board_dir,
            &ReconciliationOptions {
                now: chrono::DateTime::parse_from_rfc3339("2026-04-05T12:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
                active_members: Some(HashSet::new()),
                done_task_archive_after_secs: 86400,
                ..ReconciliationOptions::default()
            },
        )
        .unwrap();
        let applied = apply_safe_fixes(&board_dir, &report).unwrap();

        assert!(
            applied
                .fixes
                .iter()
                .any(|fix| matches!(fix, AppliedFix::Unblocked { task_id: 2, .. }))
        );
        assert!(
            applied
                .fixes
                .iter()
                .any(|fix| matches!(fix, AppliedFix::RequeuedOrphaned { task_id: 3, .. }))
        );
        assert!(
            applied
                .fixes
                .iter()
                .any(|fix| matches!(fix, AppliedFix::ArchivedDone { task_id: 4, .. }))
        );

        let remaining = load_tasks_from_dir(&board_dir.join("tasks")).unwrap();
        assert_eq!(
            remaining.iter().find(|task| task.id == 2).unwrap().status,
            "todo"
        );
        let orphan = remaining.iter().find(|task| task.id == 3).unwrap();
        assert_eq!(orphan.status, "todo");
        assert!(orphan.claimed_by.is_none());
        assert!(board_dir.join("archive").join("004-done-old.md").exists());
    }
}
