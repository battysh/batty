//! Dispatch queue population, processing, and task selection.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use anyhow::Result;
use tracing::{debug, info, warn};

use super::super::super::policy::check_wip_limit;
use super::super::super::task_loop::engineer_worktree_ready_for_dispatch;
use super::super::task_cmd::{assign_task_owners, transition_task};
use super::super::*;

/// Parse task IDs from "Blocked on:" or "Depends on:" lines in the task body.
/// Returns None if no dependency line found, Some(vec) of referenced task IDs.
fn parse_body_dependency_ids(body: &str) -> Option<Vec<u32>> {
    let lower = body.to_lowercase();
    for line in lower.lines() {
        let trimmed = line.trim().trim_start_matches('-').trim();
        if trimmed.starts_with("blocked on:") || trimmed.starts_with("depends on:") {
            let ids: Vec<u32> = trimmed
                .split('#')
                .skip(1)
                .filter_map(|s| {
                    s.chars()
                        .take_while(|c| c.is_ascii_digit())
                        .collect::<String>()
                        .parse()
                        .ok()
                })
                .collect();
            if !ids.is_empty() {
                return Some(ids);
            }
        }
    }
    None
}
use super::{DISPATCH_QUEUE_FAILURE_LIMIT, DispatchQueueEntry, dispatch_priority_rank};

impl TeamDaemon {
    pub(in super::super) fn idle_engineer_names(&self) -> Vec<String> {
        self.config
            .members
            .iter()
            .filter(|member| member.role_type == RoleType::Engineer)
            .filter(|member| self.states.get(&member.name) == Some(&MemberState::Idle))
            .map(|member| member.name.clone())
            .collect()
    }

    fn next_dispatch_task(
        &self,
        board_dir: &Path,
        queued_task_ids: &HashSet<u32>,
    ) -> Result<Option<crate::task::Task>> {
        let tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks"))?;
        let task_status_by_id: HashMap<u32, String> = tasks
            .iter()
            .map(|task| (task.id, task.status.clone()))
            .collect();

        let mut available: Vec<crate::task::Task> = tasks
            .into_iter()
            .filter(|task| matches!(task.status.as_str(), "backlog" | "todo"))
            .filter(|task| task.claimed_by.is_none())
            .filter(|task| task.blocked.is_none())
            .filter(|task| task.blocked_on.is_none())
            .filter(|task| !task.is_schedule_blocked())
            .filter(|task| !queued_task_ids.contains(&task.id))
            .filter(|task| {
                task.depends_on.iter().all(|dep_id| {
                    task_status_by_id
                        .get(dep_id)
                        .is_none_or(|status| status == "done")
                })
            })
            .collect();

        available.sort_by_key(|task| (dispatch_priority_rank(&task.priority), task.id));
        Ok(available.into_iter().next())
    }

    #[cfg(test)]
    pub(super) fn test_next_dispatch_task(
        &self,
        board_dir: &std::path::Path,
        queued: &HashSet<u32>,
    ) -> Result<Option<crate::task::Task>> {
        self.next_dispatch_task(board_dir, queued)
    }

    pub(in super::super) fn enqueue_dispatch_candidates(&mut self) -> Result<()> {
        let board_dir = self.board_dir();
        let dedup_window =
            Duration::from_secs(self.config.team_config.board.dispatch_dedup_window_secs);

        // Expire stale dedup entries.
        self.recent_dispatches
            .retain(|_, dispatched_at| dispatched_at.elapsed() < dedup_window);

        let mut queued_task_ids: HashSet<u32> = self
            .dispatch_queue
            .iter()
            .map(|entry| entry.task_id)
            .collect();
        let queued_engineers: HashSet<String> = self
            .dispatch_queue
            .iter()
            .map(|entry| entry.engineer.clone())
            .collect();

        let manual_cooldown =
            Duration::from_secs(self.config.team_config.board.dispatch_manual_cooldown_secs);

        let mut engineers = self.idle_engineer_names();
        engineers.sort();
        for engineer_name in engineers {
            if queued_engineers.contains(&engineer_name) {
                continue;
            }
            if let Some(assigned_at) = self.manual_assign_cooldowns.get(&engineer_name) {
                if assigned_at.elapsed() < manual_cooldown {
                    debug!(
                        engineer = %engineer_name,
                        "skipping dispatch — within manual assignment cooldown"
                    );
                    continue;
                }
            }
            let Some(task) = self.next_dispatch_task(&board_dir, &queued_task_ids)? else {
                break;
            };

            // Skip if this (task_id, engineer) pair was dispatched within the dedup window.
            let dedup_key = (task.id, engineer_name.clone());
            if self.recent_dispatches.contains_key(&dedup_key) {
                debug!(
                    task_id = task.id,
                    engineer = %engineer_name,
                    "skipping dispatch — within dedup window"
                );
                continue;
            }

            queued_task_ids.insert(task.id);
            self.dispatch_queue.push(DispatchQueueEntry {
                engineer: engineer_name,
                task_id: task.id,
                task_title: task.title,
                queued_at: now_unix(),
                validation_failures: 0,
                last_failure: None,
            });
        }
        Ok(())
    }

    fn task_for_dispatch_entry(
        &self,
        board_dir: &Path,
        entry: &DispatchQueueEntry,
    ) -> Result<Option<crate::task::Task>> {
        let tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks"))?;
        let task_status_by_id: HashMap<u32, String> = tasks
            .iter()
            .map(|task| (task.id, task.status.clone()))
            .collect();
        Ok(tasks.into_iter().find(|task| {
            task.id == entry.task_id
                && matches!(task.status.as_str(), "backlog" | "todo")
                && task.claimed_by.is_none()
                && task.blocked.is_none()
                && task.blocked_on.is_none()
                && !task.is_schedule_blocked()
                && task.depends_on.iter().all(|dep_id| {
                    task_status_by_id
                        .get(dep_id)
                        .is_none_or(|status| status == "done")
                })
        }))
    }

    pub(in super::super) fn process_dispatch_queue(&mut self) -> Result<()> {
        let board_dir = self.board_dir();
        let mut pending: Vec<DispatchQueueEntry> = std::mem::take(&mut self.dispatch_queue);
        let mut retained = Vec::new();

        for mut entry in pending.drain(..) {
            // Prune stale entries first: if the task is done, claimed by someone
            // else, or no longer exists, drop the entry regardless of engineer
            // state. Without this, entries for non-idle engineers persist forever.
            let task_still_dispatchable =
                self.task_for_dispatch_entry(&board_dir, &entry)?.is_some();
            if !task_still_dispatchable {
                debug!(
                    engineer = %entry.engineer,
                    task_id = entry.task_id,
                    "dispatch queue: pruning stale entry (task done/claimed/missing)"
                );
                continue;
            }

            if self.states.get(&entry.engineer) != Some(&MemberState::Idle) {
                retained.push(entry);
                continue;
            }
            if self.should_hold_dispatch_for_stabilization(&entry.engineer) {
                retained.push(entry);
                continue;
            }

            let Some(task) = self.task_for_dispatch_entry(&board_dir, &entry)? else {
                continue;
            };

            // Skip if the task is already in-progress
            if task.status == "in-progress" {
                info!(
                    engineer = %entry.engineer,
                    task_id = task.id,
                    "dispatch queue: task already in-progress, skipping"
                );
                continue;
            }

            // Skip if the task body has unmet text dependencies
            // (e.g. "Blocked on: #65, #66" where those tasks aren't done)
            if let Some(blocked_ids) = parse_body_dependency_ids(&task.description) {
                let all_tasks =
                    crate::task::load_tasks_from_dir(&board_dir.join("tasks")).unwrap_or_default();
                let unmet: Vec<u32> = blocked_ids
                    .iter()
                    .filter(|id| !all_tasks.iter().any(|t| t.id == **id && t.status == "done"))
                    .copied()
                    .collect();
                if !unmet.is_empty() {
                    warn!(
                        engineer = %entry.engineer,
                        task_id = task.id,
                        ?unmet,
                        "dispatch queue: task has unmet body dependencies, skipping"
                    );
                    // Move to blocked status
                    let _ = crate::team::task_cmd::transition_task(&board_dir, task.id, "blocked");
                    continue;
                }
            }

            let active_count =
                self.engineer_active_board_item_count(&board_dir, &entry.engineer)?;
            if active_count > 0 {
                // Try to reassign to an idle engineer with no active items
                let retained_engineers: HashSet<&str> =
                    retained.iter().map(|e| e.engineer.as_str()).collect();
                let alt = self.idle_engineer_names().into_iter().find(|name| {
                    name != &entry.engineer
                        && !retained_engineers.contains(name.as_str())
                        && self
                            .engineer_active_board_item_count(&board_dir, name)
                            .unwrap_or(1)
                            == 0
                });
                if let Some(alt_engineer) = alt {
                    debug!(
                        from = %entry.engineer,
                        to = %alt_engineer,
                        task_id = entry.task_id,
                        "dispatch queue: reassigning to idle engineer"
                    );
                    entry.engineer = alt_engineer;
                    entry.validation_failures = 0;
                    entry.last_failure = None;
                    retained.push(entry);
                    continue;
                }

                // No alternative — increment failure count
                entry.validation_failures += 1;
                entry.last_failure = Some(format!(
                    "Dispatch guard blocked assignment for '{}' with {} active board item(s); no idle alternative",
                    entry.engineer, active_count
                ));
                if entry.validation_failures >= DISPATCH_QUEUE_FAILURE_LIMIT {
                    // Drop silently — will be re-queued by auto-dispatch when
                    // an engineer frees up. No need to escalate what is just
                    // a "everyone is busy" situation.
                    debug!(
                        engineer = %entry.engineer,
                        task_id = entry.task_id,
                        "dispatch queue: all engineers busy, dropping entry (will re-queue)"
                    );
                } else {
                    retained.push(entry);
                }
                continue;
            }

            if !check_wip_limit(
                &self.config.team_config.workflow_policy,
                RoleType::Engineer,
                active_count,
            ) {
                entry.validation_failures += 1;
                entry.last_failure = Some(format!(
                    "WIP gate blocked dispatch for '{}' with {} active board task(s)",
                    entry.engineer, active_count
                ));
                warn!(
                    engineer = %entry.engineer,
                    task_id = entry.task_id,
                    failures = entry.validation_failures,
                    "dispatch queue: WIP limit blocked dispatch"
                );
                if entry.validation_failures >= DISPATCH_QUEUE_FAILURE_LIMIT {
                    self.escalate_dispatch_queue_entry(
                        &entry,
                        entry
                            .last_failure
                            .as_deref()
                            .unwrap_or("wip gate blocked dispatch"),
                    )?;
                } else {
                    retained.push(entry);
                }
                continue;
            }

            let member_uses_worktrees = self.member_uses_worktrees(&entry.engineer);
            if member_uses_worktrees {
                let worktree_dir = self.worktree_dir(&entry.engineer);
                if let Err(error) = engineer_worktree_ready_for_dispatch(
                    &self.config.project_root,
                    &worktree_dir,
                    &entry.engineer,
                ) {
                    entry.validation_failures += 1;
                    entry.last_failure = Some(error.to_string());
                    warn!(
                        engineer = %entry.engineer,
                        task_id = entry.task_id,
                        failures = entry.validation_failures,
                        error = %error,
                        "dispatch queue: worktree not ready for dispatch"
                    );

                    // Auto-recover: reset worktree to base branch on first failure
                    // instead of waiting for 3 failures to escalate.
                    let base_branch = format!("eng-main/{}", entry.engineer);
                    info!(
                        engineer = %entry.engineer,
                        base_branch = %base_branch,
                        "dispatch queue: auto-resetting worktree to base branch"
                    );
                    // Abort any in-progress merge and clean
                    let _ = std::process::Command::new("git")
                        .args(["merge", "--abort"])
                        .current_dir(&worktree_dir)
                        .output();
                    let _ = std::process::Command::new("git")
                        .args(["checkout", "--", "."])
                        .current_dir(&worktree_dir)
                        .output();
                    let _ = std::process::Command::new("git")
                        .args(["clean", "-fd"])
                        .current_dir(&worktree_dir)
                        .output();
                    if let Err(reset_err) =
                        crate::worktree::reset_worktree_to_base(&worktree_dir, &base_branch)
                    {
                        warn!(
                            engineer = %entry.engineer,
                            error = %reset_err,
                            "dispatch queue: worktree auto-reset failed; escalating"
                        );
                        if entry.validation_failures >= DISPATCH_QUEUE_FAILURE_LIMIT {
                            self.escalate_dispatch_queue_entry(
                                &entry,
                                entry
                                    .last_failure
                                    .as_deref()
                                    .unwrap_or("worktree readiness validation failed"),
                            )?;
                        } else {
                            retained.push(entry);
                        }
                    } else {
                        info!(
                            engineer = %entry.engineer,
                            "dispatch queue: worktree auto-reset succeeded; retrying dispatch"
                        );
                        // Reset failure count and retry on next cycle
                        entry.validation_failures = 0;
                        entry.last_failure = None;
                        retained.push(entry);
                    }
                    continue;
                }
            }

            // Transition to in-progress BEFORE assigning. If this fails,
            // keep the task in the queue — don't send work that the board
            // doesn't reflect, or reconciliation will undo it in a loop.
            if task.status == "backlog" {
                let _ = transition_task(&board_dir, task.id, "todo");
            }
            if let Err(e) = transition_task(&board_dir, task.id, "in-progress") {
                entry.validation_failures += 1;
                entry.last_failure = Some(format!("board transition failed: {e}"));
                warn!(
                    engineer = %entry.engineer,
                    task_id = task.id,
                    error = %e,
                    "dispatch queue: cannot transition task to in-progress, deferring"
                );
                if entry.validation_failures >= DISPATCH_QUEUE_FAILURE_LIMIT {
                    self.escalate_dispatch_queue_entry(
                        &entry,
                        entry
                            .last_failure
                            .as_deref()
                            .unwrap_or("board transition failed"),
                    )?;
                } else {
                    retained.push(entry);
                }
                continue;
            }
            assign_task_owners(&board_dir, task.id, Some(&entry.engineer), None)?;

            let assignment_message =
                format!("Task #{}: {}\n\n{}", task.id, task.title, task.description);
            match self.assign_task_with_task_id(&entry.engineer, &assignment_message, Some(task.id))
            {
                Ok(_) => {
                    self.active_tasks.insert(entry.engineer.clone(), task.id);
                    self.retry_counts.remove(&entry.engineer);
                    self.recent_dispatches
                        .insert((task.id, entry.engineer.clone()), Instant::now());
                    self.record_orchestrator_action(format!(
                        "dispatch queue: selected runnable task #{} ({}) and dispatched it to {}",
                        task.id, task.title, entry.engineer
                    ));
                    info!(
                        engineer = %entry.engineer,
                        task_id = task.id,
                        task_title = %task.title,
                        "queued task dispatched"
                    );
                }
                Err(error) => {
                    entry.validation_failures += 1;
                    entry.last_failure = Some(error.to_string());
                    warn!(
                        engineer = %entry.engineer,
                        task_id = entry.task_id,
                        failures = entry.validation_failures,
                        error = %error,
                        "dispatch queue: assignment launch failed"
                    );
                    if entry.validation_failures >= DISPATCH_QUEUE_FAILURE_LIMIT {
                        self.escalate_dispatch_queue_entry(
                            &entry,
                            entry
                                .last_failure
                                .as_deref()
                                .unwrap_or("assignment launch failed"),
                        )?;
                    } else {
                        retained.push(entry);
                    }
                }
            }
        }

        self.dispatch_queue = retained;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::path::Path;

    use crate::team::standup::MemberState;
    use crate::team::test_support::{
        TestDaemonBuilder, engineer_member, manager_member, write_open_task_file,
        write_owned_task_file,
    };

    fn write_task_with_priority(project_root: &Path, id: u32, title: &str, priority: &str) {
        let tasks_dir = project_root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join(format!("{id:03}-{title}.md")),
            format!(
                "---\nid: {id}\ntitle: {title}\nstatus: todo\npriority: {priority}\nclass: standard\n---\n\nTask.\n"
            ),
        )
        .unwrap();
    }

    fn write_task_with_deps(project_root: &Path, id: u32, title: &str, depends_on: &[u32]) {
        let tasks_dir = project_root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let mut content = format!("---\nid: {id}\ntitle: {title}\nstatus: todo\npriority: high\n");
        if !depends_on.is_empty() {
            content.push_str("depends_on:\n");
            for dep in depends_on {
                content.push_str(&format!("  - {dep}\n"));
            }
        }
        content.push_str("class: standard\n---\n\nTask.\n");
        std::fs::write(tasks_dir.join(format!("{id:03}-{title}.md")), content).unwrap();
    }

    // -- idle_engineer_names tests --

    #[test]
    fn idle_engineers_returns_only_idle() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("mgr", None),
                engineer_member("eng-1", Some("mgr"), false),
                engineer_member("eng-2", Some("mgr"), false),
                engineer_member("eng-3", Some("mgr"), false),
            ])
            .states(HashMap::from([
                ("eng-1".to_string(), MemberState::Idle),
                ("eng-2".to_string(), MemberState::Working),
                ("eng-3".to_string(), MemberState::Idle),
            ]))
            .build();

        let idle = daemon.idle_engineer_names();
        assert_eq!(idle, vec!["eng-1", "eng-3"]);
    }

    #[test]
    fn idle_engineers_empty_when_all_working() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("mgr", None),
                engineer_member("eng-1", Some("mgr"), false),
            ])
            .states(HashMap::from([("eng-1".to_string(), MemberState::Working)]))
            .build();

        assert!(daemon.idle_engineer_names().is_empty());
    }

    #[test]
    fn idle_engineers_excludes_managers() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("mgr", None),
                engineer_member("eng-1", Some("mgr"), false),
            ])
            .states(HashMap::from([
                ("mgr".to_string(), MemberState::Idle),
                ("eng-1".to_string(), MemberState::Idle),
            ]))
            .build();

        let idle = daemon.idle_engineer_names();
        assert_eq!(idle, vec!["eng-1"]);
    }

    // -- next_dispatch_task tests --

    #[test]
    fn next_task_picks_highest_priority() {
        let tmp = tempfile::tempdir().unwrap();
        write_task_with_priority(tmp.path(), 10, "low-pri", "low");
        write_task_with_priority(tmp.path(), 11, "critical-pri", "critical");
        write_task_with_priority(tmp.path(), 12, "medium-pri", "medium");

        let daemon = TestDaemonBuilder::new(tmp.path()).build();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");

        let task = daemon
            .test_next_dispatch_task(&board_dir, &HashSet::new())
            .unwrap()
            .unwrap();
        assert_eq!(task.id, 11, "should pick the critical-priority task");
    }

    #[test]
    fn next_task_breaks_ties_by_id() {
        let tmp = tempfile::tempdir().unwrap();
        write_task_with_priority(tmp.path(), 20, "second", "high");
        write_task_with_priority(tmp.path(), 10, "first", "high");

        let daemon = TestDaemonBuilder::new(tmp.path()).build();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");

        let task = daemon
            .test_next_dispatch_task(&board_dir, &HashSet::new())
            .unwrap()
            .unwrap();
        assert_eq!(task.id, 10, "should pick lower id when priority is equal");
    }

    #[test]
    fn next_task_skips_claimed_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        write_owned_task_file(tmp.path(), 10, "claimed-task", "todo", "eng-2");
        write_open_task_file(tmp.path(), 11, "open-task", "todo");

        let daemon = TestDaemonBuilder::new(tmp.path()).build();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");

        let task = daemon
            .test_next_dispatch_task(&board_dir, &HashSet::new())
            .unwrap()
            .unwrap();
        assert_eq!(task.id, 11, "should skip claimed task");
    }

    #[test]
    fn next_task_skips_done_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        write_open_task_file(tmp.path(), 10, "done-task", "done");
        write_open_task_file(tmp.path(), 11, "open-task", "todo");

        let daemon = TestDaemonBuilder::new(tmp.path()).build();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");

        let task = daemon
            .test_next_dispatch_task(&board_dir, &HashSet::new())
            .unwrap()
            .unwrap();
        assert_eq!(task.id, 11);
    }

    #[test]
    fn next_task_skips_already_queued() {
        let tmp = tempfile::tempdir().unwrap();
        write_open_task_file(tmp.path(), 10, "queued", "todo");
        write_open_task_file(tmp.path(), 11, "available", "todo");

        let daemon = TestDaemonBuilder::new(tmp.path()).build();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");

        let queued: HashSet<u32> = [10].into();
        let task = daemon
            .test_next_dispatch_task(&board_dir, &queued)
            .unwrap()
            .unwrap();
        assert_eq!(task.id, 11, "should skip task already in queue set");
    }

    #[test]
    fn next_task_skips_blocked_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        // Task 10 depends on task 9, which is in-progress (not done)
        write_open_task_file(tmp.path(), 9, "dep-task", "in-progress");
        write_task_with_deps(tmp.path(), 10, "blocked-task", &[9]);
        write_open_task_file(tmp.path(), 11, "free-task", "todo");

        let daemon = TestDaemonBuilder::new(tmp.path()).build();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");

        let task = daemon
            .test_next_dispatch_task(&board_dir, &HashSet::new())
            .unwrap()
            .unwrap();
        assert_eq!(task.id, 11, "should skip task with unmet dependency");
    }

    #[test]
    fn next_task_allows_met_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        write_open_task_file(tmp.path(), 9, "dep-done", "done");
        write_task_with_deps(tmp.path(), 10, "unblocked", &[9]);

        let daemon = TestDaemonBuilder::new(tmp.path()).build();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");

        let task = daemon
            .test_next_dispatch_task(&board_dir, &HashSet::new())
            .unwrap()
            .unwrap();
        assert_eq!(task.id, 10, "should pick task with satisfied dependency");
    }

    #[test]
    fn next_task_returns_none_when_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        let daemon = TestDaemonBuilder::new(tmp.path()).build();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");

        assert!(
            daemon
                .test_next_dispatch_task(&board_dir, &HashSet::new())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn next_task_accepts_backlog_status() {
        let tmp = tempfile::tempdir().unwrap();
        write_open_task_file(tmp.path(), 10, "backlog-task", "backlog");

        let daemon = TestDaemonBuilder::new(tmp.path()).build();
        let board_dir = tmp.path().join(".batty").join("team_config").join("board");

        let task = daemon
            .test_next_dispatch_task(&board_dir, &HashSet::new())
            .unwrap()
            .unwrap();
        assert_eq!(task.id, 10, "backlog status should be dispatchable");
    }

    // -- process_dispatch_queue pruning tests --

    #[test]
    fn process_queue_prunes_entry_for_done_task_even_when_engineer_not_idle() {
        use super::DispatchQueueEntry;
        let tmp = tempfile::tempdir().unwrap();
        // Task is done and claimed by someone else.
        write_owned_task_file(tmp.path(), 10, "finished", "done", "other-eng");

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("mgr", None),
                engineer_member("eng-1", Some("mgr"), false),
            ])
            .states(HashMap::from([
                ("eng-1".to_string(), MemberState::Working),
            ]))
            .build();

        daemon.dispatch_queue.push(DispatchQueueEntry {
            engineer: "eng-1".to_string(),
            task_id: 10,
            task_title: "finished".to_string(),
            queued_at: 0,
            validation_failures: 0,
            last_failure: None,
        });

        daemon.process_dispatch_queue().unwrap();
        assert!(
            daemon.dispatch_queue.is_empty(),
            "entry for done task should be pruned even when engineer is Working"
        );
    }

    #[test]
    fn process_queue_retains_valid_entry_for_non_idle_engineer() {
        use super::DispatchQueueEntry;
        let tmp = tempfile::tempdir().unwrap();
        // Task is still todo and unclaimed — valid for dispatch.
        write_open_task_file(tmp.path(), 10, "pending-work", "todo");

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("mgr", None),
                engineer_member("eng-1", Some("mgr"), false),
            ])
            .states(HashMap::from([
                ("eng-1".to_string(), MemberState::Working),
            ]))
            .build();

        daemon.dispatch_queue.push(DispatchQueueEntry {
            engineer: "eng-1".to_string(),
            task_id: 10,
            task_title: "pending-work".to_string(),
            queued_at: 0,
            validation_failures: 0,
            last_failure: None,
        });

        daemon.process_dispatch_queue().unwrap();
        assert_eq!(
            daemon.dispatch_queue.len(),
            1,
            "entry for valid todo task should be retained while engineer is Working"
        );
    }
}
