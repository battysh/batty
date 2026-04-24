use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::super::artifact::read_test_timing_log;
use super::super::config::{RoleType, WorkspaceType};
use super::super::git_cmd;
use super::super::hierarchy::MemberInstance;
use super::util::{
    canonical_agent_name, check_line, claude_session_id_exists, current_prompt, git_output,
    is_engineer_name, is_task_branch, resolve_task_worktree, short_prompt_hash,
};
use super::{
    ActiveTaskTargets, CheckLevel, CheckLine, LaunchIdentityRecord, ResumeEligibility, TeamConfig,
    WorktreeStatus,
};
use crate::task::load_tasks_from_dir;
use crate::team::workspace::engineer_workspace_dir;

pub(super) fn build_resume_eligibility(
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

    let config_dir = super::super::team_config_dir(project_root);
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

pub(super) fn build_worktree_statuses(
    project_root: &Path,
    workspace_type: WorkspaceType,
    members: &[MemberInstance],
) -> Vec<WorktreeStatus> {
    members
        .iter()
        .filter(|member| member.role_type == RoleType::Engineer)
        .flat_map(|member| {
            let path = if member.use_worktrees {
                engineer_workspace_dir(project_root, workspace_type, &member.name)
            } else {
                project_root.to_path_buf()
            };

            // Multi-repo mode: worktree root isn't a git repo, it holds
            // per-package git worktrees underneath. Emit one status row per
            // sub-repo with the member name suffixed (e.g. "eng-1-3:Foo").
            // Single-repo mode falls through unchanged.
            if path.exists() && !git_cmd::is_git_repo(&path) {
                let sub_repos = git_cmd::discover_sub_repos(&path);
                if !sub_repos.is_empty() {
                    return sub_repos
                        .into_iter()
                        .map(|repo| {
                            let repo_name = repo
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            let branch = git_output(&repo, &["branch", "--show-current"]);
                            let dirty = git_output(&repo, &["status", "--porcelain"])
                                .map(|output| !output.is_empty());
                            WorktreeStatus {
                                member: format!("{}:{}", member.name, repo_name),
                                path: repo,
                                branch,
                                dirty,
                            }
                        })
                        .collect::<Vec<_>>();
                }
            }

            let branch = git_output(&path, &["branch", "--show-current"]);
            let dirty = if path.exists() {
                git_output(&path, &["status", "--porcelain"]).map(|output| !output.is_empty())
            } else {
                None
            };

            vec![WorktreeStatus {
                member: member.name.clone(),
                path,
                branch,
                dirty,
            }]
        })
        .collect()
}

pub(super) fn build_board_git_checks(project_root: &Path) -> Vec<CheckLine> {
    let tasks_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    let preserved_lane_checks = build_preserved_lane_checks(project_root);
    if !tasks_dir.exists() {
        let mut lines = vec![check_line(
            CheckLevel::Pass,
            "board tasks directory missing; nothing to verify",
        )];
        lines.extend(preserved_lane_checks);
        return lines;
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
        let mut lines = vec![check_line(
            CheckLevel::Pass,
            "no in-progress or review tasks on the board",
        )];
        lines.extend(preserved_lane_checks);
        return lines;
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
    lines.extend(preserved_lane_checks);
    lines
}

fn build_preserved_lane_checks(project_root: &Path) -> Vec<CheckLine> {
    crate::team::checkpoint::list_preserved_lane_records(project_root)
        .into_iter()
        .filter(|record| preserved_lane_record_is_current(project_root, record))
        .map(|record| check_line(CheckLevel::Pass, record.doctor_check_line()))
        .collect()
}

fn preserved_lane_record_is_current(
    project_root: &Path,
    record: &crate::team::checkpoint::PreservedLaneRecord,
) -> bool {
    let worktree_dir = project_root
        .join(".batty")
        .join("worktrees")
        .join(&record.role);
    if !worktree_dir.is_dir() {
        return false;
    }

    let current_branch = git_output(&worktree_dir, &["branch", "--show-current"]);
    if current_branch.as_deref() != Some(record.target_branch.as_str()) {
        return false;
    }

    !crate::team::task_loop::worktree_has_user_changes(&worktree_dir).unwrap_or(false)
}

pub(super) fn build_board_dependency_graph(project_root: &Path) -> Vec<String> {
    let tasks_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    if !tasks_dir.exists() {
        return vec!["PASS: board tasks directory missing; nothing to visualize".to_string()];
    }

    let tasks = match load_tasks_from_dir(&tasks_dir) {
        Ok(tasks) => tasks,
        Err(error) => return vec![format!("FAIL: failed to load board tasks: {error:#}")],
    };
    if tasks.is_empty() {
        return vec!["PASS: no board tasks found".to_string()];
    }

    let mut tasks_with_dependencies: Vec<_> = tasks
        .iter()
        .filter(|task| !task.depends_on.is_empty())
        .collect();
    if tasks_with_dependencies.is_empty() {
        return vec!["PASS: no task dependencies declared".to_string()];
    }

    tasks_with_dependencies.sort_by_key(|task| task.id);
    let task_by_id: HashMap<u32, &crate::task::Task> =
        tasks.iter().map(|task| (task.id, task)).collect();

    let mut lines = Vec::new();
    for task in tasks_with_dependencies {
        lines.push(format!("#{} [{}] {}", task.id, task.status, task.title));
        for dep_id in &task.depends_on {
            match task_by_id.get(dep_id) {
                Some(dependency) => lines.push(format!(
                    "  -> #{} [{}] {} ({})",
                    dependency.id,
                    dependency.status,
                    dependency.title,
                    if dependency_satisfied(dependency) {
                        "satisfied"
                    } else {
                        "blocking"
                    }
                )),
                None => lines.push(format!("  -> #{} [missing] (blocking)", dep_id)),
            }
        }
    }

    let cycles = find_dependency_cycles(&task_by_id);
    if !cycles.is_empty() {
        lines.push("Circular dependencies:".to_string());
        for cycle in cycles {
            lines.push(format!(
                "  WARN: {}",
                cycle
                    .iter()
                    .map(|task_id| format!("#{task_id}"))
                    .collect::<Vec<_>>()
                    .join(" -> ")
            ));
        }
    }

    lines
}

fn dependency_satisfied(task: &crate::task::Task) -> bool {
    matches!(task.status.as_str(), "done" | "archived")
}

fn find_dependency_cycles(task_by_id: &HashMap<u32, &crate::task::Task>) -> Vec<Vec<u32>> {
    let mut cycle_keys = HashSet::new();
    let mut cycles = Vec::new();
    let mut task_ids: Vec<_> = task_by_id.keys().copied().collect();
    task_ids.sort_unstable();

    for task_id in task_ids {
        let mut path = Vec::new();
        find_dependency_cycles_from(task_id, task_by_id, &mut path, &mut cycle_keys, &mut cycles);
    }

    cycles.sort();
    cycles
}

fn find_dependency_cycles_from(
    task_id: u32,
    task_by_id: &HashMap<u32, &crate::task::Task>,
    path: &mut Vec<u32>,
    cycle_keys: &mut HashSet<String>,
    cycles: &mut Vec<Vec<u32>>,
) {
    let Some(task) = task_by_id.get(&task_id) else {
        return;
    };

    path.push(task_id);
    for dep_id in &task.depends_on {
        if let Some(position) = path.iter().position(|seen| seen == dep_id) {
            let cycle = canonicalize_cycle(&path[position..]);
            let key = cycle
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join("->");
            if cycle_keys.insert(key) {
                cycles.push(cycle);
            }
            continue;
        }
        if task_by_id.contains_key(dep_id) {
            find_dependency_cycles_from(*dep_id, task_by_id, path, cycle_keys, cycles);
        }
    }
    path.pop();
}

fn canonicalize_cycle(cycle: &[u32]) -> Vec<u32> {
    if cycle.is_empty() {
        return Vec::new();
    }

    let mut best = cycle.to_vec();
    for idx in 1..cycle.len() {
        let rotated = cycle[idx..]
            .iter()
            .chain(cycle[..idx].iter())
            .copied()
            .collect::<Vec<_>>();
        if rotated < best {
            best = rotated;
        }
    }
    best.push(best[0]);
    best
}

pub(super) fn build_performance_checks(project_root: &Path) -> Vec<CheckLine> {
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
    let worktrees = match super::cleanup::list_worktree_dirs(project_root) {
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

pub(super) fn active_task_targets(
    project_root: &Path,
    tasks: &[&crate::task::Task],
) -> ActiveTaskTargets {
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
            branches.insert(format!("{claimed_by}/{}", task.id));
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::super::LaunchIdentityRecord;
    use super::super::util::{load_team_config, strip_nudge_section};
    use super::*;
    use crate::team::config::RoleType;
    use crate::team::hierarchy::{self, MemberInstance};

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

    fn git_ok(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap_or_else(|error| panic!("git {:?} failed to run: {error}", args));
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

    fn write_dependency_task(root: &Path, id: u32, title: &str, status: &str, depends_on: &[u32]) {
        let tasks_dir = root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        let mut content = format!(
            "---\nid: {id}\ntitle: {title}\nstatus: {status}\npriority: medium\nclass: standard\n"
        );
        if !depends_on.is_empty() {
            content.push_str("depends_on:\n");
            for dep_id in depends_on {
                content.push_str(&format!("  - {dep_id}\n"));
            }
        }
        content.push_str("---\n\nTask body.\n");
        fs::write(tasks_dir.join(format!("{id:03}-task-{id}.md")), content).unwrap();
    }

    #[test]
    fn build_resume_eligibility_reports_missing_launch_state() {
        let tmp = tempfile::tempdir().unwrap();
        write_team_config(tmp.path());
        let members =
            hierarchy::resolve_hierarchy(&load_team_config(tmp.path()).unwrap().unwrap()).unwrap();

        let resume = build_resume_eligibility(
            tmp.path(),
            load_team_config(tmp.path()).unwrap().as_ref(),
            &members,
            &None,
        );

        assert_eq!(resume.len(), 3);
        assert!(resume.iter().all(|item| !item.eligible));
        assert!(resume.iter().all(|item| item.reason == "no_launch_state"));
    }

    #[test]
    fn build_resume_eligibility_reports_missing_member_launch_state() {
        let tmp = tempfile::tempdir().unwrap();
        write_team_config(tmp.path());
        let config = load_team_config(tmp.path()).unwrap().unwrap();
        let members = hierarchy::resolve_hierarchy(&config).unwrap();
        let launch_state = Some(HashMap::from([(
            "architect".to_string(),
            LaunchIdentityRecord {
                agent: "claude-code".to_string(),
                prompt: strip_nudge_section(
                    "Architect prompt\n## Nudge\nnudge text\n## Next\nkeep this",
                ),
                session_id: None,
            },
        )]));

        let resume = build_resume_eligibility(tmp.path(), Some(&config), &members, &launch_state);

        let manager = resume.iter().find(|item| item.member == "manager").unwrap();
        assert!(!manager.eligible);
        assert_eq!(manager.reason, "missing_member_launch_state");
        assert!(manager.current_prompt_hash.is_some());
    }

    #[test]
    fn build_resume_eligibility_reports_agent_change() {
        let tmp = tempfile::tempdir().unwrap();
        write_team_config(tmp.path());
        let config = load_team_config(tmp.path()).unwrap().unwrap();
        let members = hierarchy::resolve_hierarchy(&config).unwrap();
        let launch_state = Some(HashMap::from([(
            "manager".to_string(),
            LaunchIdentityRecord {
                agent: "claude-code".to_string(),
                prompt: "Manager prompt".to_string(),
                session_id: None,
            },
        )]));

        let resume = build_resume_eligibility(tmp.path(), Some(&config), &members, &launch_state);

        let manager = resume.iter().find(|item| item.member == "manager").unwrap();
        assert!(!manager.eligible);
        assert_eq!(manager.reason, "agent_changed");
    }

    #[test]
    fn doctor_board_git_checks_pass_when_tasks_directory_missing() {
        let tmp = tempfile::tempdir().unwrap();

        let checks = build_board_git_checks(tmp.path());

        assert_eq!(
            checks,
            vec![check_line(
                CheckLevel::Pass,
                "board tasks directory missing; nothing to verify",
            )]
        );
    }

    #[test]
    fn doctor_board_git_checks_pass_when_no_active_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        write_board_task(tmp.path(), 40, "todo", None, None);
        write_board_task(tmp.path(), 41, "done", None, None);

        let checks = build_board_git_checks(tmp.path());

        assert_eq!(
            checks,
            vec![check_line(
                CheckLevel::Pass,
                "no in-progress or review tasks on the board",
            )]
        );
    }

    #[test]
    fn doctor_board_git_checks_include_preserved_completed_lane_record() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = crate::team::test_support::init_git_repo(&tmp, "doctor-preserved-lane");
        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let base_branch = crate::team::task_loop::engineer_base_branch_name("eng-1");
        crate::team::task_loop::setup_engineer_worktree(
            &repo,
            &worktree_dir,
            &base_branch,
            &team_config_dir,
        )
        .unwrap();

        let task = crate::task::Task {
            id: 628,
            title: "done lane".to_string(),
            status: "done".to_string(),
            priority: "high".to_string(),
            assignee: None,
            claimed_by: Some("eng-1".to_string()),
            claimed_at: None,
            claim_ttl_secs: None,
            claim_expires_at: None,
            last_progress_at: None,
            claim_warning_sent_at: None,
            claim_extensions: None,
            last_output_bytes: None,
            blocked: None,
            tags: vec![],
            depends_on: vec![],
            review_owner: None,
            blocked_on: None,
            worktree_path: Some(".batty/worktrees/eng-1".to_string()),
            branch: Some("eng-1/628".to_string()),
            commit: None,
            artifacts: vec![],
            next_action: None,
            scheduled_for: None,
            cron_schedule: None,
            cron_last_run: None,
            completed: None,
            description: "done".to_string(),
            batty_config: None,
            source_path: repo
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks")
                .join("628-done.md"),
        };
        let record = crate::team::checkpoint::PreservedLaneRecord::commit(
            "eng-1",
            &task,
            "eng-1/628",
            &base_branch,
            "completed task no longer needs engineer lane",
            Some("abc123456789".to_string()),
            "def4567890abc".to_string(),
        );
        crate::team::checkpoint::write_preserved_lane_record(&repo, &record).unwrap();

        let checks = build_board_git_checks(&repo);

        assert!(checks.iter().any(|line| {
            line.level == CheckLevel::Pass
                && line
                    .message
                    .contains("eng-1 preserved completed task #628 before cleanup")
        }));
    }

    #[test]
    fn doctor_board_git_checks_fail_when_git_state_unavailable() {
        let tmp = tempfile::tempdir().unwrap();
        write_board_task(tmp.path(), 42, "in-progress", Some("eng-1/task-42"), None);

        let checks = build_board_git_checks(tmp.path());

        assert_eq!(
            checks,
            vec![check_line(
                CheckLevel::Fail,
                "git state unavailable; cannot cross-check board metadata",
            )]
        );
    }

    #[test]
    fn doctor_dependency_graph_marks_satisfied_and_blocking_deps() {
        let tmp = tempfile::tempdir().unwrap();
        write_dependency_task(tmp.path(), 10, "Completed dependency", "done", &[]);
        write_dependency_task(tmp.path(), 11, "Active dependency", "in-progress", &[]);
        write_dependency_task(tmp.path(), 12, "Consumer task", "todo", &[10, 11, 99]);

        let graph = build_board_dependency_graph(tmp.path());

        assert!(
            graph
                .iter()
                .any(|line| line.contains("#12 [todo] Consumer task"))
        );
        assert!(
            graph
                .iter()
                .any(|line| line.contains("-> #10 [done] Completed dependency (satisfied)"))
        );
        assert!(
            graph
                .iter()
                .any(|line| line.contains("-> #11 [in-progress] Active dependency (blocking)"))
        );
        assert!(
            graph
                .iter()
                .any(|line| line.contains("-> #99 [missing] (blocking)"))
        );
    }

    #[test]
    fn doctor_dependency_graph_detects_cycles() {
        let tmp = tempfile::tempdir().unwrap();
        write_dependency_task(tmp.path(), 20, "Task 20", "todo", &[21]);
        write_dependency_task(tmp.path(), 21, "Task 21", "todo", &[20]);

        let graph = build_board_dependency_graph(tmp.path());

        assert!(
            graph
                .iter()
                .any(|line| line.contains("Circular dependencies:"))
        );
        assert!(
            graph
                .iter()
                .any(|line| line.contains("WARN: #20 -> #21 -> #20"))
        );
    }

    #[test]
    fn doctor_dependency_graph_reports_no_board_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(
            tmp.path()
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks"),
        )
        .unwrap();

        let lines = build_board_dependency_graph(tmp.path());

        assert_eq!(lines, vec!["PASS: no board tasks found".to_string()]);
    }

    #[test]
    fn doctor_dependency_graph_reports_no_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        write_dependency_task(tmp.path(), 30, "Standalone", "todo", &[]);

        let lines = build_board_dependency_graph(tmp.path());

        assert_eq!(
            lines,
            vec!["PASS: no task dependencies declared".to_string()]
        );
    }

    #[test]
    fn doctor_performance_regression_reports_missing_history() {
        let tmp = tempfile::tempdir().unwrap();

        let checks = build_performance_checks(tmp.path());

        assert_eq!(
            checks,
            vec![check_line(
                CheckLevel::Pass,
                "no merge test timing history recorded yet",
            )]
        );
    }

    #[test]
    fn doctor_performance_regression_reports_insufficient_samples() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        fs::create_dir_all(tmp.path().join(".batty")).unwrap();
        fs::write(
            tmp.path().join(".batty").join("test_timing.jsonl"),
            r#"{"task_id":1,"engineer":"eng-1","branch":"eng-1/task-1","measured_at":1,"duration_ms":1000,"rolling_average_ms":null,"regression_pct":null,"regression_detected":false}"#,
        )
        .unwrap();

        let checks = build_performance_checks(tmp.path());

        assert_eq!(
            checks,
            vec![check_line(
                CheckLevel::Pass,
                "merge timing history has 1 samples; need 6 successful merges before regression detection activates",
            )]
        );
    }

    #[test]
    fn canonicalize_cycle_rotates_to_lowest_task_id() {
        let cycle = canonicalize_cycle(&[7, 9, 5]);

        assert_eq!(cycle, vec![5, 7, 9, 5]);
    }

    #[test]
    fn active_task_targets_derive_branch_and_worktree_from_claimed_engineer() {
        let tmp = tempfile::tempdir().unwrap();
        let task = crate::task::Task {
            id: 88,
            title: "Task 88".to_string(),
            status: "in-progress".to_string(),
            priority: "medium".to_string(),
            assignee: None,
            claimed_by: Some("eng-2".to_string()),
            claimed_at: None,
            claim_ttl_secs: None,
            claim_expires_at: None,
            last_progress_at: None,
            claim_warning_sent_at: None,
            claim_extensions: None,
            last_output_bytes: None,
            blocked: None,
            tags: Vec::new(),
            depends_on: Vec::new(),
            review_owner: None,
            blocked_on: None,
            branch: None,
            worktree_path: None,
            commit: None,
            artifacts: Vec::new(),
            next_action: None,
            scheduled_for: None,
            cron_schedule: None,
            cron_last_run: None,
            completed: None,
            description: "Task body.".to_string(),
            batty_config: None,
            source_path: PathBuf::from("task-88.md"),
        };

        let targets = active_task_targets(tmp.path(), &[&task]);

        assert!(targets.branches.contains("eng-2/88"));
        assert!(
            targets
                .worktrees
                .contains(&tmp.path().join(".batty").join("worktrees").join("eng-2"))
        );
    }

    #[test]
    fn build_worktree_statuses_skips_non_engineers() {
        let members = vec![
            MemberInstance {
                name: "architect".to_string(),
                role_name: "architect".to_string(),
                role_type: RoleType::Architect,
                agent: Some("claude".to_string()),
                model: None,
                prompt: None,
                posture: None,
                model_class: None,
                provider_overlay: None,
                reports_to: None,
                use_worktrees: false,
            },
            MemberInstance {
                name: "eng-1".to_string(),
                role_name: "engineer".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("codex".to_string()),
                model: None,
                prompt: None,
                posture: None,
                model_class: None,
                provider_overlay: None,
                reports_to: Some("manager".to_string()),
                use_worktrees: true,
            },
        ];
        let tmp = tempfile::tempdir().unwrap();

        let statuses = build_worktree_statuses(tmp.path(), WorkspaceType::Generic, &members);

        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].member, "eng-1");
    }

    #[test]
    fn build_worktree_statuses_fans_out_per_sub_repo_for_multi_repo_worktree() {
        // B-1(1.3): in a Brazil multi-repo workspace, the engineer's worktree
        // root is NOT a git repo — it holds per-package git worktrees. doctor
        // must emit one status per sub-repo with member-qualified name.
        use std::process::Command;

        let git_ok = Command::new("git").arg("--version").output().is_ok();
        if !git_ok {
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path();

        // Engineer worktree at <project>/.batty/worktrees/eng-1 with 2 git sub-repos.
        let worktree_root = project_root.join(".batty").join("worktrees").join("eng-1");
        std::fs::create_dir_all(&worktree_root).unwrap();

        for sub_name in ["PkgA", "PkgB"] {
            let sub = worktree_root.join(sub_name);
            std::fs::create_dir_all(&sub).unwrap();
            let _ = Command::new("git")
                .current_dir(&sub)
                .args(["init", "-q", "-b", "mainline"])
                .output();
            let _ = Command::new("git")
                .current_dir(&sub)
                .args(["config", "user.email", "t@e.x"])
                .output();
            let _ = Command::new("git")
                .current_dir(&sub)
                .args(["config", "user.name", "t"])
                .output();
            std::fs::write(sub.join("README.md"), "x\n").unwrap();
            let _ = Command::new("git")
                .current_dir(&sub)
                .args(["add", "."])
                .output();
            let _ = Command::new("git")
                .current_dir(&sub)
                .args(["commit", "-q", "-m", "init"])
                .output();
        }

        let members = vec![MemberInstance {
            name: "eng-1".to_string(),
            role_name: "engineer".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            model: None,
            prompt: None,
            posture: None,
            model_class: None,
            provider_overlay: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: true,
        }];

        let statuses = build_worktree_statuses(project_root, WorkspaceType::Generic, &members);

        // One row per sub-repo (not one row for the container with missing status).
        assert_eq!(
            statuses.len(),
            2,
            "expected 2 sub-repo rows, got {statuses:?}"
        );
        for s in &statuses {
            assert!(
                s.member.starts_with("eng-1:"),
                "member name should be qualified as 'eng-1:<repo>', got '{}'",
                s.member
            );
            // Branch should resolve (not missing like the B-1 bug report).
            assert!(
                s.branch.is_some(),
                "branch should be discovered for {}",
                s.member
            );
            // Clean sub-repo: git status --porcelain is empty; git_output
            // returns None for empty output, so dirty is None (treated as
            // "missing" in the display). Not Some(false). This matches the
            // pre-existing single-repo behavior — the B-1 fix preserves it
            // per sub-repo instead of hiding every sub-repo behind the
            // container's non-git state.
            assert!(
                !matches!(s.dirty, Some(true)),
                "fresh sub-repo should not be dirty, got {:?}",
                s.dirty
            );
        }
    }
}
