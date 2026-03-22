//! Dispatch queue population, processing, and task selection.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use anyhow::Result;
use tracing::{debug, info};

use super::super::super::policy::check_wip_limit;
use super::super::super::task_loop::engineer_worktree_ready_for_dispatch;
use super::super::task_cmd::{assign_task_owners, transition_task};
use super::super::*;
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

        let mut engineers = self.idle_engineer_names();
        engineers.sort();
        for engineer_name in engineers {
            if queued_engineers.contains(&engineer_name) {
                continue;
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

            let active_count =
                self.engineer_active_board_item_count(&board_dir, &entry.engineer)?;
            if active_count > 0 {
                entry.validation_failures += 1;
                entry.last_failure = Some(format!(
                    "Dispatch guard blocked assignment for '{}' with {} active board item(s)",
                    entry.engineer, active_count
                ));
                if entry.validation_failures >= DISPATCH_QUEUE_FAILURE_LIMIT {
                    self.escalate_dispatch_queue_entry(
                        &entry,
                        entry
                            .last_failure
                            .as_deref()
                            .unwrap_or("dispatch guard blocked assignment"),
                    )?;
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
                    continue;
                }
            }

            let assignment_message =
                format!("Task #{}: {}\n\n{}", task.id, task.title, task.description);
            match self.assign_task_with_task_id(&entry.engineer, &assignment_message, Some(task.id))
            {
                Ok(_) => {
                    assign_task_owners(&board_dir, task.id, Some(&entry.engineer), None)?;
                    transition_task(&board_dir, task.id, "in-progress")?;
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
