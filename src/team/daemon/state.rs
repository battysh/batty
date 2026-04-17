//! Daemon state persistence — save/load/restore runtime state across restarts.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::dispatch::DispatchQueueEntry;
use super::{TeamDaemon, now_unix, standup};
use crate::team::standup::MemberState;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct PersistedNudgeState {
    pub idle_elapsed_secs: Option<u64>,
    pub fired_this_idle: bool,
    pub paused: bool,
}

/// #689: per-task orphan-rescue record persisted across daemon restarts.
/// Without this, a restart in the middle of a rescue cascade wipes the
/// exponential-backoff counter back to 1 and the cascade resumes at the
/// base cooldown.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct PersistedRescueRecord {
    pub last_rescued_elapsed_secs: u64,
    pub count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct PersistedDaemonState {
    pub clean_shutdown: bool,
    pub saved_at: u64,
    pub states: HashMap<String, MemberState>,
    pub active_tasks: HashMap<String, u32>,
    pub retry_counts: HashMap<String, u32>,
    #[serde(default)]
    pub discord_event_cursor: usize,
    #[serde(default)]
    pub dispatch_queue: Vec<DispatchQueueEntry>,
    pub paused_standups: HashSet<String>,
    pub last_standup_elapsed_secs: HashMap<String, u64>,
    pub nudge_state: HashMap<String, PersistedNudgeState>,
    pub pipeline_starvation_fired: bool,
    #[serde(default)]
    pub optional_subsystem_backoff: HashMap<String, u32>,
    #[serde(default)]
    pub optional_subsystem_disabled_remaining_secs: HashMap<String, u64>,
    /// #687: seconds since `planning_cycle_last_fired` at save time (so the
    /// cooldown window survives a daemon restart). None when the cycle has
    /// never fired.
    #[serde(default)]
    pub planning_cycle_last_fired_elapsed_secs: Option<u64>,
    /// #687: consecutive empty planning cycles at save time (drives the
    /// architect planning-cadence backoff). Resets to 0 on any non-empty
    /// cycle. Must survive restart or the backoff is lost.
    #[serde(default)]
    pub planning_cycle_consecutive_empty: u32,
    /// #689: per-task orphan-rescue records. Persisted so the
    /// exponential-backoff counter (and therefore the quiet-time a
    /// cascaded task earns) survives daemon restarts.
    #[serde(default)]
    pub recently_rescued_tasks: HashMap<u32, PersistedRescueRecord>,
}

impl TeamDaemon {
    pub(super) fn restore_runtime_state(&mut self) {
        let Some(state) = load_daemon_state(&self.config.project_root) else {
            return;
        };

        self.states = state.states;
        self.discord_event_cursor = self.discord_event_cursor.max(state.discord_event_cursor);
        self.idle_started_at = self
            .states
            .iter()
            .filter(|(_, state)| matches!(state, MemberState::Idle))
            .map(|(member, _)| (member.clone(), Instant::now()))
            .collect();
        self.active_tasks = state.active_tasks;
        self.retry_counts = state.retry_counts;
        self.discord_event_cursor = state.discord_event_cursor;
        // #694: do NOT restore dispatch_queue across restarts. A queue entry
        // carries a (task_id, engineer) routing decision frozen at enqueue
        // time. When the daemon binary is upgraded to fix a routing bug, any
        // entries enqueued under the buggy binary would still be delivered by
        // the new binary — silently undoing the fix. Observed after v0.11.32
        // deploy: task #552 (tagged `kai-devrel`) was delivered to
        // sam-designer-1-1 seconds after restart because the stale queue
        // entry from a pre-upgrade binary was replayed. `enqueue_dispatch_candidates`
        // re-populates the queue from current board state within one tick, so
        // dropping it on restore costs at most a brief delay.
        let _discarded_dispatch_queue = state.dispatch_queue;
        self.paused_standups = state.paused_standups;
        self.last_standup = standup::restore_timer_state(state.last_standup_elapsed_secs);

        for (member_name, persisted) in state.nudge_state {
            let Some(schedule) = self.nudges.get_mut(&member_name) else {
                continue;
            };
            schedule.idle_since = persisted.idle_elapsed_secs.map(|elapsed_secs| {
                Instant::now()
                    .checked_sub(Duration::from_secs(elapsed_secs))
                    .unwrap_or_else(Instant::now)
            });
            schedule.fired_this_idle = persisted.fired_this_idle;
            schedule.paused = persisted.paused;
        }
        self.pipeline_starvation_fired = state.pipeline_starvation_fired;
        self.restore_optional_subsystem_budget_state(
            &state.optional_subsystem_backoff,
            &state.optional_subsystem_disabled_remaining_secs,
        );
        // #687: restore planning-cycle backoff state across restarts.
        // Without this, a daemon that had backed off to 6× cadence would
        // reset to 1× on restart and immediately fire an architect
        // planning cycle seconds later — re-wasting orchestrator tokens
        // on an empty board the previous daemon had already learned was
        // stuck.
        self.planning_cycle_consecutive_empty = state.planning_cycle_consecutive_empty;
        self.planning_cycle_last_fired =
            state.planning_cycle_last_fired_elapsed_secs.map(|elapsed_secs| {
                Instant::now()
                    .checked_sub(Duration::from_secs(elapsed_secs))
                    .unwrap_or_else(Instant::now)
            });
        // #689: restore orphan-rescue records so the exponential-backoff
        // counter survives a daemon restart mid-cascade.
        self.recently_rescued_tasks = state
            .recently_rescued_tasks
            .into_iter()
            .map(|(task_id, persisted)| {
                let last_rescued_at = Instant::now()
                    .checked_sub(Duration::from_secs(persisted.last_rescued_elapsed_secs))
                    .unwrap_or_else(Instant::now);
                (
                    task_id,
                    super::RescueRecord {
                        last_rescued_at,
                        count: persisted.count,
                    },
                )
            })
            .collect();
    }

    pub(super) fn persist_runtime_state(&self, clean_shutdown: bool) -> Result<()> {
        let optional_subsystem_backoff = self.snapshot_optional_subsystem_backoff();
        let optional_subsystem_disabled_remaining_secs =
            self.snapshot_optional_subsystem_disabled_remaining_secs();
        let state = PersistedDaemonState {
            clean_shutdown,
            saved_at: now_unix(),
            states: self.states.clone(),
            active_tasks: self.active_tasks.clone(),
            retry_counts: self.retry_counts.clone(),
            discord_event_cursor: self.discord_event_cursor,
            dispatch_queue: self.dispatch_queue.clone(),
            paused_standups: self.paused_standups.clone(),
            last_standup_elapsed_secs: standup::snapshot_timer_state(&self.last_standup),
            nudge_state: self
                .nudges
                .iter()
                .map(|(member, schedule)| {
                    (
                        member.clone(),
                        PersistedNudgeState {
                            idle_elapsed_secs: schedule.idle_since.map(|t| t.elapsed().as_secs()),
                            fired_this_idle: schedule.fired_this_idle,
                            paused: schedule.paused,
                        },
                    )
                })
                .collect(),
            pipeline_starvation_fired: self.pipeline_starvation_fired,
            optional_subsystem_backoff,
            optional_subsystem_disabled_remaining_secs,
            // #687: snapshot the architect planning-cycle backoff so it
            // survives a daemon restart.
            planning_cycle_last_fired_elapsed_secs: self
                .planning_cycle_last_fired
                .map(|t| t.elapsed().as_secs()),
            planning_cycle_consecutive_empty: self.planning_cycle_consecutive_empty,
            recently_rescued_tasks: self
                .recently_rescued_tasks
                .iter()
                .map(|(task_id, record)| {
                    (
                        *task_id,
                        PersistedRescueRecord {
                            last_rescued_elapsed_secs: record.last_rescued_at.elapsed().as_secs(),
                            count: record.count,
                        },
                    )
                })
                .collect(),
        };
        save_daemon_state(&self.config.project_root, &state)
    }
}

pub(super) fn daemon_state_path(project_root: &std::path::Path) -> PathBuf {
    super::super::daemon_state_path(project_root)
}

pub(super) fn load_daemon_state(project_root: &std::path::Path) -> Option<PersistedDaemonState> {
    let path = daemon_state_path(project_root);
    let Ok(content) = fs::read_to_string(&path) else {
        return None;
    };

    match serde_json::from_str(&content) {
        Ok(state) => Some(state),
        Err(error) => {
            warn!(path = %path.display(), error = %error, "failed to parse daemon state, ignoring");
            None
        }
    }
}

pub fn load_dispatch_queue_snapshot(project_root: &std::path::Path) -> Vec<DispatchQueueEntry> {
    load_daemon_state(project_root)
        .map(|state| state.dispatch_queue)
        .unwrap_or_default()
}

pub(super) fn save_daemon_state(
    project_root: &std::path::Path,
    state: &PersistedDaemonState,
) -> Result<()> {
    let path = daemon_state_path(project_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let content =
        serde_json::to_string_pretty(state).context("failed to serialize daemon state")?;
    fs::write(&path, content).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}
