//! Dispatch queue population, processing, and task selection.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use regex::Regex;
use tracing::{debug, info, warn};

use super::super::super::policy::check_wip_limit;
use super::super::super::task_loop::engineer_worktree_ready_for_dispatch;
use super::super::task_cmd::{append_task_dependencies, assign_task_owners, transition_task};
use super::super::*;
use crate::team::allocation::{
    EngineerProfile, load_engineer_profiles, predict_task_file_paths, rank_engineers_for_task,
};
use crate::team::config::AllocationStrategy;
use serde::Deserialize;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlapConflict {
    pub task_id: String,
    pub conflicting_files: Vec<String>,
    pub in_progress_engineer: String,
}

#[derive(Debug, Default, Deserialize)]
struct ChangedPathsFrontmatter {
    #[serde(default)]
    changed_paths: Vec<String>,
}

fn board_tasks_dir(project_root: &Path) -> std::path::PathBuf {
    project_root
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks")
}

fn extract_frontmatter(content: &str) -> Option<&str> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }
    let after_open = trimmed[3..].strip_prefix('\n').unwrap_or(&trimmed[3..]);
    let end = after_open.find("\n---")?;
    Some(&after_open[..end])
}

fn load_changed_paths(path: &Path) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Some(frontmatter) = extract_frontmatter(&content) else {
        return Vec::new();
    };
    serde_yaml::from_str::<ChangedPathsFrontmatter>(frontmatter)
        .map(|parsed| parsed.changed_paths)
        .unwrap_or_default()
}

fn normalize_predicted_path(path: &str) -> String {
    path.trim_matches(|ch: char| matches!(ch, '.' | ',' | ';' | ':' | ')' | ']'))
        .to_string()
}

fn has_glob_magic(path: &str) -> bool {
    path.contains('*') || path.contains('?')
}

fn glob_to_regex(pattern: &str) -> Option<Regex> {
    let mut regex = String::from("^");
    let mut chars = pattern.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '*' => {
                if chars.peek() == Some(&'*') {
                    chars.next();
                    if chars.peek() == Some(&'/') {
                        chars.next();
                        regex.push_str("(?:.*/)?");
                    } else {
                        regex.push_str(".*");
                    }
                } else {
                    regex.push_str("[^/]*");
                }
            }
            '?' => regex.push_str("[^/]"),
            '.' | '+' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' | '\\' => {
                regex.push('\\');
                regex.push(ch);
            }
            _ => regex.push(ch),
        }
    }
    regex.push('$');
    Regex::new(&regex).ok()
}

fn glob_matches_path(pattern: &str, path: &str) -> bool {
    if !has_glob_magic(pattern) {
        return pattern == path;
    }
    glob_to_regex(pattern)
        .map(|regex| regex.is_match(path))
        .unwrap_or(false)
}

fn glob_literal_prefix(pattern: &str) -> Option<&str> {
    let idx = pattern
        .char_indices()
        .find_map(|(idx, ch)| matches!(ch, '*' | '?').then_some(idx))
        .unwrap_or(pattern.len());
    let prefix = pattern[..idx].trim_end_matches('/');
    (!prefix.is_empty()).then_some(prefix)
}

fn paths_overlap(left: &str, right: &str) -> bool {
    match (has_glob_magic(left), has_glob_magic(right)) {
        (false, false) => left == right,
        (true, false) => glob_matches_path(left, right),
        (false, true) => glob_matches_path(right, left),
        (true, true) => {
            if left == right {
                return true;
            }
            match (glob_literal_prefix(left), glob_literal_prefix(right)) {
                (Some(left_prefix), Some(right_prefix)) => {
                    left_prefix.starts_with(right_prefix) || right_prefix.starts_with(left_prefix)
                }
                _ => true,
            }
        }
    }
}

fn describe_overlap(left: &str, right: &str) -> String {
    match (has_glob_magic(left), has_glob_magic(right)) {
        (false, false) => left.to_string(),
        (true, false) => right.to_string(),
        (false, true) => left.to_string(),
        (true, true) if left == right => left.to_string(),
        (true, true) => format!("{left} <> {right}"),
    }
}

pub fn predicted_files(task: &crate::task::Task, project_root: &Path) -> Vec<String> {
    let mut paths = predict_task_file_paths(project_root, task)
        .unwrap_or_default()
        .into_iter()
        .map(|path| normalize_predicted_path(&path))
        .collect::<Vec<_>>();
    if let Ok(tasks) = crate::task::load_tasks_from_dir(&board_tasks_dir(project_root)) {
        for historical in tasks {
            if historical.id == task.id || historical.tags.is_empty() {
                continue;
            }
            if !task
                .tags
                .iter()
                .any(|tag| historical.tags.iter().any(|candidate| candidate == tag))
            {
                continue;
            }
            paths.extend(
                load_changed_paths(historical.source_path.as_path())
                    .into_iter()
                    .map(|path| normalize_predicted_path(&path)),
            );
        }
    }
    paths.retain(|path| !path.is_empty());
    paths.sort();
    paths.dedup();
    paths
}

fn overlapping_files(candidate_paths: &[String], active_paths: &[String]) -> Vec<String> {
    let mut overlaps = BTreeSet::new();
    for candidate in candidate_paths {
        for active in active_paths {
            if paths_overlap(candidate, active) {
                overlaps.insert(describe_overlap(candidate, active));
            }
        }
    }
    overlaps.into_iter().collect()
}

pub fn find_overlapping_tasks(
    candidate: &crate::task::Task,
    in_progress: &[crate::task::Task],
    project_root: &Path,
) -> Vec<OverlapConflict> {
    let candidate_paths = predicted_files(candidate, project_root);
    let mut conflicts = Vec::new();

    for active_task in in_progress {
        if active_task.id == candidate.id {
            continue;
        }
        let active_paths = predicted_files(active_task, project_root);
        let conflicting_files = overlapping_files(&candidate_paths, &active_paths);
        if conflicting_files.is_empty() {
            continue;
        }
        conflicts.push(OverlapConflict {
            task_id: active_task.id.to_string(),
            conflicting_files,
            in_progress_engineer: active_task
                .claimed_by
                .clone()
                .unwrap_or_else(|| "unknown".to_string()),
        });
    }

    conflicts.sort_by(|left, right| left.task_id.cmp(&right.task_id));
    conflicts
}

fn available_dispatch_tasks(
    board_dir: &Path,
    queued_task_ids: &HashSet<u32>,
    excluded_tags: &[String],
    non_engineer_assignees: &HashSet<String>,
    rescued_task_ids: &HashSet<u32>,
) -> Result<Vec<crate::task::Task>> {
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
        .filter(|task| !task_has_excluded_tag(task, excluded_tags))
        // #684: tasks orphan-rescued back to todo within the cooldown
        // window are held off dispatch so the releasing engineer or
        // manager can reclaim/re-route before an auto-redispatch to a
        // peer (which is almost always the wrong answer when the
        // original claimer parked intentionally).
        .filter(|task| !rescued_task_ids.contains(&task.id))
        // #682: tasks with `assignee:` pointing at a non-engineer (manager,
        // architect, writer, …) are messages for that member's inbox, not
        // dispatch candidates. Leaving them in the pool causes repeated
        // wrong-role dispatches that burn engineer context re-reading and
        // rejecting a task they can't take.
        .filter(|task| {
            task.assignee
                .as_deref()
                .is_none_or(|name| !non_engineer_assignees.contains(name))
        })
        .filter(|task| {
            task.depends_on.iter().all(|dep_id| {
                task_status_by_id
                    .get(dep_id)
                    .is_none_or(|status| dep_status_satisfied(status))
            })
        })
        .collect();

    available.sort_by_key(|task| (dispatch_priority_rank(&task.priority), task.id));
    Ok(available)
}

/// #681: A dependency in `done` or `archived` state is fully satisfied.
/// Archived tasks are terminal (completed then cleaned up) and should
/// unblock dependents the same way `done` does — otherwise downstream
/// work stays stuck after a long-running project winds down.
fn dep_status_satisfied(status: &str) -> bool {
    matches!(status, "done" | "archived")
}

/// #677: A task matches the excluded tags list (case-insensitive) when any
/// of its tags appears in the operator's `board.dispatch_excluded_tags`.
/// Matched tasks are held off the dispatch queue until an operator claims
/// them manually. Empty list means no filtering.
fn task_has_excluded_tag(task: &crate::task::Task, excluded_tags: &[String]) -> bool {
    if excluded_tags.is_empty() {
        return false;
    }
    task.tags.iter().any(|task_tag| {
        excluded_tags
            .iter()
            .any(|excluded| excluded.eq_ignore_ascii_case(task_tag))
    })
}

impl TeamDaemon {
    fn serialize_overlapping_candidate(
        &mut self,
        board_dir: &Path,
        candidate: &crate::task::Task,
        conflicts: &[OverlapConflict],
        persist_dependency: bool,
    ) -> Result<bool> {
        if conflicts.is_empty() {
            return Ok(false);
        }

        let mut blocking_task_ids: Vec<u32> = conflicts
            .iter()
            .filter_map(|conflict| conflict.task_id.parse::<u32>().ok())
            .collect();
        blocking_task_ids.sort_unstable();
        blocking_task_ids.dedup();
        let overlap_details = conflicts
            .iter()
            .map(|conflict| {
                format!(
                    "#{} [{}]",
                    conflict.task_id,
                    conflict.conflicting_files.join(", ")
                )
            })
            .collect::<Vec<_>>();
        let updated_dependencies = if persist_dependency {
            Some(append_task_dependencies(
                board_dir,
                candidate.id,
                &blocking_task_ids,
            )?)
        } else {
            None
        };
        let details = if persist_dependency {
            format!(
                "serialized task #{} behind {} due to predicted file overlap",
                candidate.id,
                overlap_details.join("; ")
            )
        } else {
            format!(
                "deferred task #{} in file_lock_wait behind {} due to predicted file overlap",
                candidate.id,
                overlap_details.join("; ")
            )
        };
        self.emit_event(TeamEvent::dispatch_overlap_prevented(
            candidate.id,
            &blocking_task_ids,
            &details,
        ));
        self.record_orchestrator_action(format!("dispatch overlap: {details}"));
        info!(
            task_id = candidate.id,
            blocking = ?updated_dependencies,
            persist_dependency,
            "dispatch queue: prevented overlapping dispatch"
        );
        Ok(true)
    }

    pub(in super::super) fn idle_engineer_names(&self) -> Vec<String> {
        self.config
            .members
            .iter()
            .filter(|member| member.role_type == RoleType::Engineer)
            .filter(|member| {
                let state = self.states.get(&member.name);
                match state {
                    Some(&MemberState::Idle) => true,
                    // Working engineers with no active task are effectively idle
                    // and should be eligible for dispatch.
                    Some(&MemberState::Working) => !self.active_tasks.contains_key(&member.name),
                    _ => false,
                }
            })
            .map(|member| member.name.clone())
            .collect()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn next_dispatch_task(
        &self,
        board_dir: &Path,
        queued_task_ids: &HashSet<u32>,
    ) -> Result<Option<crate::task::Task>> {
        Ok(available_dispatch_tasks(
            board_dir,
            queued_task_ids,
            &self.config.team_config.board.dispatch_excluded_tags,
            &self.non_engineer_member_names(),
            &self.rescued_task_ids(),
        )?
        .into_iter()
        .next())
    }

    /// #684 / #686: task IDs currently within the orphan-rescue cooldown
    /// window (exponentially grown per repeated rescue). Dispatch filters
    /// these out so a task the releasing engineer parked doesn't immediately
    /// bounce to a peer, and tasks that keep getting rescued stay quiet
    /// longer instead of re-cascading every base window.
    pub(super) fn rescued_task_ids(&self) -> HashSet<u32> {
        let base = Duration::from_secs(
            self.config.team_config.board.orphan_rescue_cooldown_secs,
        );
        self.recently_rescued_tasks
            .iter()
            .filter(|(_, record)| record.dispatch_blocked(base))
            .map(|(task_id, _)| *task_id)
            .collect()
    }

    /// #686 / #689: record a rescue event for `task_id`. Growth condition
    /// uses the cascade-observation window (2× effective cooldown), not
    /// the dispatch-gate window — rescues can only fire *after* the
    /// dispatch gate has opened, so gating growth on `dispatch_blocked`
    /// meant the counter never climbed past 1 in production.
    pub(in super::super) fn record_task_rescue(&mut self, task_id: u32) {
        let base = Duration::from_secs(
            self.config.team_config.board.orphan_rescue_cooldown_secs,
        );
        let now = Instant::now();
        self.recently_rescued_tasks
            .entry(task_id)
            .and_modify(|record| {
                if record.in_cascade_window(base) {
                    record.count = record.count.saturating_add(1);
                } else {
                    record.count = 1;
                }
                record.last_rescued_at = now;
            })
            .or_insert(crate::team::daemon::RescueRecord {
                last_rescued_at: now,
                count: 1,
            });
    }

    /// Returns names of configured members whose role is NOT `Engineer`.
    /// Tasks whose `assignee:` frontmatter points at one of these names
    /// are excluded from dispatch — they belong in that member's inbox.
    fn non_engineer_member_names(&self) -> HashSet<String> {
        self.config
            .members
            .iter()
            .filter(|member| member.role_type != RoleType::Engineer)
            .map(|member| member.name.clone())
            .collect()
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
        let board_tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks"))?;
        let benched_engineers = crate::team::bench::benched_engineer_names(self.project_root())?;
        let dedup_window =
            Duration::from_secs(self.config.team_config.board.dispatch_dedup_window_secs);

        // Expire stale dedup entries.
        self.recent_dispatches
            .retain(|_, dispatched_at| dispatched_at.elapsed() < dedup_window);

        // #684 / #686 / #689: retain rescue records through the full
        // cascade-observation window (2× effective cooldown), not just
        // the dispatch gate. Dropping the record the moment the gate
        // opens is what caused the counter to reset to 1 on every
        // rescue in production — killing the exponential backoff.
        let rescue_base_cooldown = Duration::from_secs(
            self.config.team_config.board.orphan_rescue_cooldown_secs,
        );
        self.recently_rescued_tasks
            .retain(|_, record| record.in_cascade_window(rescue_base_cooldown));
        // Only task IDs still behind the dispatch gate should block new
        // dispatch; past-gate-but-in-window entries live on only so the
        // next rescue can see them and grow the counter.
        let rescued_task_ids: HashSet<u32> = self
            .recently_rescued_tasks
            .iter()
            .filter(|(_, record)| record.dispatch_blocked(rescue_base_cooldown))
            .map(|(task_id, _)| *task_id)
            .collect();

        let mut queued_task_ids: HashSet<u32> = self
            .dispatch_queue
            .iter()
            .map(|entry| entry.task_id)
            .collect();
        let mut queued_engineers: HashSet<String> = self
            .dispatch_queue
            .iter()
            .map(|entry| entry.engineer.clone())
            .collect();
        let mut file_locked_task_ids = HashSet::new();

        let manual_cooldown =
            Duration::from_secs(self.config.team_config.board.dispatch_manual_cooldown_secs);

        let all_engineers: Vec<String> = self
            .config
            .members
            .iter()
            .filter(|member| member.role_type == RoleType::Engineer)
            .map(|member| member.name.clone())
            .collect();
        let non_engineer_names = self.non_engineer_member_names();
        let profiles = load_engineer_profiles(self.project_root(), &all_engineers, &board_tasks)?;

        loop {
            let mut unavailable_task_ids = queued_task_ids.clone();
            unavailable_task_ids.extend(file_locked_task_ids.iter().copied());
            let available_tasks = available_dispatch_tasks(
                &board_dir,
                &unavailable_task_ids,
                &self.config.team_config.board.dispatch_excluded_tags,
                &non_engineer_names,
                &rescued_task_ids,
            )?;
            if available_tasks.is_empty() {
                break;
            }

            let in_progress_tasks: Vec<crate::task::Task> =
                crate::task::load_tasks_from_dir(&board_dir.join("tasks"))?
                    .into_iter()
                    .filter(|task| task.status == "in-progress")
                    .collect();
            let mut selected_task = None;
            let mut least_conflicted: Option<(crate::task::Task, Vec<OverlapConflict>)> = None;
            let file_level_locks_enabled = self.config.team_config.workflow_policy.file_level_locks;

            // Skip overlap check when all engineers use worktrees — conflicts
            // are handled at merge time (cherry-pick), not dispatch time.
            let all_engineers_use_worktrees = self
                .config
                .team_config
                .roles
                .iter()
                .filter(|r| r.role_type == crate::team::config::RoleType::Engineer)
                .all(|r| r.use_worktrees);
            let skip_overlap_checks = all_engineers_use_worktrees && !file_level_locks_enabled;

            for task in available_tasks {
                if skip_overlap_checks {
                    selected_task = Some(task);
                    break;
                }

                let conflicts =
                    find_overlapping_tasks(&task, &in_progress_tasks, self.project_root());
                if conflicts.is_empty() {
                    selected_task = Some(task);
                    break;
                }

                for conflict in &conflicts {
                    self.emit_event(TeamEvent::dispatch_overlap_skipped(
                        task.id,
                        &conflict.task_id,
                        &conflict.conflicting_files,
                    ));
                }

                if file_level_locks_enabled {
                    self.serialize_overlapping_candidate(&board_dir, &task, &conflicts, false)?;
                    file_locked_task_ids.insert(task.id);
                    continue;
                }

                let replace = least_conflicted
                    .as_ref()
                    .is_none_or(|(_, existing)| conflicts.len() < existing.len());
                if replace {
                    least_conflicted = Some((task, conflicts));
                }
            }

            let task = if let Some(task) = selected_task {
                task
            } else if let Some((task, conflicts)) = least_conflicted {
                self.serialize_overlapping_candidate(&board_dir, &task, &conflicts, true)?;
                continue;
            } else {
                break;
            };
            let ranked_engineers = self.rank_dispatch_engineers(
                &task,
                &queued_engineers,
                &benched_engineers,
                manual_cooldown,
                &profiles,
            );
            let Some(engineer_name) = ranked_engineers.into_iter().find(|engineer_name| {
                !self
                    .recent_dispatches
                    .contains_key(&(task.id, engineer_name.clone()))
            }) else {
                break;
            };

            queued_task_ids.insert(task.id);
            queued_engineers.insert(engineer_name.clone());
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
                        .is_none_or(|status| dep_status_satisfied(status))
                })
        }))
    }

    pub(in super::super) fn process_dispatch_queue(&mut self) -> Result<()> {
        let board_dir = self.board_dir();
        let benched_engineers = crate::team::bench::benched_engineer_names(self.project_root())?;
        // #689: re-check the rescue cooldown at drain time. An entry can
        // be queued at `enqueue_dispatch_candidates` time, then the task
        // enters the rescue cooldown later in the same tick (when the
        // orphan-rescue runs after the queue was populated). Without
        // this re-check, the stale entry consumes the new cooldown by
        // dispatching on the very next drain.
        let rescued_task_ids = self.rescued_task_ids();
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
            if rescued_task_ids.contains(&entry.task_id) {
                info!(
                    engineer = %entry.engineer,
                    task_id = entry.task_id,
                    "dispatch queue: pruning entry — task re-entered orphan-rescue cooldown"
                );
                continue;
            }
            if benched_engineers.contains(&entry.engineer) {
                debug!(
                    engineer = %entry.engineer,
                    task_id = entry.task_id,
                    "dispatch queue: pruning benched engineer entry"
                );
                continue;
            }

            // Recover engineers stuck in Working with no active task.
            // This happens when mark_member_working() fires but task
            // delivery fails, leaving the state map inconsistent.
            if self.states.get(&entry.engineer) == Some(&MemberState::Working)
                && !self.active_tasks.contains_key(&entry.engineer)
            {
                info!(
                    engineer = %entry.engineer,
                    task_id = entry.task_id,
                    "dispatch queue: recovering engineer stuck in Working with no active task"
                );
                self.states
                    .insert(entry.engineer.clone(), MemberState::Idle);
                self.update_automation_timers_for_state(&entry.engineer, MemberState::Idle);
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

                    // Auto-recover: try rebase first, only reset as last resort.
                    let base_branch = format!("eng-main/{}", entry.engineer);

                    // SAFETY: if worktree has commits ahead of main, try rebase not reset.
                    let has_work = crate::worktree::commits_ahead(&worktree_dir, "main")
                        .map(|n| n > 0)
                        .unwrap_or(false)
                        || crate::worktree::has_uncommitted_changes(&worktree_dir).unwrap_or(false);

                    if has_work {
                        info!(
                            engineer = %entry.engineer,
                            "dispatch queue: worktree has work; trying rebase instead of reset"
                        );
                        // Try to rebase onto main to preserve work
                        let rebase_result = std::process::Command::new("git")
                            .args(["rebase", "main"])
                            .current_dir(&worktree_dir)
                            .output();
                        if rebase_result.map(|o| o.status.success()).unwrap_or(false) {
                            match crate::team::task_loop::engineer_worktree_ready_for_dispatch(
                                &self.config.project_root,
                                &worktree_dir,
                                &entry.engineer,
                            ) {
                                Ok(()) => {
                                    info!(
                                        engineer = %entry.engineer,
                                        "dispatch queue: rebase succeeded; retrying dispatch"
                                    );
                                    entry.validation_failures = 0;
                                    entry.last_failure = None;
                                    retained.push(entry);
                                    continue;
                                }
                                Err(error) => {
                                    warn!(
                                        engineer = %entry.engineer,
                                        error = %error,
                                        "dispatch queue: rebase succeeded but worktree is still not ready; falling through to reset"
                                    );
                                }
                            }
                        }
                        // Rebase failed — abort and fall through to reset
                        let _ = std::process::Command::new("git")
                            .args(["rebase", "--abort"])
                            .current_dir(&worktree_dir)
                            .output();
                        warn!(
                            engineer = %entry.engineer,
                            "dispatch queue: rebase failed; falling through to reset (work may be lost)"
                        );
                    }

                    info!(
                        engineer = %entry.engineer,
                        base_branch = %base_branch,
                        "dispatch queue: auto-resetting worktree to base branch"
                    );
                    match crate::worktree::reset_worktree_to_base_if_clean(
                        &worktree_dir,
                        &base_branch,
                        "dispatch/reset recovery",
                    ) {
                        Err(reset_err) => {
                            warn!(
                                engineer = %entry.engineer,
                                error = %reset_err,
                                "dispatch queue: worktree auto-reset failed; escalating"
                            );
                            entry.validation_failures += 1;
                            entry.last_failure = Some(reset_err.to_string());
                            self.report_preserve_failure(
                                &entry.engineer,
                                None,
                                "dispatch/reset recovery",
                                &reset_err.to_string(),
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
                        }
                        Ok(reason) if reason.reset_performed() => {
                            info!(
                                engineer = %entry.engineer,
                                reset_reason = reason.as_str(),
                                "dispatch queue: worktree auto-reset succeeded; retrying dispatch"
                            );
                            entry.validation_failures = 0;
                            entry.last_failure = None;
                            retained.push(entry);
                        }
                        Ok(reason) => {
                            warn!(
                                engineer = %entry.engineer,
                                reset_reason = reason.as_str(),
                                "dispatch queue: worktree auto-reset skipped"
                            );
                            entry.validation_failures += 1;
                            entry.last_failure = Some(
                                crate::team::task_loop::dirty_worktree_preservation_blocked_reason(
                                    &worktree_dir,
                                    "dispatch/reset recovery",
                                ),
                            );
                            self.report_preserve_failure(
                                &entry.engineer,
                                None,
                                "dispatch/reset recovery",
                                reason.as_str(),
                            );
                            retained.push(entry);
                        }
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

    fn rank_dispatch_engineers(
        &self,
        task: &crate::task::Task,
        queued_engineers: &HashSet<String>,
        benched_engineers: &std::collections::BTreeSet<String>,
        manual_cooldown: Duration,
        profiles: &HashMap<String, EngineerProfile>,
    ) -> Vec<String> {
        let mut eligible: Vec<String> = self
            .idle_engineer_names()
            .into_iter()
            .filter(|engineer_name| !queued_engineers.contains(engineer_name))
            .filter(|engineer_name| !benched_engineers.contains(engineer_name))
            // #682: honor `assignee:` frontmatter when it names an engineer.
            // Non-engineer assignees are filtered earlier in
            // `available_dispatch_tasks`; by the time we get here, an
            // assignee must be an engineer who wants this specific task.
            .filter(|engineer_name| {
                task.assignee
                    .as_deref()
                    .is_none_or(|preferred| preferred == engineer_name)
            })
            .filter(|engineer_name| {
                // #674 defect 2: skip engineers whose backend is parked
                // (quota_exhausted with future retry_at). Without this gate,
                // a stale cached `Healthy` state or the 15-minute stall-timer
                // reclaim would rotate tasks through every quota-blocked
                // engineer on every dispatch tick.
                if self.member_backend_parked(engineer_name) {
                    debug!(
                        engineer = %engineer_name,
                        "skipping dispatch — backend quota parked"
                    );
                    return false;
                }
                let Some(assigned_at) = self.manual_assign_cooldowns.get(engineer_name) else {
                    return true;
                };
                if assigned_at.elapsed() < manual_cooldown {
                    debug!(
                        engineer = %engineer_name,
                        "skipping dispatch — within manual assignment cooldown"
                    );
                    false
                } else {
                    true
                }
            })
            .collect();
        eligible.sort();

        if self.config.team_config.workflow_policy.allocation.strategy
            == AllocationStrategy::RoundRobin
        {
            return eligible;
        }
        rank_engineers_for_task(
            &eligible,
            profiles,
            task,
            &self.config.team_config.workflow_policy.allocation,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::path::Path;

    use super::{OverlapConflict, find_overlapping_tasks, predicted_files};
    use crate::team::standup::MemberState;
    use crate::team::task_loop::{
        current_worktree_branch, engineer_base_branch_name, setup_engineer_worktree,
    };
    use crate::team::test_support::{
        TestDaemonBuilder, engineer_member, git_ok, git_stdout, init_git_repo, manager_member,
        write_open_task_file, write_owned_task_file,
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

    fn write_task_with_body(
        project_root: &Path,
        id: u32,
        title: &str,
        status: &str,
        claimed_by: Option<&str>,
        body: &str,
    ) {
        let tasks_dir = project_root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let mut content =
            format!("---\nid: {id}\ntitle: {title}\nstatus: {status}\npriority: high\n");
        if let Some(claimed_by) = claimed_by {
            content.push_str(&format!("claimed_by: {claimed_by}\n"));
        }
        content.push_str("class: standard\n---\n\n");
        content.push_str(body);
        content.push('\n');
        std::fs::write(tasks_dir.join(format!("{id:03}-{title}.md")), content).unwrap();
    }

    fn write_task_with_files(
        project_root: &Path,
        id: u32,
        title: &str,
        status: &str,
        claimed_by: Option<&str>,
        files: &[&str],
        body: &str,
    ) {
        let tasks_dir = project_root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let mut content =
            format!("---\nid: {id}\ntitle: {title}\nstatus: {status}\npriority: high\n");
        if let Some(claimed_by) = claimed_by {
            content.push_str(&format!("claimed_by: {claimed_by}\n"));
        }
        if !files.is_empty() {
            content.push_str("files:\n");
            for file in files {
                content.push_str(&format!("  - {file}\n"));
            }
        }
        content.push_str("class: standard\n---\n\n");
        content.push_str(body);
        content.push('\n');
        std::fs::write(tasks_dir.join(format!("{id:03}-{title}.md")), content).unwrap();
    }

    fn write_task_with_assignee(project_root: &Path, id: u32, title: &str, assignee: &str) {
        let tasks_dir = project_root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join(format!("{id:03}-{title}.md")),
            format!(
                "---\nid: {id}\ntitle: {title}\nstatus: todo\npriority: high\nassignee: {assignee}\nclass: standard\n---\n\nTask.\n"
            ),
        )
        .unwrap();
    }

    fn write_bench_test_team_config(project_root: &Path, engineer_instances: u32) {
        let team_dir = project_root.join(".batty").join("team_config");
        std::fs::create_dir_all(&team_dir).unwrap();
        std::fs::write(
            team_dir.join("team.yaml"),
            format!(
                "name: test\nagent: codex\nroles:\n  - name: eng\n    role_type: engineer\n    instances: {engineer_instances}\n"
            ),
        )
        .unwrap();
    }

    // -- idle_engineer_names tests --

    #[test]
    fn idle_engineers_returns_only_idle() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
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
        // eng-2 is Working WITH an active task — should be excluded
        daemon.active_tasks.insert("eng-2".to_string(), 42);

        let idle = daemon.idle_engineer_names();
        assert_eq!(idle, vec!["eng-1", "eng-3"]);
    }

    #[test]
    fn idle_engineers_empty_when_all_working_with_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("mgr", None),
                engineer_member("eng-1", Some("mgr"), false),
            ])
            .states(HashMap::from([("eng-1".to_string(), MemberState::Working)]))
            .build();
        daemon.active_tasks.insert("eng-1".to_string(), 10);

        assert!(daemon.idle_engineer_names().is_empty());
    }

    #[test]
    fn idle_engineers_includes_working_without_active_task() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("mgr", None),
                engineer_member("eng-1", Some("mgr"), false),
                engineer_member("eng-2", Some("mgr"), false),
            ])
            .states(HashMap::from([
                ("eng-1".to_string(), MemberState::Working),
                ("eng-2".to_string(), MemberState::Idle),
            ]))
            .build();
        // eng-1 is Working but has NO active task — should be dispatchable
        let idle = daemon.idle_engineer_names();
        assert_eq!(idle, vec!["eng-1", "eng-2"]);
    }

    #[test]
    fn idle_engineers_working_no_task_mixed_with_working_with_task() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("mgr", None),
                engineer_member("eng-1", Some("mgr"), false),
                engineer_member("eng-2", Some("mgr"), false),
                engineer_member("eng-3", Some("mgr"), false),
            ])
            .states(HashMap::from([
                ("eng-1".to_string(), MemberState::Working),
                ("eng-2".to_string(), MemberState::Working),
                ("eng-3".to_string(), MemberState::Idle),
            ]))
            .build();
        // eng-1 has an active task, eng-2 does not
        daemon.active_tasks.insert("eng-1".to_string(), 50);

        let idle = daemon.idle_engineer_names();
        assert_eq!(idle, vec!["eng-2", "eng-3"]);
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
            .states(HashMap::from([("eng-1".to_string(), MemberState::Working)]))
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
            .states(HashMap::from([("eng-1".to_string(), MemberState::Working)]))
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

    #[test]
    fn process_queue_blocks_dirty_worktree_instead_of_auto_preserving() {
        use super::DispatchQueueEntry;

        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "dispatch-preserve-reset");
        write_open_task_file(&repo, 42, "dispatch-reset", "todo");

        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");
        let base_branch = engineer_base_branch_name("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
        git_ok(&worktree_dir, &["checkout", "-b", "eng-1/41"]);
        std::fs::write(worktree_dir.join("tracked.txt"), "tracked dispatch work\n").unwrap();
        git_ok(&worktree_dir, &["add", "tracked.txt"]);
        std::fs::write(
            worktree_dir.join("untracked.txt"),
            "untracked dispatch work\n",
        )
        .unwrap();

        let mut daemon = TestDaemonBuilder::new(repo.as_path())
            .members(vec![
                manager_member("mgr", None),
                engineer_member("eng-1", Some("mgr"), true),
            ])
            .board(crate::team::config::BoardConfig {
                dispatch_stabilization_delay_secs: 0,
                ..crate::team::config::BoardConfig::default()
            })
            .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
            .build();
        daemon.idle_started_at.insert(
            "eng-1".to_string(),
            std::time::Instant::now() - std::time::Duration::from_secs(1),
        );
        daemon.dispatch_queue.push(DispatchQueueEntry {
            engineer: "eng-1".to_string(),
            task_id: 42,
            task_title: "dispatch-reset".to_string(),
            queued_at: 0,
            validation_failures: 0,
            last_failure: None,
        });

        daemon.process_dispatch_queue().unwrap();

        assert_eq!(daemon.dispatch_queue.len(), 1);
        assert_eq!(daemon.dispatch_queue[0].validation_failures, 2);
        assert!(
            daemon.dispatch_queue[0]
                .last_failure
                .as_deref()
                .unwrap_or("")
                .contains("could not safely auto-save dirty worktree")
        );
        assert_eq!(current_worktree_branch(&worktree_dir).unwrap(), "eng-1/41");
        let status = git_stdout(&worktree_dir, &["status", "--short"]);
        assert!(
            status.contains("A  tracked.txt"),
            "tracked work should remain staged instead of being auto-preserved: {status}"
        );
        assert!(
            status.contains("?? untracked.txt"),
            "untracked work should remain untouched instead of being auto-preserved: {status}"
        );
        assert!(
            git_stdout(&repo, &["branch", "--list", "eng-1/41"]).contains("eng-1/41"),
            "dirty task branch should remain in place for manual recovery"
        );
    }

    #[test]
    fn process_queue_blocks_dirty_worktree_when_preserve_fails() {
        use super::DispatchQueueEntry;

        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "dispatch-preserve-blocked");
        write_open_task_file(&repo, 42, "dispatch-reset", "todo");

        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");
        let base_branch = engineer_base_branch_name("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
        git_ok(&worktree_dir, &["checkout", "-b", "eng-1/41"]);
        std::fs::write(worktree_dir.join("tracked.txt"), "tracked dispatch work\n").unwrap();
        git_ok(&worktree_dir, &["add", "tracked.txt"]);
        std::fs::write(worktree_dir.join("unstaged.txt"), "leave unstaged\n").unwrap();
        let git_dir =
            std::path::PathBuf::from(git_stdout(&worktree_dir, &["rev-parse", "--git-dir"]));
        let git_dir = if git_dir.is_absolute() {
            git_dir
        } else {
            worktree_dir.join(git_dir)
        };
        std::fs::write(git_dir.join("index.lock"), "locked\n").unwrap();

        let mut daemon = TestDaemonBuilder::new(repo.as_path())
            .members(vec![
                manager_member("mgr", None),
                engineer_member("eng-1", Some("mgr"), true),
            ])
            .board(crate::team::config::BoardConfig {
                dispatch_stabilization_delay_secs: 0,
                ..crate::team::config::BoardConfig::default()
            })
            .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
            .build();
        daemon.idle_started_at.insert(
            "eng-1".to_string(),
            std::time::Instant::now() - std::time::Duration::from_secs(1),
        );
        daemon.dispatch_queue.push(DispatchQueueEntry {
            engineer: "eng-1".to_string(),
            task_id: 42,
            task_title: "dispatch-reset".to_string(),
            queued_at: 0,
            validation_failures: 0,
            last_failure: None,
        });

        daemon.process_dispatch_queue().unwrap();

        assert_eq!(daemon.dispatch_queue.len(), 1);
        assert_eq!(daemon.dispatch_queue[0].validation_failures, 2);
        assert!(
            daemon.dispatch_queue[0]
                .last_failure
                .as_deref()
                .unwrap_or("")
                .contains("could not safely auto-save dirty worktree")
        );
        assert_eq!(current_worktree_branch(&worktree_dir).unwrap(), "eng-1/41");
        let status = git_stdout(&worktree_dir, &["status", "--short"]);
        assert!(
            status.contains("A  tracked.txt"),
            "pre-existing staged work should remain staged: {status}"
        );
        assert!(
            status.contains("?? unstaged.txt"),
            "idle dispatch recovery must not stage new files: {status}"
        );
    }

    fn write_blocked_task(project_root: &Path, id: u32, title: &str, block_reason: &str) {
        let tasks_dir = project_root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        // kanban-md --block writes `blocked: true` + `block_reason: "..."`.
        // Regression guard against the old Option<String> deserializer which
        // silently dropped the boolean shape and let dispatch see the task
        // as runnable.
        let content = format!(
            "---\nid: {id}\ntitle: {title}\nstatus: todo\npriority: high\nblocked: true\nblock_reason: \"{block_reason}\"\nclass: standard\n---\n\nBody.\n"
        );
        std::fs::write(tasks_dir.join(format!("{id:03}-{title}.md")), content).unwrap();
    }

    #[test]
    fn enqueue_dispatch_candidates_skips_kanban_md_blocked_tasks() {
        // Regression for #589: kanban-md --block writes `blocked: true` +
        // `block_reason: "..."`, which used to deserialize to None because
        // the Task struct's blocked field was Option<String>. Dispatch then
        // treated the task as runnable and auto-assigned it to benched
        // engineers. The fix is an untagged deserializer that accepts both
        // boolean and string shapes and routes `block_reason` into `blocked`.
        let tmp = tempfile::tempdir().unwrap();
        write_blocked_task(
            tmp.path(),
            30,
            "kanban-md-blocked",
            "Deferred per architect",
        );
        write_task_with_body(
            tmp.path(),
            31,
            "runnable-candidate",
            "todo",
            None,
            "Touch src/team/telemetry_db.rs only.",
        );

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("mgr", None),
                engineer_member("eng-1", Some("mgr"), false),
            ])
            .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
            .build();

        daemon.enqueue_dispatch_candidates().unwrap();

        assert_eq!(daemon.dispatch_queue.len(), 1);
        assert_eq!(
            daemon.dispatch_queue[0].task_id, 31,
            "blocked task #30 must be filtered out; only the runnable #31 should be queued"
        );
    }

    #[test]
    fn enqueue_dispatch_candidates_skips_tasks_assigned_to_non_engineer() {
        // #682: a task whose `assignee:` frontmatter points at a manager/
        // architect is a message for that member's inbox, not a dispatch
        // candidate. Previously these tasks were repeatedly handed to
        // engineers who immediately rejected them — burning engineer
        // context re-reading huge bodies on every dispatch tick.
        let tmp = tempfile::tempdir().unwrap();
        write_task_with_assignee(tmp.path(), 40, "pm-intake", "mgr");
        write_task_with_body(
            tmp.path(),
            41,
            "engineer-candidate",
            "todo",
            None,
            "Touch src/team/telemetry_db.rs only.",
        );

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("mgr", None),
                engineer_member("eng-1", Some("mgr"), false),
            ])
            .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
            .build();

        daemon.enqueue_dispatch_candidates().unwrap();

        assert_eq!(daemon.dispatch_queue.len(), 1);
        assert_eq!(
            daemon.dispatch_queue[0].task_id, 41,
            "non-engineer-assigned task #40 must be filtered out; only #41 should dispatch"
        );
    }

    #[test]
    fn enqueue_dispatch_candidates_routes_engineer_assigned_task_to_named_engineer() {
        // #682: when `assignee:` names an engineer, dispatch must route the
        // task only to that engineer — even when other idle engineers could
        // otherwise take it. Previously the dispatcher ignored the field
        // and the task went to whichever idle engineer won the ranking.
        let tmp = tempfile::tempdir().unwrap();
        write_task_with_assignee(tmp.path(), 50, "for-eng-2", "eng-2");

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("mgr", None),
                engineer_member("eng-1", Some("mgr"), false),
                engineer_member("eng-2", Some("mgr"), false),
                engineer_member("eng-3", Some("mgr"), false),
            ])
            .states(HashMap::from([
                ("eng-1".to_string(), MemberState::Idle),
                ("eng-2".to_string(), MemberState::Idle),
                ("eng-3".to_string(), MemberState::Idle),
            ]))
            .build();

        daemon.enqueue_dispatch_candidates().unwrap();

        assert_eq!(daemon.dispatch_queue.len(), 1);
        assert_eq!(daemon.dispatch_queue[0].task_id, 50);
        assert_eq!(
            daemon.dispatch_queue[0].engineer, "eng-2",
            "task with `assignee: eng-2` must dispatch to eng-2, not a peer"
        );
    }

    #[test]
    fn enqueue_dispatch_candidates_waits_when_assigned_engineer_busy() {
        // #682: if the named engineer is not idle, leave the task in the
        // pool rather than re-routing to a peer. Reassigning defeats the
        // purpose of the `assignee:` hint.
        let tmp = tempfile::tempdir().unwrap();
        write_task_with_assignee(tmp.path(), 60, "for-eng-2", "eng-2");

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("mgr", None),
                engineer_member("eng-1", Some("mgr"), false),
                engineer_member("eng-2", Some("mgr"), false),
            ])
            .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
            .build();

        daemon.enqueue_dispatch_candidates().unwrap();

        assert!(
            daemon.dispatch_queue.is_empty(),
            "task must remain undispatched while named engineer is unavailable"
        );
    }

    #[test]
    fn enqueue_dispatch_candidates_skips_recently_orphan_rescued_task() {
        // #684: after the orphan-rescue path moves an in-progress task back
        // to todo (e.g. the claimer released/parked), dispatch must wait
        // for the cooldown to elapse before re-dispatching. Previously the
        // task bounced straight to a peer within the same tick.
        let tmp = tempfile::tempdir().unwrap();
        write_task_with_body(
            tmp.path(),
            70,
            "rescued-task",
            "todo",
            None,
            "Touch src/team/telemetry_db.rs only.",
        );

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("mgr", None),
                engineer_member("eng-1", Some("mgr"), false),
            ])
            .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
            .build();
        daemon.record_task_rescue(70);

        daemon.enqueue_dispatch_candidates().unwrap();

        assert!(
            daemon.dispatch_queue.is_empty(),
            "task under orphan-rescue cooldown must stay off the dispatch queue"
        );
    }

    #[test]
    fn enqueue_dispatch_candidates_includes_task_after_orphan_rescue_cooldown_expires() {
        // #684: once the cooldown passes the task becomes eligible again.
        let tmp = tempfile::tempdir().unwrap();
        write_task_with_body(
            tmp.path(),
            71,
            "expired-rescue",
            "todo",
            None,
            "Touch src/team/telemetry_db.rs only.",
        );

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("mgr", None),
                engineer_member("eng-1", Some("mgr"), false),
            ])
            .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
            .build();
        // Force cooldown to 0 so the task is immediately eligible.
        daemon.config.team_config.board.orphan_rescue_cooldown_secs = 0;
        daemon.record_task_rescue(71);

        daemon.enqueue_dispatch_candidates().unwrap();

        assert_eq!(daemon.dispatch_queue.len(), 1);
        assert_eq!(daemon.dispatch_queue[0].task_id, 71);
    }

    #[test]
    fn record_task_rescue_grows_cooldown_exponentially_on_repeat() {
        // #686: repeated rescues of the same task should widen the
        // effective dispatch-cooldown window (1×, 2×, 4×, 8×, 16× cap)
        // so the engine doesn't cascade a task across every idle peer
        // every base window.
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("mgr", None),
                engineer_member("eng-1", Some("mgr"), false),
            ])
            .build();

        daemon.record_task_rescue(99);
        let first = daemon.recently_rescued_tasks[&99];
        assert_eq!(first.count, 1);

        // Rescue again while still active — count must grow.
        daemon.record_task_rescue(99);
        let second = daemon.recently_rescued_tasks[&99];
        assert_eq!(second.count, 2);

        daemon.record_task_rescue(99);
        let third = daemon.recently_rescued_tasks[&99];
        assert_eq!(third.count, 3);

        // Effective cooldown doubles each rescue up to the 16× cap.
        let base = std::time::Duration::from_secs(100);
        assert_eq!(
            third.effective_cooldown(base),
            std::time::Duration::from_secs(400)
        );

        // Simulate many rescues — multiplier caps at 16×.
        for _ in 0..10 {
            daemon.record_task_rescue(99);
        }
        let capped = daemon.recently_rescued_tasks[&99];
        assert_eq!(
            capped.effective_cooldown(base),
            std::time::Duration::from_secs(1600)
        );
    }

    #[test]
    fn record_task_rescue_grows_count_across_dispatch_gate_openings() {
        // #689 regression: the dispatch cooldown gates dispatch, so the
        // next rescue always fires *after* the effective_cooldown has
        // elapsed. The old `is_active`-gated growth check therefore never
        // triggered in production — count reset to 1 on every rescue and
        // the exponential backoff flatlined at base. Here we simulate
        // that by backdating `last_rescued_at` past the dispatch gate
        // but still within the cascade-observation window.
        use std::time::Duration;
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("mgr", None),
                engineer_member("eng-1", Some("mgr"), false),
            ])
            .build();
        // 100s base cooldown → count=1 effective_cooldown=100s, cascade_window=200s.
        daemon.config.team_config.board.orphan_rescue_cooldown_secs = 100;

        daemon.record_task_rescue(42);
        // Simulate 150s elapsed — past the 100s dispatch gate (so a
        // re-dispatch happens and the rescued engineer quickly releases)
        // but still inside the 200s cascade window.
        let record = daemon.recently_rescued_tasks.get_mut(&42).unwrap();
        record.last_rescued_at = std::time::Instant::now() - Duration::from_secs(150);

        daemon.record_task_rescue(42);
        let grown = daemon.recently_rescued_tasks[&42];
        assert_eq!(
            grown.count, 2,
            "rescue after gate-open but inside cascade window must grow the counter"
        );

        // Same scenario but past the cascade window → counter resets.
        daemon.record_task_rescue(77);
        let record = daemon.recently_rescued_tasks.get_mut(&77).unwrap();
        // cascade_window at count=1 is 2× base = 200s; go well past it.
        record.last_rescued_at = std::time::Instant::now() - Duration::from_secs(500);

        daemon.record_task_rescue(77);
        let reset = daemon.recently_rescued_tasks[&77];
        assert_eq!(
            reset.count, 1,
            "rescue past cascade window is a new cascade — counter resets"
        );
    }

    #[test]
    fn enqueue_dispatch_candidates_serializes_overlapping_task_and_enqueues_non_overlapping_task() {
        let tmp = tempfile::tempdir().unwrap();
        write_task_with_body(
            tmp.path(),
            10,
            "active-overlap",
            "in-progress",
            Some("eng-2"),
            "Modify src/team/dispatch/queue.rs and tests.",
        );
        write_task_with_body(
            tmp.path(),
            11,
            "candidate-overlap",
            "todo",
            None,
            "Update src/team/dispatch/queue.rs overlap logic.",
        );
        write_task_with_body(
            tmp.path(),
            12,
            "candidate-safe",
            "todo",
            None,
            "Touch src/team/telemetry_db.rs only.",
        );

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("mgr", None),
                engineer_member("eng-1", Some("mgr"), false),
                engineer_member("eng-2", Some("mgr"), false),
            ])
            .states(HashMap::from([
                ("eng-1".to_string(), MemberState::Idle),
                ("eng-2".to_string(), MemberState::Working),
            ]))
            .build();

        daemon.enqueue_dispatch_candidates().unwrap();

        assert_eq!(daemon.dispatch_queue.len(), 1);
        assert_eq!(daemon.dispatch_queue[0].task_id, 12);

        let task = crate::task::Task::from_file(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks")
                .join("011-candidate-overlap.md"),
        )
        .unwrap();
        assert_eq!(task.depends_on, vec![10]);

        let events =
            crate::team::events::read_events(&crate::team::team_events_path(tmp.path())).unwrap();
        assert!(events.iter().any(|event| {
            event.event == "dispatch_overlap_skipped" && event.task.as_deref() == Some("11")
        }));
    }

    #[test]
    fn enqueue_dispatch_candidates_leaves_serialized_task_unqueued_when_no_safe_alternative_exists()
    {
        let tmp = tempfile::tempdir().unwrap();
        write_task_with_body(
            tmp.path(),
            20,
            "active-overlap",
            "in-progress",
            Some("eng-2"),
            "Modify src/team/dispatch/mod.rs.",
        );
        write_task_with_body(
            tmp.path(),
            21,
            "candidate-overlap",
            "todo",
            None,
            "Also update src/team/dispatch/mod.rs for prevention logic.",
        );

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("mgr", None),
                engineer_member("eng-1", Some("mgr"), false),
                engineer_member("eng-2", Some("mgr"), false),
            ])
            .states(HashMap::from([
                ("eng-1".to_string(), MemberState::Idle),
                ("eng-2".to_string(), MemberState::Working),
            ]))
            .build();

        daemon.enqueue_dispatch_candidates().unwrap();

        assert!(daemon.dispatch_queue.is_empty());
        let task = crate::task::Task::from_file(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks")
                .join("021-candidate-overlap.md"),
        )
        .unwrap();
        assert_eq!(task.depends_on, vec![20]);
    }

    #[test]
    fn test_predicted_files_from_body() {
        let tmp = tempfile::tempdir().unwrap();
        write_task_with_body(
            tmp.path(),
            30,
            "body-path",
            "todo",
            None,
            "Update src/team/daemon.rs to add the new check.",
        );
        let task = crate::task::Task::from_file(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks")
                .join("030-body-path.md"),
        )
        .unwrap();

        assert!(predicted_files(&task, tmp.path()).contains(&"src/team/daemon.rs".to_string()));
    }

    #[test]
    fn test_predicted_files_from_frontmatter_files() {
        let tmp = tempfile::tempdir().unwrap();
        write_task_with_files(
            tmp.path(),
            31,
            "frontmatter-paths",
            "todo",
            None,
            &["src/app.rs", "src/**/*.rs"],
            "Use the declared file list.",
        );
        let task = crate::task::Task::from_file(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks")
                .join("031-frontmatter-paths.md"),
        )
        .unwrap();

        let predicted = predicted_files(&task, tmp.path());
        assert!(predicted.contains(&"src/**/*.rs".to_string()));
        assert!(predicted.contains(&"src/app.rs".to_string()));
    }

    #[test]
    fn test_find_overlapping_with_frontmatter_glob() {
        let tmp = tempfile::tempdir().unwrap();
        write_task_with_files(
            tmp.path(),
            40,
            "active-glob",
            "in-progress",
            Some("eng-2"),
            &["src/**/*.rs"],
            "Broad source lock.",
        );
        write_task_with_body(
            tmp.path(),
            41,
            "candidate-file",
            "todo",
            None,
            "Change src/app.rs only.",
        );

        let mut active = None;
        let mut candidate = None;
        for task in crate::task::load_tasks_from_dir(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks"),
        )
        .unwrap()
        {
            match task.id {
                40 => active = Some(task),
                41 => candidate = Some(task),
                _ => {}
            }
        }

        let conflicts = find_overlapping_tasks(
            &candidate.expect("candidate task"),
            &[active.expect("active task")],
            tmp.path(),
        );
        assert_eq!(conflicts.len(), 1);
        assert_eq!(
            conflicts[0].conflicting_files,
            vec!["src/app.rs".to_string()]
        );
    }

    #[test]
    fn test_predicted_files_from_tags() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join("001-prior-shim.md"),
            "---\nid: 1\ntitle: prior shim\nstatus: done\npriority: high\nclaimed_by: eng-2\ntags:\n  - shim\nchanged_paths:\n  - src/shim/runtime.rs\nclass: standard\n---\n\nEarlier shim work.\n",
        )
        .unwrap();
        std::fs::write(
            tasks_dir.join("031-new-shim.md"),
            "---\nid: 31\ntitle: new shim\nstatus: todo\npriority: high\ntags:\n  - shim\nclass: standard\n---\n\nNo explicit paths.\n",
        )
        .unwrap();
        let task = crate::task::Task::from_file(&tasks_dir.join("031-new-shim.md")).unwrap();

        assert!(predicted_files(&task, tmp.path()).contains(&"src/shim/runtime.rs".to_string()));
    }

    #[test]
    fn test_predicted_files_empty() {
        let tmp = tempfile::tempdir().unwrap();
        write_task_with_body(
            tmp.path(),
            32,
            "no-hints",
            "todo",
            None,
            "No file hints here.",
        );
        let task = crate::task::Task::from_file(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks")
                .join("032-no-hints.md"),
        )
        .unwrap();

        assert!(predicted_files(&task, tmp.path()).is_empty());
    }

    #[test]
    fn test_find_overlapping_no_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        write_task_with_body(
            tmp.path(),
            40,
            "candidate",
            "todo",
            None,
            "Edit src/team/daemon.rs.",
        );
        write_task_with_body(
            tmp.path(),
            41,
            "active",
            "in-progress",
            Some("eng-2"),
            "Edit src/team/status.rs.",
        );
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        let candidate = crate::task::Task::from_file(&tasks_dir.join("040-candidate.md")).unwrap();
        let active = crate::task::Task::from_file(&tasks_dir.join("041-active.md")).unwrap();

        assert!(find_overlapping_tasks(&candidate, &[active], tmp.path()).is_empty());
    }

    #[test]
    fn test_find_overlapping_with_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        write_task_with_body(
            tmp.path(),
            42,
            "candidate",
            "todo",
            None,
            "Edit src/team/daemon.rs.",
        );
        write_task_with_body(
            tmp.path(),
            43,
            "active",
            "in-progress",
            Some("eng-2"),
            "Edit src/team/daemon.rs too.",
        );
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        let candidate = crate::task::Task::from_file(&tasks_dir.join("042-candidate.md")).unwrap();
        let active = crate::task::Task::from_file(&tasks_dir.join("043-active.md")).unwrap();

        let conflicts = find_overlapping_tasks(&candidate, &[active], tmp.path());
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].task_id, "43");
    }

    #[test]
    fn test_find_overlapping_multiple_conflicts() {
        let tmp = tempfile::tempdir().unwrap();
        write_task_with_body(
            tmp.path(),
            44,
            "candidate",
            "todo",
            None,
            "Edit src/team/daemon.rs.",
        );
        write_task_with_body(
            tmp.path(),
            45,
            "active-a",
            "in-progress",
            Some("eng-2"),
            "Edit src/team/daemon.rs too.",
        );
        write_task_with_body(
            tmp.path(),
            46,
            "active-b",
            "in-progress",
            Some("eng-3"),
            "Also touch src/team/daemon.rs.",
        );
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        let candidate = crate::task::Task::from_file(&tasks_dir.join("044-candidate.md")).unwrap();
        let active_a = crate::task::Task::from_file(&tasks_dir.join("045-active-a.md")).unwrap();
        let active_b = crate::task::Task::from_file(&tasks_dir.join("046-active-b.md")).unwrap();

        let conflicts = find_overlapping_tasks(&candidate, &[active_a, active_b], tmp.path());
        assert_eq!(conflicts.len(), 2);
    }

    #[test]
    fn test_dispatch_skips_overlapping_candidate() {
        let tmp = tempfile::tempdir().unwrap();
        write_task_with_body(
            tmp.path(),
            50,
            "active",
            "in-progress",
            Some("eng-2"),
            "Edit src/team/daemon.rs.",
        );
        write_task_with_body(
            tmp.path(),
            51,
            "overlap",
            "todo",
            None,
            "Edit src/team/daemon.rs.",
        );
        write_task_with_body(
            tmp.path(),
            52,
            "safe",
            "todo",
            None,
            "Edit src/team/status.rs.",
        );

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("mgr", None),
                engineer_member("eng-1", Some("mgr"), false),
                engineer_member("eng-2", Some("mgr"), false),
            ])
            .states(HashMap::from([
                ("eng-1".to_string(), MemberState::Idle),
                ("eng-2".to_string(), MemberState::Working),
            ]))
            .build();

        daemon.enqueue_dispatch_candidates().unwrap();
        assert_eq!(daemon.dispatch_queue.len(), 1);
        assert_eq!(daemon.dispatch_queue[0].task_id, 52);
    }

    #[test]
    fn enqueue_dispatch_candidates_skips_benched_engineer() {
        let tmp = tempfile::tempdir().unwrap();
        write_open_task_file(tmp.path(), 70, "dispatchable", "todo");
        write_bench_test_team_config(tmp.path(), 2);

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                engineer_member("eng-1", None, false),
                engineer_member("eng-2", None, false),
            ])
            .states(HashMap::from([
                ("eng-1".to_string(), MemberState::Idle),
                ("eng-2".to_string(), MemberState::Idle),
            ]))
            .build();

        crate::team::bench::bench_engineer(tmp.path(), "eng-1", Some("session end")).unwrap();

        daemon.enqueue_dispatch_candidates().unwrap();
        assert_eq!(daemon.dispatch_queue.len(), 1);
        assert_eq!(daemon.dispatch_queue[0].engineer, "eng-2");
        assert_eq!(daemon.dispatch_queue[0].task_id, 70);
    }

    /// #674 defect 2: dispatch selection must skip engineers whose backend
    /// is parked (quota_exhausted with future retry_at), regardless of
    /// cached health state. Without this gate, the stall-timer reclaim
    /// cascade rotates tasks across every quota-blocked engineer every
    /// 15 minutes, producing board churn with zero real progress.
    #[test]
    fn enqueue_dispatch_candidates_skips_quota_parked_engineer() {
        let tmp = tempfile::tempdir().unwrap();
        write_open_task_file(tmp.path(), 674, "dispatchable", "todo");

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                engineer_member("eng-1", None, false),
                engineer_member("eng-2", None, false),
            ])
            .states(HashMap::from([
                ("eng-1".to_string(), MemberState::Idle),
                ("eng-2".to_string(), MemberState::Idle),
            ]))
            .build();

        // eng-1 is quota-parked via future retry_at (32h out). Its cached
        // health value is intentionally left as the default (Healthy) to
        // prove the retry_at deadline alone is sufficient to park it.
        let future_deadline = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or(std::time::Duration::ZERO)
            .as_secs()
            + 32 * 3600;
        daemon
            .backend_quota_retry_at
            .insert("eng-1".to_string(), future_deadline);

        daemon.enqueue_dispatch_candidates().unwrap();
        assert_eq!(daemon.dispatch_queue.len(), 1);
        assert_eq!(
            daemon.dispatch_queue[0].engineer, "eng-2",
            "quota-parked engineer must be skipped even if cached health is Healthy"
        );
        assert_eq!(daemon.dispatch_queue[0].task_id, 674);
    }

    #[test]
    fn unbench_restores_dispatch_eligibility() {
        let tmp = tempfile::tempdir().unwrap();
        write_open_task_file(tmp.path(), 71, "dispatchable", "todo");
        write_bench_test_team_config(tmp.path(), 2);

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                engineer_member("eng-1", None, false),
                engineer_member("eng-2", None, false),
            ])
            .states(HashMap::from([
                ("eng-1".to_string(), MemberState::Idle),
                ("eng-2".to_string(), MemberState::Idle),
            ]))
            .build();

        crate::team::bench::bench_engineer(tmp.path(), "eng-1", Some("pause")).unwrap();
        daemon.enqueue_dispatch_candidates().unwrap();
        assert_eq!(daemon.dispatch_queue.len(), 1);
        assert_eq!(daemon.dispatch_queue[0].engineer, "eng-2");

        crate::team::bench::unbench_engineer(tmp.path(), "eng-1").unwrap();
        daemon.dispatch_queue.clear();
        daemon.enqueue_dispatch_candidates().unwrap();
        assert_eq!(daemon.dispatch_queue.len(), 1);
        assert_eq!(daemon.dispatch_queue[0].engineer, "eng-1");
        assert_eq!(daemon.dispatch_queue[0].task_id, 71);
    }

    #[test]
    fn test_dispatch_all_overlap_picks_least_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        write_task_with_body(
            tmp.path(),
            60,
            "active-daemon",
            "in-progress",
            Some("eng-2"),
            "Edit src/team/daemon.rs.",
        );
        write_task_with_body(
            tmp.path(),
            61,
            "active-status",
            "in-progress",
            Some("eng-3"),
            "Edit src/team/status.rs.",
        );
        write_task_with_body(
            tmp.path(),
            62,
            "overlap-both",
            "todo",
            None,
            "Edit src/team/daemon.rs and src/team/status.rs.",
        );
        write_task_with_body(
            tmp.path(),
            63,
            "overlap-one",
            "todo",
            None,
            "Edit src/team/daemon.rs only.",
        );

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("mgr", None),
                engineer_member("eng-1", Some("mgr"), false),
                engineer_member("eng-2", Some("mgr"), false),
                engineer_member("eng-3", Some("mgr"), false),
            ])
            .states(HashMap::from([
                ("eng-1".to_string(), MemberState::Idle),
                ("eng-2".to_string(), MemberState::Working),
                ("eng-3".to_string(), MemberState::Working),
            ]))
            .build();

        daemon.enqueue_dispatch_candidates().unwrap();
        assert!(daemon.dispatch_queue.is_empty());
        let task = crate::task::Task::from_file(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks")
                .join("063-overlap-one.md"),
        )
        .unwrap();
        assert_eq!(task.depends_on, vec![60]);
    }

    #[test]
    fn test_overlap_conflict_struct_fields() {
        let conflict = OverlapConflict {
            task_id: "42".to_string(),
            conflicting_files: vec!["src/team/daemon.rs".to_string()],
            in_progress_engineer: "eng-2".to_string(),
        };
        assert_eq!(conflict.task_id, "42");
        assert_eq!(conflict.conflicting_files, vec!["src/team/daemon.rs"]);
        assert_eq!(conflict.in_progress_engineer, "eng-2");
    }

    #[test]
    fn file_level_locks_defer_then_release_overlapping_work_for_worktree_teams() {
        let tmp = tempfile::tempdir().unwrap();
        write_task_with_body(
            tmp.path(),
            60,
            "active-overlap",
            "in-progress",
            Some("eng-2"),
            "Modify src/app.rs only.",
        );
        write_task_with_files(
            tmp.path(),
            61,
            "waiting-overlap",
            "todo",
            None,
            &["src/*.rs"],
            "Lock should wait without rewriting dependencies.",
        );

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("mgr", None),
                engineer_member("eng-1", Some("mgr"), true),
                engineer_member("eng-2", Some("mgr"), true),
            ])
            .workflow_policy(crate::team::config::WorkflowPolicy {
                file_level_locks: true,
                ..crate::team::config::WorkflowPolicy::default()
            })
            .states(HashMap::from([
                ("eng-1".to_string(), MemberState::Idle),
                ("eng-2".to_string(), MemberState::Working),
            ]))
            .build();

        daemon.enqueue_dispatch_candidates().unwrap();
        assert!(daemon.dispatch_queue.is_empty());

        let waiting_task = crate::task::Task::from_file(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks")
                .join("061-waiting-overlap.md"),
        )
        .unwrap();
        assert!(
            waiting_task.depends_on.is_empty(),
            "file-level wait should not rewrite task dependencies"
        );

        write_task_with_body(
            tmp.path(),
            60,
            "active-overlap",
            "done",
            Some("eng-2"),
            "Modify src/app.rs only.",
        );

        daemon.enqueue_dispatch_candidates().unwrap();
        assert_eq!(daemon.dispatch_queue.len(), 1);
        assert_eq!(daemon.dispatch_queue[0].task_id, 61);
    }
}
