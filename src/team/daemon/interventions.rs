//! Idle nudge and intervention automation extracted from the daemon.
//!
//! This module keeps the daemon poll loop readable by isolating the logic that
//! decides when to nudge idle members or escalate stalled ownership, review,
//! dispatch-gap, and utilization conditions. It operates on `TeamDaemon`
//! state directly, but it is intentionally limited to automation decisions and
//! message delivery side effects rather than broader daemon orchestration.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{info, warn};

use super::*;

#[derive(Debug, Clone)]
pub(crate) struct NudgeSchedule {
    pub(crate) text: String,
    pub(crate) interval: Duration,
    pub(crate) idle_since: Option<Instant>,
    pub(crate) fired_this_idle: bool,
    pub(crate) paused: bool,
}

#[derive(Debug, Clone)]
pub(super) struct OwnedTaskInterventionState {
    pub(super) idle_epoch: u64,
    pub(super) signature: String,
    pub(super) detected_at: Instant,
    pub(super) escalation_sent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReportDispatchSnapshot {
    name: String,
    is_working: bool,
    active_task_ids: Vec<u32>,
}

impl TeamDaemon {
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
        match inbox::pending_message_count(inbox_root, member_name) {
            Ok(count) => count > 0,
            Err(error) => {
                warn!(member = %member_name, error = %error, "failed to count pending inbox before automation");
                true
            }
        }
    }

    fn ready_for_idle_automation(&self, inbox_root: &Path, member_name: &str) -> bool {
        self.automation_idle_grace_elapsed(member_name)
            && !self.member_has_pending_inbox(inbox_root, member_name)
    }

    fn intervention_on_cooldown(&self, key: &str) -> bool {
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

    fn is_member_idle(&self, member_name: &str) -> bool {
        self.watchers
            .get(member_name)
            .map(|watcher| matches!(watcher.state, WatcherState::Idle))
            .unwrap_or(matches!(
                self.states.get(member_name),
                Some(MemberState::Idle) | None
            ))
    }

    pub(super) fn maybe_fire_nudges(&mut self) -> Result<()> {
        if !self.config.team_config.automation.timeout_nudges {
            return Ok(());
        }
        if super::super::pause_marker_path(&self.config.project_root).exists() {
            return Ok(());
        }
        let inbox_root = inbox::inboxes_root(&self.config.project_root);
        let member_names: Vec<String> = self.nudges.keys().cloned().collect();

        for name in member_names {
            let fire = {
                let schedule = &self.nudges[&name];
                if schedule.fired_this_idle {
                    false
                } else if let Some(idle_since) = schedule.idle_since {
                    idle_since.elapsed()
                        >= schedule.interval.max(self.automation_idle_grace_duration())
                        && self.ready_for_idle_automation(&inbox_root, &name)
                } else {
                    false
                }
            };

            if fire {
                let text = self.nudges[&name].text.clone();
                info!(member = %name, "firing nudge (idle timeout)");
                let delivered_live = match self.queue_daemon_message(&name, &text) {
                    Ok(MessageDelivery::LivePane) => true,
                    Ok(_) => false,
                    Err(error) => {
                        warn!(member = %name, error = %error, "failed to deliver nudge");
                        continue;
                    }
                };
                if let Some(schedule) = self.nudges.get_mut(&name) {
                    schedule.fired_this_idle = true;
                }
                if delivered_live {
                    self.mark_member_working(&name);
                }
            }
        }

        Ok(())
    }

    pub(super) fn maybe_intervene_triage_backlog(&mut self) -> Result<()> {
        if !self.config.team_config.automation.triage_interventions {
            return Ok(());
        }
        if super::super::pause_marker_path(&self.config.project_root).exists() {
            return Ok(());
        }

        let inbox_root = inbox::inboxes_root(&self.config.project_root);
        let direct_reports = super::super::status::direct_reports_by_member(&self.config.members);
        let member_names: Vec<String> = self.config.pane_map.keys().cloned().collect();

        for name in member_names {
            let Some(member) = self
                .config
                .members
                .iter()
                .find(|member| member.name == name)
                .cloned()
            else {
                continue;
            };
            if !self.is_member_idle(&name) {
                continue;
            }
            if !self.ready_for_idle_automation(&inbox_root, &name) {
                continue;
            }

            let Some(reports) = direct_reports.get(&name) else {
                continue;
            };

            let triage_state = match super::super::status::delivered_direct_report_triage_state(
                &inbox_root,
                &name,
                reports,
            ) {
                Ok(state) => state,
                Err(error) => {
                    warn!(member = %name, error = %error, "failed to compute triage intervention state");
                    continue;
                }
            };
            if triage_state.count == 0 {
                continue;
            }

            let idle_epoch = self.triage_idle_epochs.get(&name).copied().unwrap_or(0);
            if idle_epoch == 0 {
                continue;
            }

            let already_notified_for = self.triage_interventions.get(&name).copied().unwrap_or(0);
            if already_notified_for >= idle_epoch {
                continue;
            }

            let triage_cooldown_key = format!("triage::{name}");
            if self.intervention_on_cooldown(&triage_cooldown_key) {
                continue;
            }

            let text = self.build_triage_intervention_message(&member, reports, triage_state.count);
            info!(member = %name, triage_backlog = triage_state.count, "firing triage intervention");
            let delivered_live = match self.queue_daemon_message(&name, &text) {
                Ok(MessageDelivery::LivePane) => true,
                Ok(_) => false,
                Err(error) => {
                    warn!(member = %name, error = %error, "failed to deliver triage intervention");
                    continue;
                }
            };
            self.record_orchestrator_action(format!(
                "recovery: triage intervention for {} with {} pending direct-report result(s)",
                name, triage_state.count
            ));
            self.triage_interventions.insert(name.clone(), idle_epoch);
            self.intervention_cooldowns
                .insert(triage_cooldown_key, Instant::now());
            if delivered_live {
                self.mark_member_working(&name);
            }
        }

        Ok(())
    }

    pub(super) fn maybe_intervene_owned_tasks(&mut self) -> Result<()> {
        if !self.config.team_config.automation.owned_task_interventions {
            return Ok(());
        }
        if super::super::pause_marker_path(&self.config.project_root).exists() {
            return Ok(());
        }

        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let inbox_root = inbox::inboxes_root(&self.config.project_root);
        let tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks"))?;
        let direct_reports = super::super::status::direct_reports_by_member(&self.config.members);
        let member_names: Vec<String> = self.config.pane_map.keys().cloned().collect();

        for name in member_names {
            let Some(member) = self
                .config
                .members
                .iter()
                .find(|member| member.name == name)
                .cloned()
            else {
                continue;
            };
            if !self.is_member_idle(&name) {
                continue;
            }
            let owned_tasks: Vec<&crate::task::Task> = tasks
                .iter()
                .filter(|task| task.claimed_by.as_deref() == Some(name.as_str()))
                .filter(|task| task_needs_owned_intervention(task.status.as_str()))
                .collect();
            if owned_tasks.is_empty() {
                self.owned_task_interventions.remove(&name);
                continue;
            }

            let idle_epoch = self.triage_idle_epochs.get(&name).copied().unwrap_or(0);
            let signature = owned_task_intervention_signature(&owned_tasks);
            if let Some(existing) = self.owned_task_interventions.get(&name) {
                if existing.signature == signature {
                    let stuck_age_secs = existing.detected_at.elapsed().as_secs();
                    let should_escalate = !existing.escalation_sent
                        && super::super::policy::should_escalate(
                            &self.config.team_config.workflow_policy,
                            stuck_age_secs,
                        );
                    if let Some(state) = self.owned_task_interventions.get_mut(&name) {
                        state.idle_epoch = idle_epoch;
                        if !should_escalate {
                            continue;
                        }
                    }

                    let Some(parent) = member.reports_to.clone() else {
                        if let Some(state) = self.owned_task_interventions.get_mut(&name) {
                            state.escalation_sent = true;
                        }
                        continue;
                    };
                    let text = self.build_stuck_task_escalation_message(
                        &member,
                        &owned_tasks,
                        stuck_age_secs,
                    );
                    info!(
                        member = %name,
                        parent = %parent,
                        owned_task_count = owned_tasks.len(),
                        stuck_age_secs,
                        "escalating stuck owned task"
                    );
                    match self.queue_message("daemon", &parent, &text) {
                        Ok(()) => {
                            self.record_orchestrator_action(format!(
                                "recovery: stuck-task escalation for {} to {} after {}s on {} active task(s)",
                                name,
                                parent,
                                stuck_age_secs,
                                owned_tasks.len()
                            ));
                            for task in &owned_tasks {
                                self.record_task_escalated(&name, task.id.to_string());
                            }
                            if let Some(state) = self.owned_task_interventions.get_mut(&name) {
                                state.escalation_sent = true;
                            }
                        }
                        Err(error) => {
                            warn!(member = %name, parent = %parent, error = %error, "failed to escalate stuck task");
                        }
                    }
                    continue;
                }
            }

            if self.intervention_on_cooldown(&name) {
                continue;
            }
            if !self.ready_for_idle_automation(&inbox_root, &name) {
                continue;
            }

            let reports = direct_reports.get(&name).cloned().unwrap_or_default();
            let text = self.build_owned_task_intervention_message(&member, &owned_tasks, &reports);
            info!(
                member = %name,
                owned_task_count = owned_tasks.len(),
                "firing owned-task intervention"
            );
            let delivered_live = match self.queue_daemon_message(&name, &text) {
                Ok(MessageDelivery::LivePane) => true,
                Ok(_) => false,
                Err(error) => {
                    warn!(member = %name, error = %error, "failed to deliver owned-task intervention");
                    continue;
                }
            };
            self.record_orchestrator_action(format!(
                "recovery: owned-task intervention for {} covering {} active task(s)",
                name,
                owned_tasks.len()
            ));
            self.owned_task_interventions.insert(
                name.clone(),
                OwnedTaskInterventionState {
                    idle_epoch,
                    signature,
                    detected_at: Instant::now(),
                    escalation_sent: false,
                },
            );
            self.intervention_cooldowns
                .insert(name.clone(), Instant::now());
            if delivered_live {
                self.mark_member_working(&name);
            }
        }

        Ok(())
    }

    pub(super) fn maybe_intervene_review_backlog(&mut self) -> Result<()> {
        if !self.config.team_config.automation.review_interventions {
            return Ok(());
        }
        if super::super::pause_marker_path(&self.config.project_root).exists() {
            return Ok(());
        }

        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let inbox_root = inbox::inboxes_root(&self.config.project_root);
        let tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks"))?;
        let member_names: Vec<String> = self.config.pane_map.keys().cloned().collect();

        for name in member_names {
            let Some(member) = self
                .config
                .members
                .iter()
                .find(|member| member.name == name)
            else {
                continue;
            };
            if !self.is_member_idle(&name) {
                continue;
            }
            if !self.ready_for_idle_automation(&inbox_root, &name) {
                continue;
            }

            let review_tasks: Vec<&crate::task::Task> = tasks
                .iter()
                .filter(|task| {
                    review_backlog_owner_for_task(task, &self.config.members).as_deref()
                        == Some(name.as_str())
                })
                .collect();
            if review_tasks.is_empty() {
                self.owned_task_interventions
                    .remove(&review_intervention_key(&name));
                continue;
            }

            let idle_epoch = self.triage_idle_epochs.get(&name).copied().unwrap_or(0);
            if idle_epoch == 0 {
                continue;
            }

            let signature = review_task_intervention_signature(&review_tasks);
            let review_key = review_intervention_key(&name);
            if self
                .owned_task_interventions
                .get(&review_key)
                .is_some_and(|state| state.signature == signature)
            {
                continue;
            }
            if self.intervention_on_cooldown(&review_key) {
                continue;
            }

            let text = self.build_review_intervention_message(member, &review_tasks);
            info!(
                member = %name,
                review_task_count = review_tasks.len(),
                "firing review intervention"
            );
            let delivered_live = match self.queue_daemon_message(&name, &text) {
                Ok(MessageDelivery::LivePane) => true,
                Ok(_) => false,
                Err(error) => {
                    warn!(member = %name, error = %error, "failed to deliver review intervention");
                    continue;
                }
            };
            self.record_orchestrator_action(format!(
                "recovery: review intervention for {} covering {} queued review task(s)",
                name,
                review_tasks.len()
            ));
            self.owned_task_interventions.insert(
                review_key.clone(),
                OwnedTaskInterventionState {
                    idle_epoch,
                    signature,
                    detected_at: Instant::now(),
                    escalation_sent: false,
                },
            );
            self.intervention_cooldowns
                .insert(review_key, Instant::now());
            if delivered_live {
                self.mark_member_working(&name);
            }
        }

        Ok(())
    }

    pub(super) fn maybe_intervene_manager_dispatch_gap(&mut self) -> Result<()> {
        if !self
            .config
            .team_config
            .automation
            .manager_dispatch_interventions
        {
            return Ok(());
        }
        if super::super::pause_marker_path(&self.config.project_root).exists() {
            return Ok(());
        }

        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let inbox_root = inbox::inboxes_root(&self.config.project_root);
        let tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks"))?;
        let direct_reports = super::super::status::direct_reports_by_member(&self.config.members);
        let member_names: Vec<String> = self.config.pane_map.keys().cloned().collect();

        for name in member_names {
            let Some(member) = self
                .config
                .members
                .iter()
                .find(|member| member.name == name)
            else {
                continue;
            };
            if member.role_type != RoleType::Manager {
                continue;
            }
            if !self.is_member_idle(&name) {
                continue;
            }
            if !self.ready_for_idle_automation(&inbox_root, &name) {
                continue;
            }

            let Some(reports) = direct_reports.get(&name) else {
                continue;
            };
            if reports.is_empty() {
                continue;
            }

            let triage_state = super::super::status::delivered_direct_report_triage_state(
                &inbox_root,
                &name,
                reports,
            )?;
            if triage_state.count > 0 {
                continue;
            }

            let review_count = tasks
                .iter()
                .filter(|task| {
                    review_backlog_owner_for_task(task, &self.config.members).as_deref()
                        == Some(name.as_str())
                })
                .count();
            if review_count > 0 {
                continue;
            }

            let report_snapshots: Vec<ReportDispatchSnapshot> = reports
                .iter()
                .map(|report| ReportDispatchSnapshot {
                    name: report.clone(),
                    is_working: !self.is_member_idle(report),
                    active_task_ids: tasks
                        .iter()
                        .filter(|task| task.claimed_by.as_deref() == Some(report.as_str()))
                        .filter(|task| task_needs_owned_intervention(task.status.as_str()))
                        .map(|task| task.id)
                        .collect(),
                })
                .collect();

            if report_snapshots.iter().any(|snapshot| snapshot.is_working) {
                continue;
            }

            let idle_active_reports: Vec<&ReportDispatchSnapshot> = report_snapshots
                .iter()
                .filter(|snapshot| !snapshot.active_task_ids.is_empty())
                .collect();
            let idle_unassigned_reports: Vec<&ReportDispatchSnapshot> = report_snapshots
                .iter()
                .filter(|snapshot| snapshot.active_task_ids.is_empty())
                .collect();

            let unassigned_open_tasks: Vec<&crate::task::Task> = tasks
                .iter()
                .filter(|task| task.claimed_by.is_none())
                .filter(|task| matches!(task.status.as_str(), "backlog" | "todo"))
                .collect();

            if idle_active_reports.is_empty() && unassigned_open_tasks.is_empty() {
                continue;
            }

            let dispatch_key = manager_dispatch_intervention_key(&name);
            let signature = manager_dispatch_intervention_signature(
                &idle_active_reports,
                &idle_unassigned_reports,
                &unassigned_open_tasks,
            );
            if self
                .owned_task_interventions
                .get(&dispatch_key)
                .is_some_and(|state| state.signature == signature)
            {
                continue;
            }
            if self.intervention_on_cooldown(&dispatch_key) {
                continue;
            }

            let text = self.build_manager_dispatch_gap_message(
                member,
                &idle_active_reports,
                &idle_unassigned_reports,
                &unassigned_open_tasks,
            );
            info!(
                member = %name,
                idle_active_reports = idle_active_reports.len(),
                idle_unassigned_reports = idle_unassigned_reports.len(),
                unassigned_open_tasks = unassigned_open_tasks.len(),
                "firing manager dispatch-gap intervention"
            );
            let delivered_live = match self.queue_daemon_message(&name, &text) {
                Ok(MessageDelivery::LivePane) => true,
                Ok(_) => false,
                Err(error) => {
                    warn!(member = %name, error = %error, "failed to deliver manager dispatch-gap intervention");
                    continue;
                }
            };
            self.record_orchestrator_action(format!(
                "recovery: dispatch-gap intervention for {} (idle reports with active work: {}, unassigned reports: {}, open tasks: {})",
                name,
                idle_active_reports.len(),
                idle_unassigned_reports.len(),
                unassigned_open_tasks.len()
            ));
            let idle_epoch = self.triage_idle_epochs.get(&name).copied().unwrap_or(0);
            self.owned_task_interventions.insert(
                dispatch_key.clone(),
                OwnedTaskInterventionState {
                    idle_epoch,
                    signature,
                    detected_at: Instant::now(),
                    escalation_sent: false,
                },
            );
            self.intervention_cooldowns
                .insert(dispatch_key, Instant::now());
            if delivered_live {
                self.mark_member_working(&name);
            }
        }

        Ok(())
    }

    pub(super) fn maybe_intervene_architect_utilization(&mut self) -> Result<()> {
        if !self
            .config
            .team_config
            .automation
            .architect_utilization_interventions
        {
            return Ok(());
        }
        if super::super::pause_marker_path(&self.config.project_root).exists() {
            return Ok(());
        }

        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let inbox_root = inbox::inboxes_root(&self.config.project_root);
        let tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks"))?;
        let direct_reports = super::super::status::direct_reports_by_member(&self.config.members);
        let engineer_names: Vec<String> = self
            .config
            .members
            .iter()
            .filter(|member| member.role_type == RoleType::Engineer)
            .map(|member| member.name.clone())
            .collect();
        let total_engineers = engineer_names.len();
        if total_engineers == 0 {
            return Ok(());
        }

        let working_engineers: Vec<String> = engineer_names
            .iter()
            .filter(|name| !self.is_member_idle(name))
            .cloned()
            .collect();
        let idle_unassigned_engineers: Vec<String> = engineer_names
            .iter()
            .filter(|name| self.is_member_idle(name))
            .filter(|name| {
                !tasks.iter().any(|task| {
                    task.claimed_by.as_deref() == Some(name.as_str())
                        && task_needs_owned_intervention(task.status.as_str())
                })
            })
            .cloned()
            .collect();
        let idle_active_engineers: Vec<(String, Vec<u32>)> = engineer_names
            .iter()
            .filter(|name| self.is_member_idle(name))
            .filter_map(|name| {
                let task_ids: Vec<u32> = tasks
                    .iter()
                    .filter(|task| task.claimed_by.as_deref() == Some(name.as_str()))
                    .filter(|task| task_needs_owned_intervention(task.status.as_str()))
                    .map(|task| task.id)
                    .collect();
                (!task_ids.is_empty()).then(|| (name.clone(), task_ids))
            })
            .collect();
        let unassigned_open_tasks: Vec<&crate::task::Task> = tasks
            .iter()
            .filter(|task| task.claimed_by.is_none())
            .filter(|task| matches!(task.status.as_str(), "backlog" | "todo"))
            .collect();

        let utilization_gap = !idle_active_engineers.is_empty()
            || (!idle_unassigned_engineers.is_empty() && !unassigned_open_tasks.is_empty());
        if !utilization_gap {
            return Ok(());
        }
        if working_engineers.len() >= total_engineers.div_ceil(2) {
            return Ok(());
        }

        let architect_members: Vec<MemberInstance> = self
            .config
            .members
            .iter()
            .filter(|member| {
                member.role_type == RoleType::Architect && direct_reports.contains_key(&member.name)
            })
            .cloned()
            .collect();

        for architect in &architect_members {
            if !self.is_member_idle(&architect.name) {
                continue;
            }
            if !self.ready_for_idle_automation(&inbox_root, &architect.name) {
                continue;
            }

            let utilization_key = architect_utilization_intervention_key(&architect.name);
            let signature = architect_utilization_intervention_signature(
                &working_engineers,
                &idle_active_engineers,
                &idle_unassigned_engineers,
                &unassigned_open_tasks,
            );
            if self
                .owned_task_interventions
                .get(&utilization_key)
                .is_some_and(|state| state.signature == signature)
            {
                continue;
            }
            if self.intervention_on_cooldown(&utilization_key) {
                continue;
            }

            let text = self.build_architect_utilization_message(
                architect,
                &working_engineers,
                &idle_active_engineers,
                &idle_unassigned_engineers,
                &unassigned_open_tasks,
            );
            info!(
                member = %architect.name,
                working_engineers = working_engineers.len(),
                idle_active_engineers = idle_active_engineers.len(),
                idle_unassigned_engineers = idle_unassigned_engineers.len(),
                unassigned_open_tasks = unassigned_open_tasks.len(),
                "firing architect utilization intervention"
            );
            let delivered_live = match self.queue_daemon_message(&architect.name, &text) {
                Ok(MessageDelivery::LivePane) => true,
                Ok(_) => false,
                Err(error) => {
                    warn!(member = %architect.name, error = %error, "failed to deliver architect utilization intervention");
                    continue;
                }
            };
            self.record_orchestrator_action(format!(
                "recovery: utilization intervention for {} (working engineers: {}, idle active: {}, idle unassigned: {}, open tasks: {})",
                architect.name,
                working_engineers.len(),
                idle_active_engineers.len(),
                idle_unassigned_engineers.len(),
                unassigned_open_tasks.len()
            ));
            let idle_epoch = self
                .triage_idle_epochs
                .get(&architect.name)
                .copied()
                .unwrap_or(0);
            self.owned_task_interventions.insert(
                utilization_key.clone(),
                OwnedTaskInterventionState {
                    idle_epoch,
                    signature,
                    detected_at: Instant::now(),
                    escalation_sent: false,
                },
            );
            self.intervention_cooldowns
                .insert(utilization_key, Instant::now());
            if delivered_live {
                self.mark_member_working(&architect.name);
            }
        }

        Ok(())
    }

    fn build_triage_intervention_message(
        &self,
        member: &MemberInstance,
        direct_reports: &[String],
        triage_count: usize,
    ) -> String {
        let report_list = direct_reports.join(", ");
        let first_report = direct_reports.first().cloned().unwrap_or_default();
        let engineer_reports: Vec<&String> = direct_reports
            .iter()
            .filter(|name| {
                self.config
                    .members
                    .iter()
                    .find(|member| member.name == **name)
                    .is_some_and(|member| member.role_type == RoleType::Engineer)
            })
            .collect();
        let first_engineer = engineer_reports.first().map(|name| name.as_str());

        let mut message = format!(
            "Triage backlog detected: you have {triage_count} delivered direct-report result packet(s) waiting for review. Reports in scope: {report_list}.\n\
Resolve it with Batty commands now:\n\
1. `batty inbox {member_name}` to list the recent result packets.\n\
2. `batty read {member_name} <ref>` for each packet you need to review in full.\n\
3. `batty send {first_report} \"accepted / blocked / next step\"` to disposition each report and unblock the sender.",
            member_name = member.name,
        );

        if let Some(engineer) = first_engineer {
            message.push_str(&format!(
                "\n4. If more implementation is needed, issue it directly with `batty assign {engineer} \"<next task>\"`."
            ));
        }

        if let Some(parent) = &member.reports_to {
            message.push_str(&format!(
                "\n5. After triage, summarize upward with `batty send {parent} \"triage summary: accepted / blocked / reassigned / next load\"`."
            ));
        }

        message.push_str(
            "\nDo the triage now and drive the backlog to zero. Batty will remind you again the next time you become idle while triage backlog remains.",
        );
        message
    }

    fn build_owned_task_intervention_message(
        &self,
        member: &MemberInstance,
        owned_tasks: &[&crate::task::Task],
        direct_reports: &[String],
    ) -> String {
        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let board_dir_str = board_dir.display();
        let task_summary = owned_tasks
            .iter()
            .map(|task| format!("#{} ({}) {}", task.id, task.status, task.title))
            .collect::<Vec<_>>()
            .join("; ");
        let task_context_cmds = owned_tasks
            .iter()
            .map(|task| {
                format!(
                    "- `kanban-md show --dir {board_dir_str} {task_id}`\n- `sed -n '1,220p' {task_path}`",
                    task_id = task.id,
                    task_path = task.source_path.display(),
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let first_task = owned_tasks[0];

        let mut message = format!(
            "Owned active task backlog detected: you are idle but still own active board task(s): {task_summary}.\n\
Retrieve task context now:\n\
1. `kanban-md list --dir {board_dir_str} --status in-progress`\n\
2. Review each owned task:\n{task_context_cmds}",
        );

        if let Some(first_report) = direct_reports.first() {
            let report_is_engineer = self
                .config
                .members
                .iter()
                .find(|candidate| candidate.name == *first_report)
                .is_some_and(|candidate| candidate.role_type == RoleType::Engineer);
            if report_is_engineer {
                message.push_str(&format!(
                    "\n3. If the task can move, assign the next concrete slice now with `batty assign {first_report} \"Task #{task_id}: <scoped subtask>\"`.",
                    task_id = first_task.id,
                ));
            } else {
                message.push_str(&format!(
                    "\n3. If the task can move, delegate the next concrete step now with `batty send {first_report} \"Task #{task_id}: <next step>\"`.",
                    task_id = first_task.id,
                ));
            }
        }

        if let Some(parent) = &member.reports_to {
            message.push_str(&format!(
                "\n4. If the lane is blocked, escalate explicitly with `batty send {parent} \"Task #{task_id} blocker: <exact blocker and next decision>\"`.",
                task_id = first_task.id,
            ));
        }

        message.push_str(&format!(
            "\n5. If the work is complete or ready for review, update board state now with `kanban-md move --dir {board_dir_str} {task_id} review` or `kanban-md move --dir {board_dir_str} {task_id} done` as appropriate.",
            task_id = first_task.id,
        ));
        message.push_str(
            "\nDo not stay idle while owning active work. Either move the task forward, split it, or escalate the blocker now. Batty will remind you again the next time you become idle while you still own unfinished tasks.",
        );
        message
    }

    fn build_review_intervention_message(
        &self,
        member: &MemberInstance,
        review_tasks: &[&crate::task::Task],
    ) -> String {
        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let board_dir_str = board_dir.display();
        let task_summary = review_tasks
            .iter()
            .map(|task| {
                let claimed_by = task.claimed_by.as_deref().unwrap_or("unknown");
                if let Some(context) = self.member_worktree_context(claimed_by) {
                    match context.branch {
                        Some(branch) => format!(
                            "#{} by {} [branch: {} | worktree: {}]",
                            task.id,
                            claimed_by,
                            branch,
                            context.path.display()
                        ),
                        None => format!(
                            "#{} by {} [worktree: {}]",
                            task.id,
                            claimed_by,
                            context.path.display()
                        ),
                    }
                } else {
                    format!("#{} by {}", task.id, claimed_by)
                }
            })
            .collect::<Vec<_>>()
            .join("; ");
        let task_context_cmds = review_tasks
            .iter()
            .map(|task| {
                let claimed_by = task.claimed_by.as_deref().unwrap_or("unknown");
                let mut lines = vec![
                    format!("- `kanban-md show --dir {board_dir_str} {}`", task.id),
                    format!("- `sed -n '1,220p' {}`", task.source_path.display()),
                ];
                if let Some(context) = self.member_worktree_context(claimed_by) {
                    lines.push(format!(
                        "- worktree: `{}`{}",
                        context.path.display(),
                        context
                            .branch
                            .as_deref()
                            .map(|branch| format!(" (branch `{branch}`)"))
                            .unwrap_or_default()
                    ));
                }
                lines.join("\n")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let first_task = review_tasks[0];
        let first_report = first_task.claimed_by.as_deref().unwrap_or("engineer");
        let first_report_is_engineer = self
            .config
            .members
            .iter()
            .find(|candidate| candidate.name == first_report)
            .is_some_and(|candidate| candidate.role_type == RoleType::Engineer);

        let mut message = format!(
            "Review backlog detected: direct-report work has completed and is waiting for your review: {task_summary}.\n\
Review and disposition it now:\n\
1. `kanban-md list --dir {board_dir_str} --status review`\n\
2. `batty inbox {member_name}` then `batty read {member_name} <ref>` to inspect the completion packet(s).\n\
3. Review each task and its lane context:\n{task_context_cmds}",
            member_name = member.name,
        );

        if first_report_is_engineer {
            message.push_str(&format!(
                "\n4. To accept engineer work, run `batty merge {first_report}` then `kanban-md move --dir {board_dir_str} {task_id} done`.",
                task_id = first_task.id,
            ));
        } else {
            message.push_str(&format!(
                "\n4. To accept the review packet, move it forward with `kanban-md move --dir {board_dir_str} {task_id} done` and send the disposition to `{first_report}`.",
                task_id = first_task.id,
            ));
        }

        message.push_str(&format!(
            "\n5. To discard it, run `kanban-md move --dir {board_dir_str} {task_id} archived` and `batty send {first_report} \"Task #{task_id} discarded: <reason>\"`.",
            task_id = first_task.id,
        ));
        let rework_command = if first_report_is_engineer {
            format!(
                "`batty assign {first_report} \"Task #{task_id}: <required changes>\"`",
                task_id = first_task.id
            )
        } else {
            format!(
                "`batty send {first_report} \"Task #{task_id}: <required changes>\"`",
                task_id = first_task.id
            )
        };
        message.push_str(&format!(
            "\n6. To request rework, run `kanban-md move --dir {board_dir_str} {task_id} in-progress` and {rework_command}.",
            task_id = first_task.id,
        ));

        if let Some(parent) = &member.reports_to {
            message.push_str(&format!(
                "\n7. After each review decision, report upward with `batty send {parent} \"Reviewed Task #{task_id}: merged / archived / rework sent to {first_report}\"`.",
                task_id = first_task.id,
            ));
        }

        message.push_str(
            "\nDo not leave completed direct-report work parked in review. Merge it, discard it, or send exact rework now. Batty will remind you again if review backlog remains unchanged.",
        );
        message
    }

    fn build_stuck_task_escalation_message(
        &self,
        member: &MemberInstance,
        owned_tasks: &[&crate::task::Task],
        stuck_age_secs: u64,
    ) -> String {
        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let board_dir_str = board_dir.display();
        let task_summary = owned_tasks
            .iter()
            .map(|task| format!("#{} ({}) {}", task.id, task.status, task.title))
            .collect::<Vec<_>>()
            .join("; ");
        let task_context_cmds = owned_tasks
            .iter()
            .map(|task| {
                format!(
                    "- `kanban-md show --dir {board_dir_str} {task_id}`\n- `sed -n '1,220p' {task_path}`",
                    task_id = task.id,
                    task_path = task.source_path.display(),
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let first_task = owned_tasks[0];
        let redirect_command = if member.role_type == RoleType::Engineer {
            format!(
                "`batty assign {member_name} \"Task #{task_id}: <next concrete step or unblock plan>\"`",
                member_name = member.name,
                task_id = first_task.id,
            )
        } else {
            format!(
                "`batty send {member_name} \"Task #{task_id}: <next concrete step or unblock plan>\"`",
                member_name = member.name,
                task_id = first_task.id,
            )
        };

        let mut message = format!(
            "Stuck task escalation: {member_name} has remained idle while still owning active board task(s) for at least {stuck_duration}: {task_summary}.\n\
Intervene now:\n\
1. `batty status`\n\
2. `kanban-md list --dir {board_dir_str} --status in-progress`\n\
3. Review the stuck task context:\n{task_context_cmds}\n\
4. If the lane is executable, push the next action now with {redirect_command}.",
            member_name = member.name,
            stuck_duration = format_stuck_duration(stuck_age_secs),
        );

        message.push_str(&format!(
            "\n5. If the lane is blocked, record it now with `kanban-md edit --dir {board_dir_str} {task_id} --block \"<exact blocker>\" --claim {member_name}` and send the decision back to `{member_name}`.",
            task_id = first_task.id,
            member_name = member.name,
        ));

        if let Some(parent) = &member.reports_to {
            message.push_str(&format!(
                "\n6. If you need a higher-level decision, escalate again with `batty send {parent} \"Task #{task_id} stuck under {member_name}: <decision needed>\"`.",
                task_id = first_task.id,
                member_name = member.name,
            ));
        }

        message.push_str(
            "\nDo not leave the task parked. Re-dispatch it, block it with a specific reason, or escalate the exact decision needed now.",
        );
        message
    }

    fn build_manager_dispatch_gap_message(
        &self,
        member: &MemberInstance,
        idle_active_reports: &[&ReportDispatchSnapshot],
        idle_unassigned_reports: &[&ReportDispatchSnapshot],
        unassigned_open_tasks: &[&crate::task::Task],
    ) -> String {
        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let board_dir_str = board_dir.display();
        let active_report_summary = if idle_active_reports.is_empty() {
            "none".to_string()
        } else {
            idle_active_reports
                .iter()
                .map(|snapshot| {
                    let ids = snapshot
                        .active_task_ids
                        .iter()
                        .map(|id| format!("#{id}"))
                        .collect::<Vec<_>>()
                        .join(",");
                    format!("{} on {}", snapshot.name, ids)
                })
                .collect::<Vec<_>>()
                .join("; ")
        };
        let unassigned_report_summary = if idle_unassigned_reports.is_empty() {
            "none".to_string()
        } else {
            idle_unassigned_reports
                .iter()
                .map(|snapshot| snapshot.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let open_task_summary = if unassigned_open_tasks.is_empty() {
            "none".to_string()
        } else {
            unassigned_open_tasks
                .iter()
                .take(3)
                .map(|task| format!("#{} ({}) {}", task.id, task.status, task.title))
                .collect::<Vec<_>>()
                .join("; ")
        };

        let mut message = format!(
            "Dispatch recovery needed: you are idle, your reports are idle, and the lane has no triage/review backlog. Idle reports still holding active work: {active_report_summary}. Idle reports with no active task: {unassigned_report_summary}. Unassigned open board work: {open_task_summary}.\n\
Recover the lane now:\n\
1. `batty status`\n\
2. `kanban-md list --dir {board_dir_str} --status in-progress`\n\
3. `kanban-md list --dir {board_dir_str} --status todo`\n\
4. `kanban-md list --dir {board_dir_str} --status backlog`"
        );

        if let Some(first_active) = idle_active_reports.first() {
            let first_task_id = first_active.active_task_ids[0];
            message.push_str(&format!(
                "\n5. For an idle active lane, intervene directly with `batty send {report} \"Task #{task_id} is idle under your ownership. Either move it forward now, report the exact blocker, or request board normalization.\"`.",
                report = first_active.name,
                task_id = first_task_id,
            ));
        }

        if let (Some(first_unassigned_report), Some(first_open_task)) = (
            idle_unassigned_reports.first(),
            unassigned_open_tasks.first(),
        ) {
            message.push_str(&format!(
                "\n6. If executable work exists, start it now with `batty assign {report} \"Task #{task_id}: {title}\"`.",
                report = first_unassigned_report.name,
                task_id = first_open_task.id,
                title = first_open_task.title,
            ));
        }

        if let Some(parent) = &member.reports_to {
            message.push_str(&format!(
                "\n7. If the lane has no executable next step, escalate explicitly with `batty send {parent} \"lane blocked: all reports idle; need new dispatch or decision\"`."
            ));
        }

        message.push_str(
            "\nDo not let the entire lane sit idle. Either wake an active task, assign new executable work, or escalate the exact blockage now.",
        );
        message
    }

    fn build_architect_utilization_message(
        &self,
        member: &MemberInstance,
        working_engineers: &[String],
        idle_active_engineers: &[(String, Vec<u32>)],
        idle_unassigned_engineers: &[String],
        unassigned_open_tasks: &[&crate::task::Task],
    ) -> String {
        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let board_dir_str = board_dir.display();
        let working_summary = if working_engineers.is_empty() {
            "none".to_string()
        } else {
            working_engineers.join(", ")
        };
        let idle_active_summary = if idle_active_engineers.is_empty() {
            "none".to_string()
        } else {
            idle_active_engineers
                .iter()
                .map(|(engineer, task_ids)| {
                    let ids = task_ids
                        .iter()
                        .map(|id| format!("#{id}"))
                        .collect::<Vec<_>>()
                        .join(",");
                    format!("{engineer} on {ids}")
                })
                .collect::<Vec<_>>()
                .join("; ")
        };
        let idle_unassigned_summary = if idle_unassigned_engineers.is_empty() {
            "none".to_string()
        } else {
            idle_unassigned_engineers.join(", ")
        };
        let open_task_summary = if unassigned_open_tasks.is_empty() {
            "none".to_string()
        } else {
            unassigned_open_tasks
                .iter()
                .take(4)
                .map(|task| format!("#{} ({}) {}", task.id, task.status, task.title))
                .collect::<Vec<_>>()
                .join("; ")
        };

        let mut message = format!(
            "Utilization recovery needed: you are idle while team throughput is low. Working engineers: {working_summary}. Idle engineers still holding active work: {idle_active_summary}. Idle engineers with no active task: {idle_unassigned_summary}. Unassigned open board work: {open_task_summary}.\n\
Recover throughput now:\n\
1. `batty status`\n\
2. `kanban-md list --dir {board_dir_str} --status in-progress`\n\
3. `kanban-md list --dir {board_dir_str} --status todo`\n\
4. `kanban-md list --dir {board_dir_str} --status backlog`"
        );

        if let Some((engineer, task_ids)) = idle_active_engineers.first() {
            let task_id = task_ids[0];
            if let Some(lead) = self.manager_for_member_name(engineer) {
                message.push_str(&format!(
                    "\n5. For an idle active lane, force lead action now with `batty send {lead} \"Engineer {engineer} is idle on Task #{task_id}. Normalize the board state or unblock/reassign this lane now.\"`."
                ));
            }
        }

        if let (Some(engineer), Some(task)) = (
            idle_unassigned_engineers.first(),
            unassigned_open_tasks.first(),
        ) {
            if let Some(lead) = self.manager_for_member_name(engineer) {
                message.push_str(&format!(
                    "\n6. For unused capacity, dispatch through the lead now with `batty send {lead} \"Start Task #{task_id} on {engineer} now: {title}\"`.",
                    task_id = task.id,
                    title = task.title,
                ));
            }
        }

        message.push_str(
            "\n7. If the board has no executable work left, create the next concrete task or ask the human only for a real policy decision. Do not leave the team underloaded without an explicit next dispatch.",
        );
        if let Some(parent) = &member.reports_to {
            message.push_str(&format!(
                "\n8. Report the recovery decision upward with `batty send {parent} \"utilization recovery: <what was dispatched or why the board is blocked>\"`."
            ));
        }
        message
    }
}

fn task_needs_owned_intervention(status: &str) -> bool {
    !matches!(status, "review" | "done" | "archived")
}

fn manager_dispatch_intervention_key(member_name: &str) -> String {
    format!("dispatch::{member_name}")
}

fn manager_dispatch_intervention_signature(
    idle_active_reports: &[&ReportDispatchSnapshot],
    idle_unassigned_reports: &[&ReportDispatchSnapshot],
    unassigned_open_tasks: &[&crate::task::Task],
) -> String {
    let mut parts = Vec::new();
    for snapshot in idle_active_reports {
        let task_ids = snapshot
            .active_task_ids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        parts.push(format!("active:{}:{task_ids}", snapshot.name));
    }
    for snapshot in idle_unassigned_reports {
        parts.push(format!("idle:{}", snapshot.name));
    }
    for task in unassigned_open_tasks {
        parts.push(format!("open:{}:{}", task.id, task.status));
    }
    parts.sort();
    parts.join("|")
}

fn owned_task_intervention_signature(tasks: &[&crate::task::Task]) -> String {
    let mut parts = tasks
        .iter()
        .map(|task| format!("{}:{}", task.id, task.status))
        .collect::<Vec<_>>();
    parts.sort();
    parts.join("|")
}

fn review_backlog_owner_for_task(
    task: &crate::task::Task,
    members: &[MemberInstance],
) -> Option<String> {
    if task.status != "review" {
        return None;
    }
    let claimed_by = task.claimed_by.as_deref()?;
    Some(
        members
            .iter()
            .find(|member| member.name == claimed_by)
            .and_then(|member| member.reports_to.clone())
            .unwrap_or_else(|| claimed_by.to_string()),
    )
}

fn review_intervention_key(member_name: &str) -> String {
    format!("review::{member_name}")
}

fn architect_utilization_intervention_key(member_name: &str) -> String {
    format!("utilization::{member_name}")
}

fn architect_utilization_intervention_signature(
    working_engineers: &[String],
    idle_active_engineers: &[(String, Vec<u32>)],
    idle_unassigned_engineers: &[String],
    unassigned_open_tasks: &[&crate::task::Task],
) -> String {
    let mut parts = Vec::new();
    for engineer in working_engineers {
        parts.push(format!("working:{engineer}"));
    }
    for (engineer, task_ids) in idle_active_engineers {
        let ids = task_ids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        parts.push(format!("idle-active:{engineer}:{ids}"));
    }
    for engineer in idle_unassigned_engineers {
        parts.push(format!("idle-free:{engineer}"));
    }
    for task in unassigned_open_tasks {
        parts.push(format!("open:{}:{}", task.id, task.status));
    }
    parts.sort();
    parts.join("|")
}

fn review_task_intervention_signature(tasks: &[&crate::task::Task]) -> String {
    let mut parts = tasks
        .iter()
        .map(|task| {
            format!(
                "{}:{}:{}",
                task.id,
                task.status,
                task.claimed_by.as_deref().unwrap_or("unknown")
            )
        })
        .collect::<Vec<_>>();
    parts.sort();
    parts.join("|")
}
