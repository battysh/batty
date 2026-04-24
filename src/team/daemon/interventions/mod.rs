//! Idle nudge and intervention automation extracted from the daemon.
//!
//! This module keeps the daemon poll loop readable by isolating the logic that
//! decides when to nudge idle members or escalate stalled ownership, review,
//! dispatch-gap, and utilization conditions. It operates on `TeamDaemon`
//! state directly, but it is intentionally limited to automation decisions and
//! message delivery side effects rather than broader daemon orchestration.

mod board_replenishment;
mod dispatch;
mod nudge;
mod owned_tasks;
mod review;
mod triage;
mod utilization;

use std::path::Path;
use std::time::{Duration, Instant};

use tracing::warn;

use super::*;
use crate::team::config::{PlanningDirectiveFile, load_planning_directive};
use crate::team::supervisory_notice::{
    SupervisoryPressure, classify_supervisory_pressure_normalized, normalized_body,
};

const DIRECTIVE_MAX_CHARS: usize = 2_000;

#[derive(Debug, Clone)]
pub(crate) struct NudgeSchedule {
    pub(crate) text: String,
    pub(crate) interval: Duration,
    pub(crate) idle_since: Option<Instant>,
    pub(crate) fired_this_idle: bool,
    pub(crate) paused: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct OwnedTaskInterventionState {
    pub(crate) idle_epoch: u64,
    pub(crate) signature: String,
    pub(crate) detected_at: Instant,
    pub(crate) escalation_sent: bool,
}

impl TeamDaemon {
    fn prepend_planning_directive(
        &self,
        directive: PlanningDirectiveFile,
        heading: &str,
        message: String,
    ) -> String {
        match load_planning_directive(&self.config.project_root, directive, DIRECTIVE_MAX_CHARS) {
            Ok(Some(content)) => format!("{heading}\n{content}\n\n{message}"),
            Ok(None) => message,
            Err(error) => {
                warn!(directive = directive.file_name(), error = %error, "failed to load planning directive");
                message
            }
        }
    }

    pub(super) fn update_nudge_for_state(&mut self, member_name: &str, new_state: MemberState) {
        if let Some(schedule) = self.nudges.get_mut(member_name) {
            match new_state {
                MemberState::Idle => {
                    if schedule.paused || schedule.idle_since.is_none() {
                        schedule.idle_since = Some(Instant::now());
                        schedule.fired_this_idle = false;
                    }
                    schedule.paused = false;
                }
                MemberState::Working => {
                    schedule.idle_since = None;
                    schedule.fired_this_idle = false;
                    schedule.paused = true;
                }
            }
        }
    }

    pub(super) fn update_triage_intervention_for_state(
        &mut self,
        member_name: &str,
        new_state: MemberState,
    ) {
        match new_state {
            MemberState::Working => {
                self.triage_idle_epochs
                    .entry(member_name.to_string())
                    .or_insert(0);
            }
            MemberState::Idle => {
                let had_epoch = self.triage_idle_epochs.contains_key(member_name);
                let epoch = self
                    .triage_idle_epochs
                    .entry(member_name.to_string())
                    .or_insert(0);
                if had_epoch {
                    *epoch += 1;
                }
            }
        }
    }

    pub(super) fn automation_idle_grace_duration(&self) -> Duration {
        Duration::from_secs(
            self.config
                .team_config
                .automation
                .intervention_idle_grace_secs,
        )
    }

    fn automation_idle_grace_elapsed(&self, member_name: &str) -> bool {
        let grace = self.automation_idle_grace_duration();
        self.idle_started_at
            .get(member_name)
            .is_some_and(|started_at| started_at.elapsed() >= grace)
    }

    fn member_has_pending_inbox(&self, inbox_root: &Path, member_name: &str) -> bool {
        let role_type = self
            .config
            .members
            .iter()
            .find(|member| member.name == member_name)
            .map(|member| member.role_type);
        match role_type {
            Some(RoleType::Architect | RoleType::Manager) => {
                match crate::team::inbox_tiered::pending_messages_union(inbox_root, member_name) {
                    Ok(messages) => messages.into_iter().any(|message| {
                        !matches!(
                            classify_supervisory_pressure_normalized(&normalized_body(
                                &message.body
                            )),
                            Some(
                                SupervisoryPressure::StatusUpdate
                                    | SupervisoryPressure::ResolvedUpdate
                                    | SupervisoryPressure::RecoveryUpdate
                                    | SupervisoryPressure::IdleNudge
                                    | SupervisoryPressure::ReviewNudge
                            )
                        )
                    }),
                    Err(error) => {
                        warn!(member = %member_name, error = %error, "failed to read supervisory inbox before automation");
                        true
                    }
                }
            }
            _ => match crate::team::inbox_tiered::pending_message_count_union(
                inbox_root,
                member_name,
            ) {
                Ok(count) => count > 0,
                Err(error) => {
                    warn!(member = %member_name, error = %error, "failed to count pending inbox before automation");
                    true
                }
            },
        }
    }

    fn ready_for_idle_automation(&self, inbox_root: &Path, member_name: &str) -> bool {
        self.automation_idle_grace_elapsed(member_name)
            && !self.member_has_pending_inbox(inbox_root, member_name)
    }

    pub(in crate::team::daemon) fn intervention_on_cooldown(&self, key: &str) -> bool {
        let cooldown = Duration::from_secs(
            self.config
                .team_config
                .automation
                .intervention_cooldown_secs,
        );
        self.intervention_cooldowns
            .get(key)
            .is_some_and(|fired_at| fired_at.elapsed() < cooldown)
    }

    fn utilization_intervention_on_cooldown(&self, key: &str) -> bool {
        let cooldown = Duration::from_secs(
            self.config
                .team_config
                .automation
                .utilization_recovery_interval_secs,
        );
        self.intervention_cooldowns
            .get(key)
            .is_some_and(|fired_at| fired_at.elapsed() < cooldown)
    }

    fn is_member_idle(&self, member_name: &str) -> bool {
        self.watchers
            .get(member_name)
            .map(|watcher| matches!(watcher.state, WatcherState::Ready | WatcherState::Idle))
            .unwrap_or(matches!(
                self.states.get(member_name),
                Some(MemberState::Idle) | None
            ))
    }
}

pub(super) fn task_needs_owned_intervention(status: &str) -> bool {
    !matches!(status, "review" | "done" | "archived")
}

/// Returns true if every engineer has at least one in-progress task claimed by them.
/// Used to suppress false-positive starvation/utilization alerts when all engineers
/// are actively working but show transient idle state.
pub(super) fn all_engineers_have_active_tasks(
    engineer_names: &[String],
    tasks: &[crate::task::Task],
) -> bool {
    !engineer_names.is_empty()
        && engineer_names.iter().all(|name| {
            tasks.iter().any(|task| {
                task.claimed_by.as_deref() == Some(name.as_str())
                    && matches!(task.status.as_str(), "in-progress" | "in_progress")
            })
        })
}

#[cfg(test)]
mod tests;
