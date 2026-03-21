use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::artifact::read_test_timing_log;
use super::config::{RoleType, TeamConfig};
use super::git_cmd;
use super::hierarchy::{self, MemberInstance};
use super::standup::MemberState;
use crate::task::load_tasks_from_dir;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct LaunchIdentityRecord {
    agent: String,
    prompt: String,
    session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DoctorDaemonState {
    clean_shutdown: bool,
    saved_at: u64,
    states: HashMap<String, MemberState>,
    active_tasks: HashMap<String, u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResumeEligibility {
    member: String,
    eligible: bool,
    reason: String,
    stored_prompt_hash: Option<String>,
    current_prompt_hash: Option<String>,
    session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorktreeStatus {
    member: String,
    path: PathBuf,
    branch: Option<String>,
    dirty: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LogSize {
    name: &'static str,
    bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckLevel {
    Pass,
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CheckLine {
    level: CheckLevel,
    message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OrphanStatus {
    branches: Vec<String>,
    worktrees: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CleanupSummary {
    branches_removed: usize,
    worktrees_removed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveTaskTargets {
    branches: HashSet<String>,
    worktrees: HashSet<PathBuf>,
}

pub fn run(project_root: &Path, fix: bool) -> Result<String> {
    let mut report = build_report(project_root)?;
    if !fix {
        return Ok(report);
    }

    let orphan_status = detect_orphans(project_root)?;
    if orphan_status.branches.is_empty() && orphan_status.worktrees.is_empty() {
        report.push_str("\nNo orphan branches or worktrees to clean up.\n");
        return Ok(report);
    }

    if !prompt_yes_no(
        "Delete the orphan branches and worktrees listed above? [y/N] ",
        false,
    )? {
        report.push_str("\nCleanup aborted.\n");
        return Ok(report);
    }

    let summary = cleanup_orphans(project_root, &orphan_status)?;
    report.push_str("\n== Cleanup ==\n");
    report.push_str(&format!(
        "removed_branches: {}\nremoved_worktrees: {}\n",
        summary.branches_removed, summary.worktrees_removed
    ));
    Ok(report)
}

pub fn build_report(project_root: &Path) -> Result<String> {
    let launch_state = load_launch_state(&launch_state_path(project_root))?;
    let daemon_state = load_daemon_state(&super::daemon_state_path(project_root))?;
    let team_config = load_team_config(project_root)?;
    let members = match &team_config {
        Some(config) => hierarchy::resolve_hierarchy(config)?,
        None => Vec::new(),
    };
    let resume =
        build_resume_eligibility(project_root, team_config.as_ref(), &members, &launch_state);
    let worktrees = build_worktree_statuses(project_root, &members);
    let board_git_checks = build_board_git_checks(project_root);
    let performance_checks = build_performance_checks(project_root);
    let log_sizes = vec![
        LogSize {
            name: "daemon.log",
            bytes: file_size(&project_root.join(".batty").join("daemon.log")),
        },
        LogSize {
            name: "orchestrator.log",
            bytes: file_size(&project_root.join(".batty").join("orchestrator.log")),
        },
    ];

    Ok(render_report(
        project_root,
        launch_state.as_ref(),
        daemon_state.as_ref(),
        &resume,
        &worktrees,
        &board_git_checks,
        &performance_checks,
        &log_sizes,
    ))
}

fn render_report(
    project_root: &Path,
    launch_state: Option<&HashMap<String, LaunchIdentityRecord>>,
    daemon_state: Option<&DoctorDaemonState>,
    resume: &[ResumeEligibility],
    worktrees: &[WorktreeStatus],
    board_git_checks: &[CheckLine],
    performance_checks: &[CheckLine],
    log_sizes: &[LogSize],
) -> String {
    let mut out = String::new();
    out.push_str(&format!("Batty doctor for {}\n\n", project_root.display()));

    out.push_str("== Launch State ==\n");
    match launch_state {
        Some(state) if !state.is_empty() => {
            let mut names: Vec<_> = state.keys().cloned().collect();
            names.sort();
            for name in names {
                let identity = &state[&name];
                out.push_str(&format!(
                    "{}: agent={}, prompt_hash={}, session_id={}\n",
                    name,
                    identity.agent,
                    short_prompt_hash(&identity.prompt),
                    identity.session_id.as_deref().unwrap_or("-"),
                ));
            }
        }
        _ => out.push_str("(missing)\n"),
    }
    out.push('\n');

    out.push_str("== Daemon State ==\n");
    match daemon_state {
        Some(state) => {
            out.push_str(&format!("clean_shutdown: {}\n", state.clean_shutdown));
            if state.states.is_empty() {
                out.push_str("member_states: (none)\n");
            } else {
                let mut names: Vec<_> = state.states.keys().cloned().collect();
                names.sort();
                out.push_str("member_states:\n");
                for name in names {
                    out.push_str(&format!("  {}: {:?}\n", name, state.states[&name]));
                }
            }
            if state.active_tasks.is_empty() {
                out.push_str("active_tasks: (none)\n");
            } else {
                let mut names: Vec<_> = state.active_tasks.keys().cloned().collect();
                names.sort();
                out.push_str("active_tasks:\n");
                for name in names {
                    out.push_str(&format!("  {}: #{}\n", name, state.active_tasks[&name]));
                }
            }
        }
        None => out.push_str("(missing)\n"),
    }
    out.push('\n');

    out.push_str("== Resume Eligibility ==\n");
    if resume.is_empty() {
        out.push_str("(no team config or members)\n");
    } else {
        for item in resume {
            out.push_str(&format!(
                "{}: eligible={} reason={} stored_hash={} current_hash={} session_id={}\n",
                item.member,
                item.eligible,
                item.reason,
                item.stored_prompt_hash.as_deref().unwrap_or("-"),
                item.current_prompt_hash.as_deref().unwrap_or("-"),
                item.session_id.as_deref().unwrap_or("-"),
            ));
        }
    }
    out.push('\n');

    out.push_str("== Worktree Status ==\n");
    if worktrees.is_empty() {
        out.push_str("(no engineers)\n");
    } else {
        for status in worktrees {
            let dirty = match status.dirty {
                Some(true) => "dirty",
                Some(false) => "clean",
                None => "missing",
            };
            out.push_str(&format!(
                "{}: path={} branch={} status={}\n",
                status.member,
                status.path.display(),
                status.branch.as_deref().unwrap_or("-"),
                dirty,
            ));
        }
    }
    out.push('\n');

    out.push_str("== Board-Git Consistency ==\n");
    if board_git_checks.is_empty() {
        out.push_str("PASS: no active board tasks or git consistency issues detected\n");
    } else {
        for line in board_git_checks {
            out.push_str(&format!(
                "{}: {}\n",
                match line.level {
                    CheckLevel::Pass => "PASS",
                    CheckLevel::Warn => "WARN",
                    CheckLevel::Fail => "FAIL",
                },
                line.message
            ));
        }
    }
    out.push('\n');

    out.push_str("== Performance Regression ==\n");
    for line in performance_checks {
        out.push_str(&format!(
            "{}: {}\n",
            match line.level {
                CheckLevel::Pass => "PASS",
                CheckLevel::Warn => "WARN",
                CheckLevel::Fail => "FAIL",
            },
            line.message
        ));
    }
    out.push('\n');

    out.push_str("== Log Sizes ==\n");
    for log in log_sizes {
        match log.bytes {
            Some(bytes) => out.push_str(&format!("{}: {} bytes\n", log.name, bytes)),
            None => out.push_str(&format!("{}: missing\n", log.name)),
        }
    }

    out
}

fn build_resume_eligibility(
    project_root: &Path,
    team_config: Option<&TeamConfig>,
    members: &[MemberInstance],
    launch_state: &Option<HashMap<String, LaunchIdentityRecord>>,
) -> Vec<ResumeEligibility> {
    let Some(launch_state) = launch_state.as_ref() else {
        return members
            .iter()
            .map(|member| ResumeEligibility {
                member: member.name.clone(),
                eligible: false,
                reason: "no_launch_state".to_string(),
                stored_prompt_hash: None,
                current_prompt_hash: None,
                session_id: None,
            })
            .collect();
    };

    let config_dir = super::team_config_dir(project_root);
    members
        .iter()
        .map(|member| {
            let Some(stored) = launch_state.get(&member.name) else {
                return ResumeEligibility {
                    member: member.name.clone(),
                    eligible: false,
                    reason: "missing_member_launch_state".to_string(),
                    stored_prompt_hash: None,
                    current_prompt_hash: team_config
                        .map(|_| short_prompt_hash(&current_prompt(member, &config_dir))),
                    session_id: None,
                };
            };

            let current_prompt = team_config
                .map(|_| current_prompt(member, &config_dir))
                .unwrap_or_default();
            let current_agent = canonical_agent_name(member.agent.as_deref().unwrap_or("claude"));
            let prompt_matches = team_config.is_some() && stored.prompt == current_prompt;
            let agent_matches = stored.agent == current_agent;
            let session_ok = if stored.agent == "claude-code" {
                stored
                    .session_id
                    .as_deref()
                    .is_some_and(claude_session_id_exists)
            } else {
                true
            };
            let eligible = agent_matches && prompt_matches && session_ok;
            let reason = if !agent_matches {
                "agent_changed"
            } else if team_config.is_none() {
                "missing_team_config"
            } else if !prompt_matches {
                "prompt_changed"
            } else if !session_ok {
                "session_missing"
            } else {
                "ok"
            };

            ResumeEligibility {
                member: member.name.clone(),
                eligible,
                reason: reason.to_string(),
                stored_prompt_hash: Some(short_prompt_hash(&stored.prompt)),
                current_prompt_hash: team_config.map(|_| short_prompt_hash(&current_prompt)),
                session_id: stored.session_id.clone(),
            }
        })
        .collect()
}

fn build_worktree_statuses(project_root: &Path, members: &[MemberInstance]) -> Vec<WorktreeStatus> {
    members
        .iter()
        .filter(|member| member.role_type == RoleType::Engineer)
        .map(|member| {
            let path = if member.use_worktrees {
                project_root
                    .join(".batty")
                    .join("worktrees")
                    .join(&member.name)
            } else {
                project_root.to_path_buf()
            };

            let branch = git_output(&path, &["branch", "--show-current"]);
            let dirty = if path.exists() {
                git_output(&path, &["status", "--porcelain"]).map(|output| !output.is_empty())
            } else {
                None
            };

            WorktreeStatus {
                member: member.name.clone(),
                path,
                branch,
                dirty,
            }
        })
        .collect()
}

fn build_board_git_checks(project_root: &Path) -> Vec<CheckLine> {
    let tasks_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    if !tasks_dir.exists() {
        return vec![check_line(
            CheckLevel::Pass,
            "board tasks directory missing; nothing to verify",
        )];
    }

    let tasks = match load_tasks_from_dir(&tasks_dir) {
        Ok(tasks) => tasks,
        Err(error) => {
            return vec![check_line(
                CheckLevel::Fail,
                format!("failed to load board tasks: {error:#}"),
            )];
        }
    };
    let active_tasks: Vec<_> = tasks
        .iter()
        .filter(|task| matches!(task.status.as_str(), "in-progress" | "review"))
        .collect();

    if active_tasks.is_empty() {
        return vec![check_line(
            CheckLevel::Pass,
            "no in-progress or review tasks on the board",
        )];
    }

    if git_cmd::rev_parse_toplevel(project_root).is_err() {
        return vec![check_line(
            CheckLevel::Fail,
            "git state unavailable; cannot cross-check board metadata",
        )];
    }

    let active_targets = active_task_targets(project_root, &active_tasks);
    let mut lines = Vec::new();
    lines.extend(branch_consistency_checks(project_root, &active_tasks));
    lines.extend(worktree_consistency_checks(project_root, &active_tasks));
    lines.extend(orphan_branch_checks(project_root, &active_targets));
    lines.extend(orphan_worktree_checks(project_root, &active_targets));
    lines
}

fn build_performance_checks(project_root: &Path) -> Vec<CheckLine> {
    let log_path = project_root.join(".batty").join("test_timing.jsonl");
    let records = match read_test_timing_log(&log_path) {
        Ok(records) => records,
        Err(error) => {
            return vec![check_line(
                CheckLevel::Fail,
                format!("failed to read test timing history: {error:#}"),
            )];
        }
    };

    let Some(latest) = records.last() else {
        return vec![check_line(
            CheckLevel::Pass,
            "no merge test timing history recorded yet",
        )];
    };

    match latest.rolling_average_ms {
        None => vec![check_line(
            CheckLevel::Pass,
            format!(
                "merge timing history has {} samples; need 6 successful merges before regression detection activates",
                records.len()
            ),
        )],
        Some(rolling_average_ms) if latest.regression_detected => vec![check_line(
            CheckLevel::Warn,
            format!(
                "latest merge test runtime regressed on task #{}: {} ms vs rolling 5-merge average {} ms ({}% slower)",
                latest.task_id,
                latest.duration_ms,
                rolling_average_ms,
                latest.regression_pct.unwrap_or_default()
            ),
        )],
        Some(rolling_average_ms) => vec![check_line(
            CheckLevel::Pass,
            format!(
                "latest merge test runtime is {} ms vs rolling 5-merge average {} ms",
                latest.duration_ms, rolling_average_ms
            ),
        )],
    }
}

fn branch_consistency_checks(project_root: &Path, tasks: &[&crate::task::Task]) -> Vec<CheckLine> {
    let tasks_with_branch: Vec<_> = tasks
        .iter()
        .copied()
        .filter(|task| {
            task.branch
                .as_deref()
                .is_some_and(|branch| !branch.trim().is_empty())
        })
        .collect();

    if tasks_with_branch.is_empty() {
        return vec![check_line(
            CheckLevel::Pass,
            "no active tasks declare a branch",
        )];
    }

    let mut warnings = Vec::new();
    for task in tasks_with_branch.iter().copied() {
        let branch = task.branch.as_deref().unwrap().trim();
        match git_cmd::show_ref_exists(project_root, branch) {
            Ok(false) => warnings.push(check_line(
                CheckLevel::Warn,
                format!("task #{} declares missing branch '{branch}'", task.id),
            )),
            Ok(true) => match git_cmd::rev_list_count(project_root, &format!("main..{branch}")) {
                Ok(0) => warnings.push(check_line(
                    CheckLevel::Warn,
                    format!(
                        "task #{} branch '{branch}' has no commits ahead of main",
                        task.id
                    ),
                )),
                Ok(_) => {}
                Err(error) => warnings.push(check_line(
                    CheckLevel::Warn,
                    format!(
                        "task #{} branch '{branch}' could not be compared to main: {error}",
                        task.id
                    ),
                )),
            },
            Err(error) => warnings.push(check_line(
                CheckLevel::Warn,
                format!("task #{} branch '{branch}' lookup failed: {error}", task.id),
            )),
        }
    }

    if warnings.is_empty() {
        vec![check_line(
            CheckLevel::Pass,
            format!(
                "all {} active task branches exist and are ahead of main",
                tasks_with_branch.len()
            ),
        )]
    } else {
        warnings
    }
}

fn worktree_consistency_checks(
    project_root: &Path,
    tasks: &[&crate::task::Task],
) -> Vec<CheckLine> {
    let tasks_with_worktree: Vec<_> = tasks
        .iter()
        .copied()
        .filter(|task| {
            task.worktree_path
                .as_deref()
                .is_some_and(|path| !path.trim().is_empty())
        })
        .collect();

    if tasks_with_worktree.is_empty() {
        return vec![check_line(
            CheckLevel::Pass,
            "no active tasks declare a worktree path",
        )];
    }

    let mut warnings = Vec::new();
    for task in tasks_with_worktree.iter().copied() {
        let worktree = resolve_task_worktree(project_root, task.worktree_path.as_deref().unwrap());
        if !worktree.exists() {
            warnings.push(check_line(
                CheckLevel::Warn,
                format!(
                    "task #{} declares missing worktree '{}'",
                    task.id,
                    worktree.display()
                ),
            ));
            continue;
        }

        if let Some(expected_branch) = task.branch.as_deref() {
            match git_cmd::rev_parse_branch(&worktree) {
                Ok(current_branch) if current_branch != expected_branch => {
                    warnings.push(check_line(
                        CheckLevel::Warn,
                        format!(
                            "task #{} worktree '{}' is on branch '{}' instead of '{}'",
                            task.id,
                            worktree.display(),
                            current_branch,
                            expected_branch
                        ),
                    ));
                }
                Ok(_) => {}
                Err(error) => warnings.push(check_line(
                    CheckLevel::Warn,
                    format!(
                        "task #{} worktree '{}' branch lookup failed: {error}",
                        task.id,
                        worktree.display()
                    ),
                )),
            }
        }

        match git_cmd::status_porcelain(&worktree) {
            Ok(status) if !status.trim().is_empty() => warnings.push(check_line(
                CheckLevel::Warn,
                format!(
                    "task #{} worktree '{}' has uncommitted changes",
                    task.id,
                    worktree.display()
                ),
            )),
            Ok(_) => {}
            Err(error) => warnings.push(check_line(
                CheckLevel::Warn,
                format!(
                    "task #{} worktree '{}' status check failed: {error}",
                    task.id,
                    worktree.display()
                ),
            )),
        }
    }

    if warnings.is_empty() {
        vec![check_line(
            CheckLevel::Pass,
            format!(
                "all {} active task worktrees exist and match board metadata",
                tasks_with_worktree.len()
            ),
        )]
    } else {
        warnings
    }
}

fn orphan_branch_checks(project_root: &Path, active_targets: &ActiveTaskTargets) -> Vec<CheckLine> {
    let branches = match git_cmd::for_each_ref_branches(project_root) {
        Ok(branches) => branches,
        Err(error) => {
            return vec![check_line(
                CheckLevel::Warn,
                format!("failed to list git branches for orphan detection: {error}"),
            )];
        }
    };

    let orphans: Vec<_> = branches
        .into_iter()
        .filter(|branch| is_task_branch(branch))
        .filter(|branch| !active_targets.branches.contains(branch))
        .collect();

    if orphans.is_empty() {
        vec![check_line(
            CheckLevel::Pass,
            "no orphan task branches found",
        )]
    } else {
        orphans
            .into_iter()
            .map(|branch| {
                check_line(
                    CheckLevel::Warn,
                    format!("orphan task branch '{branch}' has no active board task"),
                )
            })
            .collect()
    }
}

fn orphan_worktree_checks(
    project_root: &Path,
    active_targets: &ActiveTaskTargets,
) -> Vec<CheckLine> {
    let worktrees = match list_worktree_dirs(project_root) {
        Ok(worktrees) => worktrees,
        Err(error) => {
            return vec![check_line(
                CheckLevel::Warn,
                format!("failed to read worktree directory for orphan detection: {error}"),
            )];
        }
    };

    if worktrees.is_empty() {
        return vec![check_line(
            CheckLevel::Pass,
            "no worktree directory exists for orphan detection",
        )];
    }

    let mut orphans = Vec::new();
    for path in worktrees {
        if active_targets.worktrees.contains(&path) {
            continue;
        }

        let Ok(branch) = git_cmd::rev_parse_branch(&path) else {
            continue;
        };
        if is_task_branch(&branch) && !active_targets.branches.contains(&branch) {
            orphans.push(check_line(
                CheckLevel::Warn,
                format!(
                    "orphan worktree '{}' is still checked out on task branch '{}'",
                    path.display(),
                    branch
                ),
            ));
        }
    }

    if orphans.is_empty() {
        vec![check_line(
            CheckLevel::Pass,
            "no orphan task worktrees found",
        )]
    } else {
        orphans
    }
}

fn active_task_targets(project_root: &Path, tasks: &[&crate::task::Task]) -> ActiveTaskTargets {
    let mut branches = HashSet::new();
    let mut worktrees = HashSet::new();

    for task in tasks {
        if let Some(branch) = task
            .branch
            .as_deref()
            .map(str::trim)
            .filter(|branch| is_task_branch(branch))
        {
            branches.insert(branch.to_string());
        } else if let Some(claimed_by) = task
            .claimed_by
            .as_deref()
            .map(str::trim)
            .filter(|name| is_engineer_name(name))
        {
            branches.insert(format!("{claimed_by}/task-{}", task.id));
        }

        if let Some(worktree_path) = task
            .worktree_path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
        {
            worktrees.insert(resolve_task_worktree(project_root, worktree_path));
        } else if let Some(claimed_by) = task
            .claimed_by
            .as_deref()
            .map(str::trim)
            .filter(|name| is_engineer_name(name))
        {
            worktrees.insert(
                project_root
                    .join(".batty")
                    .join("worktrees")
                    .join(claimed_by),
            );
        }
    }

    ActiveTaskTargets {
        branches,
        worktrees,
    }
}

fn detect_orphans(project_root: &Path) -> Result<OrphanStatus> {
    let tasks_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    if !tasks_dir.is_dir() {
        return Ok(OrphanStatus {
            branches: Vec::new(),
            worktrees: Vec::new(),
        });
    }

    let tasks = load_tasks_from_dir(&tasks_dir)?;
    let active_tasks: Vec<_> = tasks
        .iter()
        .filter(|task| matches!(task.status.as_str(), "in-progress" | "review"))
        .collect();
    let active_targets = active_task_targets(project_root, &active_tasks);

    let mut branches: Vec<_> = list_task_branches(project_root)?
        .into_iter()
        .filter(|branch| !active_targets.branches.contains(branch))
        .collect();
    branches.sort();

    let mut worktrees: Vec<_> = list_worktree_dirs(project_root)?
        .into_iter()
        .filter(|path| !active_targets.worktrees.contains(path))
        .collect();
    worktrees.sort();

    Ok(OrphanStatus {
        branches,
        worktrees,
    })
}

fn list_task_branches(project_root: &Path) -> Result<Vec<String>> {
    Ok(git_cmd::for_each_ref_branches(project_root)?
        .into_iter()
        .filter(|branch| is_task_branch(branch))
        .collect())
}

fn list_worktree_dirs(project_root: &Path) -> Result<Vec<PathBuf>> {
    let worktrees_root = project_root.join(".batty").join("worktrees");
    if !worktrees_root.is_dir() {
        return Ok(Vec::new());
    }

    let mut paths = Vec::new();
    for entry in fs::read_dir(&worktrees_root)
        .with_context(|| format!("failed to read {}", worktrees_root.display()))?
    {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            paths.push(entry.path());
        }
    }
    Ok(paths)
}

fn cleanup_orphans(project_root: &Path, orphan_status: &OrphanStatus) -> Result<CleanupSummary> {
    let mut worktrees_removed = 0usize;
    for worktree in &orphan_status.worktrees {
        git_cmd::worktree_remove(project_root, worktree, true).map_err(|error| {
            anyhow::anyhow!(
                "failed to remove worktree '{}': {error}",
                worktree.display()
            )
        })?;
        worktrees_removed += 1;
    }

    let mut branches_removed = 0usize;
    for branch in &orphan_status.branches {
        git_cmd::branch_delete(project_root, branch)
            .map_err(|error| anyhow::anyhow!("failed to delete branch '{branch}': {error}"))?;
        branches_removed += 1;
    }

    Ok(CleanupSummary {
        branches_removed,
        worktrees_removed,
    })
}

fn check_line(level: CheckLevel, message: impl Into<String>) -> CheckLine {
    CheckLine {
        level,
        message: message.into(),
    }
}

fn resolve_task_worktree(project_root: &Path, worktree_path: &str) -> PathBuf {
    let path = PathBuf::from(worktree_path);
    if path.is_absolute() {
        path
    } else {
        project_root.join(path)
    }
}

fn is_task_branch(branch: &str) -> bool {
    branch.starts_with("eng-") && branch.contains("/task-")
}

fn is_engineer_name(name: &str) -> bool {
    name.starts_with("eng-")
}

fn prompt_yes_no(msg: &str, default_yes: bool) -> Result<bool> {
    print!("{msg}");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(default_yes);
    }
    Ok(trimmed.starts_with('y') || trimmed.starts_with('Y'))
}

fn current_prompt(member: &MemberInstance, config_dir: &Path) -> String {
    let prompt_file = member.prompt.as_deref().unwrap_or(match member.role_type {
        RoleType::Architect => "architect.md",
        RoleType::Manager => "manager.md",
        RoleType::Engineer => "engineer.md",
        RoleType::User => "architect.md",
    });

    let path = config_dir.join(prompt_file);
    let content = fs::read_to_string(&path).unwrap_or_else(|_| {
        format!(
            "You are {} (role: {:?}). Work on assigned tasks.",
            member.name, member.role_type
        )
    });

    strip_nudge_section(
        &content
            .replace("{{member_name}}", &member.name)
            .replace("{{role_name}}", &member.role_name)
            .replace(
                "{{reports_to}}",
                member.reports_to.as_deref().unwrap_or("none"),
            ),
    )
}

fn strip_nudge_section(prompt: &str) -> String {
    let mut lines = Vec::new();
    let mut in_nudge = false;

    for line in prompt.lines() {
        if line.starts_with("## Nudge") {
            in_nudge = true;
            continue;
        }
        if in_nudge && line.starts_with("## ") {
            in_nudge = false;
        }
        if !in_nudge {
            lines.push(line);
        }
    }

    lines.join("\n").trim_end().to_string()
}

fn short_prompt_hash(prompt: &str) -> String {
    let digest = Sha256::digest(prompt.as_bytes());
    format!(
        "{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3]
    )
}

fn canonical_agent_name(agent_name: &str) -> String {
    match agent_name {
        "claude" | "claude-code" => "claude-code".to_string(),
        "codex" | "codex-cli" => "codex-cli".to_string(),
        _ => agent_name.to_string(),
    }
}

fn git_output(path: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn load_team_config(project_root: &Path) -> Result<Option<TeamConfig>> {
    let path = super::team_config_path(project_root);
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(TeamConfig::load(&path)?))
}

fn load_launch_state(path: &Path) -> Result<Option<HashMap<String, LaunchIdentityRecord>>> {
    load_json_file(path)
}

fn load_daemon_state(path: &Path) -> Result<Option<DoctorDaemonState>> {
    load_json_file(path)
}

fn load_json_file<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let parsed = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(parsed))
}

fn launch_state_path(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("launch-state.json")
}

fn file_size(path: &Path) -> Option<u64> {
    fs::metadata(path).ok().map(|metadata| metadata.len())
}

fn default_claude_projects_root() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
        .join(".claude")
        .join("projects")
}

fn claude_session_id_exists(session_id: &str) -> bool {
    let session_file = format!("{session_id}.jsonl");
    let Ok(entries) = fs::read_dir(default_claude_projects_root()) else {
        return false;
    };

    entries.flatten().any(|entry| {
        let path = entry.path();
        path.is_dir() && path.join(&session_file).exists()
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use super::*;

    fn write_team_config(root: &Path) {
        let team_dir = root.join(".batty").join("team_config");
        fs::create_dir_all(&team_dir).unwrap();
        fs::write(
            team_dir.join("team.yaml"),
            r#"
name: test
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: manager
    role_type: manager
    agent: codex
  - name: engineer
    role_type: engineer
    agent: codex
    use_worktrees: true
"#,
        )
        .unwrap();
        fs::write(
            team_dir.join("architect.md"),
            "Architect prompt\n## Nudge\nnudge text\n## Next\nkeep this",
        )
        .unwrap();
        fs::write(team_dir.join("manager.md"), "Manager prompt").unwrap();
        fs::write(team_dir.join("engineer.md"), "Engineer prompt").unwrap();
    }

    fn init_git_repo(root: &Path) {
        git_ok(root, &["init", "-b", "main"]);
        git_ok(root, &["config", "user.email", "batty-test@example.com"]);
        git_ok(root, &["config", "user.name", "Batty Test"]);
        fs::write(root.join("README.md"), "initial\n").unwrap();
        git_ok(root, &["add", "README.md"]);
        git_ok(root, &["commit", "-m", "initial"]);
    }

    fn git(dir: &Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap_or_else(|error| panic!("git {:?} failed to run: {error}", args))
    }

    fn git_ok(dir: &Path, args: &[&str]) {
        let output = git(dir, args);
        assert!(
            output.status.success(),
            "git {:?} failed: stdout={} stderr={}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn write_board_task(
        root: &Path,
        id: u32,
        status: &str,
        branch: Option<&str>,
        worktree_path: Option<&str>,
    ) {
        let tasks_dir = root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        let mut content = format!(
            "---\nid: {id}\ntitle: Task {id}\nstatus: {status}\npriority: medium\nclass: standard\n"
        );
        if let Some(branch) = branch {
            content.push_str(&format!("branch: {branch}\n"));
        }
        if let Some(worktree_path) = worktree_path {
            content.push_str(&format!("worktree_path: {worktree_path}\n"));
        }
        content.push_str("---\n\nTask body.\n");
        fs::write(tasks_dir.join(format!("{id:03}-task-{id}.md")), content).unwrap();
    }

    fn write_claimed_task(root: &Path, id: u32, status: &str, claimed_by: &str) {
        let tasks_dir = root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        let content = format!(
            "---\nid: {id}\ntitle: Task {id}\nstatus: {status}\npriority: medium\nclaimed_by: {claimed_by}\nclass: standard\n---\n\nTask body.\n"
        );
        fs::write(tasks_dir.join(format!("{id:03}-task-{id}.md")), content).unwrap();
    }

    #[test]
    fn test_doctor_parses_launch_state() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("launch-state.json");
        fs::write(
            &path,
            r#"{"manager":{"agent":"codex-cli","prompt":"Manager prompt","session_id":null}}"#,
        )
        .unwrap();

        let parsed = load_launch_state(&path).unwrap().unwrap();
        assert_eq!(parsed["manager"].agent, "codex-cli");
        assert_eq!(parsed["manager"].prompt, "Manager prompt");
        assert_eq!(parsed["manager"].session_id, None);
    }

    #[test]
    fn test_doctor_parses_daemon_state() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon-state.json");
        fs::write(
            &path,
            r#"{"clean_shutdown":true,"saved_at":10,"states":{"manager":"idle"},"active_tasks":{"eng-1":42}}"#,
        )
        .unwrap();

        let parsed = load_daemon_state(&path).unwrap().unwrap();
        assert!(parsed.clean_shutdown);
        assert_eq!(parsed.states["manager"], MemberState::Idle);
        assert_eq!(parsed.active_tasks["eng-1"], 42);
    }

    #[test]
    fn test_doctor_formats_output() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        write_team_config(tmp.path());
        fs::create_dir_all(tmp.path().join(".batty").join("worktrees").join("engineer")).unwrap();
        let launch_state = HashMap::from([
            (
                "architect".to_string(),
                LaunchIdentityRecord {
                    agent: "claude-code".to_string(),
                    prompt: strip_nudge_section(
                        "Architect prompt\n## Nudge\nnudge text\n## Next\nkeep this",
                    ),
                    session_id: Some("missing".to_string()),
                },
            ),
            (
                "manager".to_string(),
                LaunchIdentityRecord {
                    agent: "codex-cli".to_string(),
                    prompt: "Manager prompt".to_string(),
                    session_id: None,
                },
            ),
            (
                "engineer".to_string(),
                LaunchIdentityRecord {
                    agent: "codex-cli".to_string(),
                    prompt: "Engineer prompt".to_string(),
                    session_id: None,
                },
            ),
        ]);
        fs::write(
            launch_state_path(tmp.path()),
            serde_json::to_string(&launch_state).unwrap(),
        )
        .unwrap();
        fs::write(
            super::super::daemon_state_path(tmp.path()),
            r#"{"clean_shutdown":false,"saved_at":10,"states":{"architect":"working","manager":"idle"},"active_tasks":{"engineer":58}}"#,
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join(".batty")).unwrap();
        fs::write(tmp.path().join(".batty").join("daemon.log"), "daemon").unwrap();
        fs::write(
            tmp.path().join(".batty").join("orchestrator.log"),
            "orchestrator",
        )
        .unwrap();

        let report = build_report(tmp.path()).unwrap();

        assert!(report.contains("== Launch State =="));
        assert!(report.contains("== Daemon State =="));
        assert!(report.contains("== Resume Eligibility =="));
        assert!(report.contains("== Worktree Status =="));
        assert!(report.contains("== Board-Git Consistency =="));
        assert!(report.contains("== Performance Regression =="));
        assert!(report.contains("== Log Sizes =="));
        assert!(report.contains("manager: agent=codex-cli"));
        assert!(report.contains("clean_shutdown: false"));
        assert!(report.contains("path="));
        assert!(report.contains("status=missing"));
        assert!(report.contains("daemon.log: 6 bytes"));
    }

    #[test]
    fn test_doctor_handles_missing_files() {
        let tmp = tempfile::tempdir().unwrap();

        let report = build_report(tmp.path()).unwrap();

        assert!(report.contains("== Launch State =="));
        assert!(report.contains("(missing)"));
        assert!(report.contains("== Daemon State =="));
        assert!(report.contains("== Resume Eligibility =="));
        assert!(report.contains("== Board-Git Consistency =="));
        assert!(report.contains("== Performance Regression =="));
        assert!(report.contains("(no team config or members)"));
        assert!(report.contains("daemon.log: missing"));
        assert!(report.contains("orchestrator.log: missing"));
    }

    #[test]
    fn doctor_board_clean_state_reports_passes() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        write_board_task(
            tmp.path(),
            69,
            "in-progress",
            Some("eng-1-3/task-69"),
            Some(".batty/worktrees/eng-1-3"),
        );

        let worktree_path = tmp.path().join(".batty").join("worktrees").join("eng-1-3");
        git_ok(
            tmp.path(),
            &[
                "worktree",
                "add",
                "-b",
                "eng-1-3/task-69",
                worktree_path.to_string_lossy().as_ref(),
                "main",
            ],
        );
        fs::write(worktree_path.join("feature.txt"), "feature\n").unwrap();
        git_ok(&worktree_path, &["add", "feature.txt"]);
        git_ok(&worktree_path, &["commit", "-m", "task work"]);

        let report = build_report(tmp.path()).unwrap();

        assert!(report.contains("== Board-Git Consistency =="));
        assert!(report.contains("PASS: all 1 active task branches exist and are ahead of main"));
        assert!(
            report.contains("PASS: all 1 active task worktrees exist and match board metadata")
        );
        assert!(report.contains("PASS: no orphan task branches found"));
        assert!(report.contains("PASS: no orphan task worktrees found"));
    }

    #[test]
    fn doctor_performance_regression_warns_on_latest_slow_merge() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        fs::create_dir_all(tmp.path().join(".batty")).unwrap();
        fs::write(
            tmp.path().join(".batty").join("test_timing.jsonl"),
            [
                r#"{"task_id":1,"engineer":"eng-1","branch":"eng-1/task-1","measured_at":1,"duration_ms":1000,"rolling_average_ms":null,"regression_pct":null,"regression_detected":false}"#,
                r#"{"task_id":2,"engineer":"eng-1","branch":"eng-1/task-2","measured_at":2,"duration_ms":1000,"rolling_average_ms":null,"regression_pct":null,"regression_detected":false}"#,
                r#"{"task_id":3,"engineer":"eng-1","branch":"eng-1/task-3","measured_at":3,"duration_ms":1000,"rolling_average_ms":null,"regression_pct":null,"regression_detected":false}"#,
                r#"{"task_id":4,"engineer":"eng-1","branch":"eng-1/task-4","measured_at":4,"duration_ms":1000,"rolling_average_ms":null,"regression_pct":null,"regression_detected":false}"#,
                r#"{"task_id":5,"engineer":"eng-1","branch":"eng-1/task-5","measured_at":5,"duration_ms":1000,"rolling_average_ms":null,"regression_pct":null,"regression_detected":false}"#,
                r#"{"task_id":6,"engineer":"eng-1","branch":"eng-1/task-6","measured_at":6,"duration_ms":1300,"rolling_average_ms":1000,"regression_pct":30,"regression_detected":true}"#,
            ]
            .join("\n"),
        )
        .unwrap();

        let report = build_report(tmp.path()).unwrap();

        assert!(report.contains("== Performance Regression =="));
        assert!(report.contains("WARN: latest merge test runtime regressed on task #6"));
        assert!(report.contains("1300 ms"));
        assert!(report.contains("1000 ms"));
        assert!(report.contains("30% slower"));
    }

    #[test]
    fn doctor_performance_regression_reports_clean_baseline() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        fs::create_dir_all(tmp.path().join(".batty")).unwrap();
        fs::write(
            tmp.path().join(".batty").join("test_timing.jsonl"),
            [
                r#"{"task_id":1,"engineer":"eng-1","branch":"eng-1/task-1","measured_at":1,"duration_ms":1000,"rolling_average_ms":null,"regression_pct":null,"regression_detected":false}"#,
                r#"{"task_id":2,"engineer":"eng-1","branch":"eng-1/task-2","measured_at":2,"duration_ms":1000,"rolling_average_ms":null,"regression_pct":null,"regression_detected":false}"#,
                r#"{"task_id":3,"engineer":"eng-1","branch":"eng-1/task-3","measured_at":3,"duration_ms":1000,"rolling_average_ms":null,"regression_pct":null,"regression_detected":false}"#,
                r#"{"task_id":4,"engineer":"eng-1","branch":"eng-1/task-4","measured_at":4,"duration_ms":1000,"rolling_average_ms":null,"regression_pct":null,"regression_detected":false}"#,
                r#"{"task_id":5,"engineer":"eng-1","branch":"eng-1/task-5","measured_at":5,"duration_ms":1000,"rolling_average_ms":null,"regression_pct":null,"regression_detected":false}"#,
                r#"{"task_id":6,"engineer":"eng-1","branch":"eng-1/task-6","measured_at":6,"duration_ms":1100,"rolling_average_ms":1000,"regression_pct":10,"regression_detected":false}"#,
            ]
            .join("\n"),
        )
        .unwrap();

        let report = build_report(tmp.path()).unwrap();

        assert!(report.contains(
            "PASS: latest merge test runtime is 1100 ms vs rolling 5-merge average 1000 ms"
        ));
    }

    #[test]
    fn doctor_board_warns_on_missing_branch_and_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        write_board_task(
            tmp.path(),
            69,
            "review",
            Some("eng-1-3/task-69"),
            Some(".batty/worktrees/eng-1-3"),
        );

        let report = build_report(tmp.path()).unwrap();

        assert!(report.contains("WARN: task #69 declares missing branch 'eng-1-3/task-69'"));
        assert!(report.contains("WARN: task #69 declares missing worktree"));
    }

    #[test]
    fn doctor_board_warns_on_dirty_and_orphaned_git_state() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        write_board_task(
            tmp.path(),
            69,
            "in-progress",
            Some("eng-1-3/task-69"),
            Some(".batty/worktrees/eng-1-3"),
        );

        let active_worktree = tmp.path().join(".batty").join("worktrees").join("eng-1-3");
        git_ok(
            tmp.path(),
            &[
                "worktree",
                "add",
                "-b",
                "eng-1-3/task-69",
                active_worktree.to_string_lossy().as_ref(),
                "main",
            ],
        );
        fs::write(active_worktree.join("feature.txt"), "feature\n").unwrap();
        git_ok(&active_worktree, &["add", "feature.txt"]);
        git_ok(&active_worktree, &["commit", "-m", "task work"]);
        fs::write(active_worktree.join("dirty.txt"), "dirty\n").unwrap();

        git_ok(tmp.path(), &["checkout", "-b", "eng-9/task-99"]);
        fs::write(tmp.path().join("orphan.txt"), "orphan\n").unwrap();
        git_ok(tmp.path(), &["add", "orphan.txt"]);
        git_ok(tmp.path(), &["commit", "-m", "orphan branch"]);
        git_ok(tmp.path(), &["checkout", "main"]);

        let orphan_worktree = tmp.path().join(".batty").join("worktrees").join("orphan");
        git_ok(
            tmp.path(),
            &[
                "worktree",
                "add",
                "-b",
                "eng-9/task-100",
                orphan_worktree.to_string_lossy().as_ref(),
                "main",
            ],
        );

        let report = build_report(tmp.path()).unwrap();

        assert!(report.contains("WARN: task #69 worktree"));
        assert!(report.contains("has uncommitted changes"));
        assert!(
            report.contains("WARN: orphan task branch 'eng-9/task-99' has no active board task")
        );
        assert!(report.contains("WARN: orphan worktree"));
        assert!(report.contains("eng-9/task-100"));
    }

    #[test]
    fn detect_orphans_ignores_active_claimed_task_targets() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        write_claimed_task(tmp.path(), 72, "in-progress", "eng-1");

        git_ok(tmp.path(), &["branch", "eng-1/task-72"]);
        fs::create_dir_all(tmp.path().join(".batty").join("worktrees").join("eng-1")).unwrap();

        let orphan_worktree = tmp.path().join(".batty").join("worktrees").join("eng-9");
        git_ok(
            tmp.path(),
            &[
                "worktree",
                "add",
                "-b",
                "eng-9/task-9",
                orphan_worktree.to_string_lossy().as_ref(),
                "main",
            ],
        );

        let orphans = detect_orphans(tmp.path()).unwrap();
        assert_eq!(orphans.branches, vec!["eng-9/task-9".to_string()]);
        assert_eq!(orphans.worktrees, vec![orphan_worktree]);
    }

    #[test]
    fn cleanup_orphans_removes_detected_branch_and_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        write_claimed_task(tmp.path(), 72, "review", "eng-1");

        git_ok(tmp.path(), &["branch", "eng-1/task-72"]);
        fs::create_dir_all(tmp.path().join(".batty").join("worktrees").join("eng-1")).unwrap();

        let orphan_worktree = tmp.path().join(".batty").join("worktrees").join("eng-9");
        git_ok(
            tmp.path(),
            &[
                "worktree",
                "add",
                "-b",
                "eng-9/task-9",
                orphan_worktree.to_string_lossy().as_ref(),
                "main",
            ],
        );

        let summary = cleanup_orphans(tmp.path(), &detect_orphans(tmp.path()).unwrap()).unwrap();

        assert_eq!(summary.branches_removed, 1);
        assert_eq!(summary.worktrees_removed, 1);
        assert!(!orphan_worktree.exists());
        assert_eq!(
            list_task_branches(tmp.path()).unwrap(),
            vec!["eng-1/task-72".to_string()]
        );
    }
}
