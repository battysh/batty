//! Stall detection, restart, and escalation.

use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use tracing::warn;

use super::super::*;
use super::{CONTEXT_RESTART_COOLDOWN, format_checkpoint_section};
use crate::team::supervisory_notice::{
    SupervisoryMemberActivity, SupervisoryPressure, supervisory_pending_pressure,
    supervisory_pressure_snapshots,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in super::super) enum SupervisoryLane {
    Architect,
    Manager,
}

impl SupervisoryLane {
    pub(in super::super) fn from_role(role_type: RoleType) -> Option<Self> {
        match role_type {
            RoleType::Architect => Some(Self::Architect),
            RoleType::Manager => Some(Self::Manager),
            _ => None,
        }
    }

    pub(in super::super) fn label(self) -> &'static str {
        match self {
            Self::Architect => "architect",
            Self::Manager => "manager",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in super::super) enum SupervisoryProgress {
    // Retained for future use: pressure types that represent supervisory work
    // which IS actionable but doesn't block stall detection (e.g. a new pressure
    // type that requires immediate attention but not via the dispatch-gap path).
    #[allow(dead_code)]
    Actionable(SupervisoryPressure),
    Expected(&'static str),
    Incidental(&'static str),
    None,
}

impl SupervisoryProgress {
    pub(in super::super) fn stall_reason(&self) -> &'static str {
        match self {
            Self::Actionable(_) => "supervisory_actionable_progress",
            Self::Expected("main_git_activity") => "supervisory_main_git_activity",
            Self::Expected("board_state_transition") => "supervisory_board_state_transition",
            Self::Expected("inbox_batching") => "supervisory_inbox_batching",
            Self::Expected("supervisory_digest") => "supervisory_digest_waiting",
            Self::Expected("fresh_supervisory_input") => "supervisory_fresh_input",
            Self::Expected(_) => "supervisory_review_waiting",
            Self::Incidental("shim_activity") => "supervisory_shim_activity_only",
            Self::Incidental(_) => "supervisory_status_only_output",
            Self::None => "supervisory_no_actionable_progress",
        }
    }

    fn stall_reason_suffix(self) -> &'static str {
        match self {
            Self::Actionable(pressure) => pressure.stall_reason_suffix(),
            Self::Expected("main_git_activity") => "main_git_activity",
            Self::Expected("board_state_transition") => "board_state_transition",
            Self::Expected("inbox_batching") => "inbox_batching",
            Self::Expected("supervisory_digest") => "digest_waiting",
            Self::Expected("fresh_supervisory_input") => "fresh_input",
            Self::Expected(_) => "review_waiting",
            Self::Incidental("shim_activity") => "shim_activity_only",
            Self::Incidental(_) => "status_only_output",
            Self::None => "no_actionable_progress",
        }
    }

    pub(in super::super) fn short_label(&self) -> &'static str {
        match self {
            Self::Actionable(pressure) => pressure.short_label(),
            Self::Expected("main_git_activity") => "main merge activity",
            Self::Expected("board_state_transition") => "board state transition",
            Self::Expected("inbox_batching") => "inbox batching",
            Self::Expected("supervisory_digest") => "digest review",
            Self::Expected("fresh_supervisory_input") => "fresh supervisory input",
            Self::Expected("review_waiting") => "review waiting",
            Self::Expected(_) => "expected supervisory work",
            Self::Incidental("shim_activity") => "shim activity only",
            Self::Incidental(_) => "status-only output",
            Self::None => "no actionable progress",
        }
    }

    pub(in super::super) fn stall_reason_for_lane(self, lane: SupervisoryLane) -> String {
        format!(
            "supervisory_stalled_{}_{}",
            lane.label(),
            self.stall_reason_suffix()
        )
    }

    pub(in super::super) fn is_stall_signal(&self) -> bool {
        matches!(self, Self::Incidental(_) | Self::None)
    }
}

pub(in super::super) enum SupervisoryStallRecordInput<'a> {
    Signal(SupervisoryProgress),
    LegacyReason(&'a str),
}

impl<'a> From<SupervisoryProgress> for SupervisoryStallRecordInput<'a> {
    fn from(value: SupervisoryProgress) -> Self {
        Self::Signal(value)
    }
}

impl<'a> From<&'a str> for SupervisoryStallRecordInput<'a> {
    fn from(value: &'a str) -> Self {
        Self::LegacyReason(value)
    }
}

impl TeamDaemon {
    pub(in super::super) fn supervisory_lane(&self, member_name: &str) -> Option<SupervisoryLane> {
        self.config
            .members
            .iter()
            .find(|member| member.name == member_name)
            .and_then(|member| SupervisoryLane::from_role(member.role_type))
    }

    fn supervisory_expected_progress(&self, member_name: &str) -> Option<SupervisoryProgress> {
        let inbox_root = crate::team::inbox::inboxes_root(&self.config.project_root);
        let activity = self
            .config
            .members
            .iter()
            .map(|member| {
                (
                    member.name.clone(),
                    SupervisoryMemberActivity {
                        idle: self
                            .watchers
                            .get(&member.name)
                            .map(|watcher| {
                                matches!(watcher.state, WatcherState::Ready | WatcherState::Idle)
                            })
                            .unwrap_or(matches!(
                                self.states.get(&member.name),
                                Some(MemberState::Idle) | None
                            )),
                    },
                )
            })
            .collect::<std::collections::HashMap<_, _>>();
        let pressures = supervisory_pressure_snapshots(
            &self.config.project_root,
            &self.config.members,
            &activity,
        );
        if let Some((pressure, _)) = pressures
            .get(member_name)
            .and_then(|snapshot| snapshot.top_actionable())
        {
            // ReviewBacklog and TriageBacklog are expected supervisory work — the
            // manager is actively waiting on reviews or processing direct-report
            // packets. They should suppress stall detection without triggering the
            // fallback-dispatch path.
            //
            // DispatchGap and IdleActiveRecovery are NOT returned here; they fall
            // through to the shim/watcher activity check so that shim-chatter-only
            // managers are still detected as stalled and the dispatch-gap
            // intervention can fire.
            match pressure {
                SupervisoryPressure::ReviewBacklog => {
                    return Some(SupervisoryProgress::Expected("review_waiting"));
                }
                SupervisoryPressure::TriageBacklog => {
                    return Some(SupervisoryProgress::Expected("inbox_batching"));
                }
                _ => {}
            }
        }

        if supervisory_pending_pressure(&inbox_root, member_name)
            .ok()
            .is_some_and(|snapshot| snapshot.actionable_count() > 0)
        {
            return Some(SupervisoryProgress::Expected("inbox_batching"));
        }
        if crate::team::inbox::pending_message_count(&inbox_root, member_name)
            .ok()
            .is_some_and(|count| count > 0)
        {
            return Some(SupervisoryProgress::Expected("inbox_batching"));
        }

        None
    }

    fn recent_supervisory_event_signal(
        &self,
        member_name: &str,
        threshold_secs: u64,
    ) -> Option<SupervisoryProgress> {
        let now = super::super::super::now_unix();
        let events_path = super::super::super::team_events_path(&self.config.project_root);
        let events = super::super::super::events::read_events(&events_path).ok()?;

        events.into_iter().rev().find_map(|event| {
            if now.saturating_sub(event.ts) > threshold_secs {
                return None;
            }

            match event.event.as_str() {
                "message_routed" if event.from.as_deref() == Some(member_name) => {
                    Some(SupervisoryProgress::Expected("fresh_supervisory_input"))
                }
                "task_escalated"
                | "task_unblocked"
                | "task_completed"
                | "task_auto_merged"
                | "task_manual_merged"
                | "state_reconciliation"
                    if event.role.as_deref() == Some(member_name) =>
                {
                    Some(SupervisoryProgress::Expected("fresh_supervisory_input"))
                }
                "supervisory_digest_emitted" if event.role.as_deref() == Some(member_name) => {
                    Some(SupervisoryProgress::Expected("supervisory_digest"))
                }
                "notification_delivery_sample"
                    if event.to.as_deref() == Some(member_name)
                        && event.from.as_deref() != Some(member_name) =>
                {
                    match event.action_type.as_deref() {
                        Some("digest") => Some(SupervisoryProgress::Expected("supervisory_digest")),
                        Some("live") => {
                            Some(SupervisoryProgress::Expected("fresh_supervisory_input"))
                        }
                        _ => None,
                    }
                }
                _ => None,
            }
        })
    }

    fn recent_supervisory_side_effect_signal(
        &self,
        member_name: &str,
        threshold_secs: u64,
    ) -> Option<SupervisoryProgress> {
        match self.supervisory_lane(member_name)? {
            SupervisoryLane::Architect => self.recent_architect_git_signal(threshold_secs),
            SupervisoryLane::Manager => self.recent_manager_board_signal(threshold_secs),
        }
    }

    fn recent_architect_git_signal(&self, threshold_secs: u64) -> Option<SupervisoryProgress> {
        let git_dir = resolve_git_dir(&self.config.project_root)?;
        let threshold = Duration::from_secs(threshold_secs);

        let main_activity = [
            git_dir.join("logs").join("refs").join("heads").join("main"),
            git_dir.join("CHERRY_PICK_HEAD"),
            git_dir.join("MERGE_HEAD"),
            git_dir.join("REBASE_HEAD"),
            git_dir.join("ORIG_HEAD"),
        ];
        if main_activity
            .iter()
            .any(|path| path_modified_within(path, threshold))
        {
            return Some(SupervisoryProgress::Expected("main_git_activity"));
        }

        if tree_modified_within(&git_dir.join("refs").join("tags"), threshold)
            || path_modified_within(&git_dir.join("packed-refs"), threshold)
        {
            return Some(SupervisoryProgress::Expected("main_git_activity"));
        }

        None
    }

    fn recent_manager_board_signal(&self, threshold_secs: u64) -> Option<SupervisoryProgress> {
        let tasks_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        let now = Utc::now();
        let has_recent_completion = crate::task::load_tasks_from_dir(&tasks_dir)
            .ok()?
            .into_iter()
            .any(|task| {
                task.status == "done"
                    && task
                        .completed
                        .as_deref()
                        .and_then(|completed| DateTime::parse_from_rfc3339(completed).ok())
                        .is_some_and(|completed| {
                            now.signed_duration_since(completed.with_timezone(&Utc))
                                <= chrono::Duration::seconds(threshold_secs as i64)
                        })
            });
        if has_recent_completion {
            return Some(SupervisoryProgress::Expected("board_state_transition"));
        }
        None
    }

    pub(in super::super) fn format_supervisory_stall_summary(
        &self,
        member_name: &str,
        stall_secs: u64,
        signal: &SupervisoryProgress,
    ) -> String {
        let lane = self
            .supervisory_lane(member_name)
            .map(|lane| lane.label())
            .unwrap_or("supervisory");
        format!(
            "{member_name} ({lane}) stalled after {}: {}",
            crate::team::status::format_health_duration(stall_secs),
            signal.short_label(),
        )
    }

    pub(in super::super) fn supervisory_progress_signal(
        &self,
        member_name: &str,
        threshold_secs: u64,
    ) -> SupervisoryProgress {
        if threshold_secs == 0 {
            return SupervisoryProgress::None;
        }
        if self.supervisory_lane(member_name).is_none() {
            return SupervisoryProgress::None;
        }

        if let Some(signal) = self.supervisory_expected_progress(member_name) {
            return signal;
        }

        if let Some(signal) = self.recent_supervisory_event_signal(member_name, threshold_secs) {
            return signal;
        }

        if let Some(signal) =
            self.recent_supervisory_side_effect_signal(member_name, threshold_secs)
        {
            return signal;
        }

        if self
            .shim_handles
            .get(member_name)
            .and_then(|handle| handle.secs_since_last_activity())
            .is_some_and(|secs| secs < threshold_secs)
        {
            return SupervisoryProgress::Incidental("shim_activity");
        }

        if self
            .watchers
            .get(member_name)
            .is_some_and(|watcher| watcher.secs_since_last_output_change() < threshold_secs)
        {
            return SupervisoryProgress::Incidental("output_activity");
        }

        SupervisoryProgress::None
    }

    pub(in super::super) fn is_supervisory_lane_stalled(
        &self,
        member_name: &str,
        threshold_secs: u64,
    ) -> bool {
        if threshold_secs == 0 {
            return false;
        }
        if self.supervisory_lane(member_name).is_none() {
            return false;
        }

        let working_secs = self
            .shim_handles
            .get(member_name)
            .filter(|handle| handle.state == crate::shim::protocol::ShimState::Working)
            .map(|handle| handle.secs_since_state_change());
        let Some(working_secs) = working_secs else {
            return false;
        };
        if working_secs < threshold_secs {
            return false;
        }

        self.supervisory_progress_signal(member_name, threshold_secs)
            .is_stall_signal()
    }

    pub(in super::super) fn record_supervisory_stall_reason<'a, T>(
        &mut self,
        member_name: &str,
        stall_secs: u64,
        input: T,
    ) where
        T: Into<SupervisoryStallRecordInput<'a>>,
    {
        let cooldown_key = format!("supervisory-stall::{member_name}");
        let cooldown = std::time::Duration::from_secs(
            self.config
                .team_config
                .automation
                .intervention_cooldown_secs,
        );
        if self
            .intervention_cooldowns
            .get(&cooldown_key)
            .is_some_and(|last| last.elapsed() < cooldown)
        {
            return;
        }
        let observed_stall_secs = self
            .shim_handles
            .get(member_name)
            .map(|handle| handle.secs_since_state_change())
            .unwrap_or(stall_secs);
        let (signal, fallback_reason) = match input.into() {
            SupervisoryStallRecordInput::Signal(signal) => (signal, signal.stall_reason()),
            SupervisoryStallRecordInput::LegacyReason(reason) => (
                self.supervisory_progress_signal(member_name, stall_secs),
                reason,
            ),
        };
        let reason = self
            .supervisory_lane(member_name)
            .map(|lane| signal.stall_reason_for_lane(lane))
            .unwrap_or_else(|| fallback_reason.to_string());
        let mut event = TeamEvent::stall_detected_with_reason(
            member_name,
            None,
            observed_stall_secs,
            Some(&reason),
        );
        event.task = Some(format!("supervisory::{member_name}"));
        event.details =
            Some(self.format_supervisory_stall_summary(member_name, observed_stall_secs, &signal));
        self.emit_event(event);
        self.record_orchestrator_action(format!(
            "stall: detected {}",
            self.format_supervisory_stall_summary(member_name, observed_stall_secs, &signal)
        ));
        self.intervention_cooldowns
            .insert(cooldown_key, Instant::now());
    }

    /// Handle a stalled agent — no output change for longer than the configured threshold.
    #[allow(dead_code)]
    pub(in super::super) fn handle_stalled_agent(
        &mut self,
        member_name: &str,
        stall_secs: u64,
    ) -> Result<()> {
        let Some(task) = self.active_task(member_name)? else {
            return Ok(());
        };
        let member = match self.config.members.iter().find(|m| m.name == member_name) {
            Some(m) => m.clone(),
            None => return Ok(()),
        };
        let Some(pane_id) = self.config.pane_map.get(member_name).cloned() else {
            return Ok(());
        };

        let stall_cooldown_key = format!("stall-restart::{member_name}");
        let on_cooldown = self
            .intervention_cooldowns
            .get(&stall_cooldown_key)
            .is_some_and(|last| last.elapsed() < CONTEXT_RESTART_COOLDOWN);
        if on_cooldown {
            return Ok(());
        }

        let task_id_str = task.id.to_string();
        let prior_restarts = self.stall_restart_count(task.id)?;
        let max_restarts = self.config.team_config.workflow_policy.max_stall_restarts;

        warn!(
            member = %member_name,
            task_id = task.id,
            stall_secs,
            prior_restarts,
            "agent stalled — no output change"
        );

        self.emit_event(TeamEvent::stall_detected(
            member_name,
            Some(task.id),
            stall_secs,
        ));
        self.record_orchestrator_action(format!(
            "stall: detected agent stall for {} on task #{} ({}s no output, {} prior restarts)",
            member_name, task.id, stall_secs, prior_restarts,
        ));

        if prior_restarts >= max_restarts {
            // Escalate to manager instead of restarting again.
            let escalation_key = format!("stall-escalation::{member_name}");
            let escalation_on_cooldown = self
                .intervention_cooldowns
                .get(&escalation_key)
                .is_some_and(|last| last.elapsed() < CONTEXT_RESTART_COOLDOWN);
            if escalation_on_cooldown {
                return Ok(());
            }
            self.escalate_stalled_agent(&member, &task, prior_restarts + 1)?;
            self.intervention_cooldowns
                .insert(escalation_key, Instant::now());
            return Ok(());
        }

        let work_dir = self.member_work_dir(&member);

        // Write progress checkpoint before restarting.
        self.preserve_restart_context(member_name, &task, Some(&pane_id), &work_dir, "stalled");

        // Restart the stalled agent with task context.
        tmux::respawn_pane(&pane_id, "bash")?;
        std::thread::sleep(std::time::Duration::from_millis(200));

        let assignment = self.restart_assignment_with_handoff(member_name, &task, &work_dir);
        let launch = self.launch_task_assignment(member_name, &assignment, Some(task.id), false)?;
        let mut restart_notice = format!(
            "Restarted after stall ({}s no output). Continue task #{} from the current worktree state.",
            stall_secs, task.id
        );
        if let Some(branch) = launch.branch.as_deref() {
            restart_notice.push_str(&format!("\nBranch: {branch}"));
        }
        restart_notice.push_str(&format!("\nWorktree: {}", launch.work_dir.display()));
        // Include checkpoint content in restart notice.
        if let Some(cp_content) =
            super::super::super::checkpoint::read_checkpoint(&self.config.project_root, member_name)
        {
            restart_notice.push_str(&format_checkpoint_section(&cp_content));
        }
        if let Err(error) = self.queue_message("daemon", member_name, &restart_notice) {
            warn!(member = %member_name, error = %error, "failed to inject stall restart notice");
        }
        self.record_orchestrator_action(format!(
            "stall: relaunched {} on task #{} after {}s stall",
            member_name, task.id, stall_secs,
        ));
        self.intervention_cooldowns
            .insert(stall_cooldown_key, Instant::now());
        self.record_agent_restarted(member_name, task_id_str, "stalled", prior_restarts + 1);
        Ok(())
    }

    /// Escalate a stalled agent to its manager after max restarts exceeded.
    #[allow(dead_code)]
    fn escalate_stalled_agent(
        &mut self,
        member: &MemberInstance,
        task: &crate::task::Task,
        restart_count: u32,
    ) -> Result<()> {
        let Some(manager) = member.reports_to.as_deref() else {
            warn!(
                member = %member.name,
                task_id = task.id,
                restart_count,
                "stall exceeded restart limit with no escalation target"
            );
            return Ok(());
        };

        let body = format!(
            "Task #{task_id} for {member_name} stalled {restart_count} times (no output). \
             Batty restarted it {max} time(s) already and will not restart again automatically.\n\
             Task: {title}\n\
             Next step: decide whether to split the task, redirect the engineer, or intervene directly.",
            task_id = task.id,
            member_name = member.name,
            title = task.title,
            max = restart_count.saturating_sub(1),
        );
        self.queue_message("daemon", manager, &body)?;
        self.record_orchestrator_action(format!(
            "stall: escalated stall for {} on task #{} after {} stalls",
            member.name, task.id, restart_count,
        ));
        self.record_task_escalated(&member.name, task.id.to_string(), Some("stalled"));
        Ok(())
    }

    /// Count prior stall restarts for a given task from the event log.
    #[allow(dead_code)]
    pub(in super::super) fn stall_restart_count(&self, task_id: u32) -> Result<u32> {
        let events_path = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        let task_id = task_id.to_string();
        let count = super::super::super::events::read_events(&events_path)?
            .into_iter()
            .filter(|event| event.event == "agent_restarted")
            .filter(|event| event.task.as_deref() == Some(task_id.as_str()))
            .filter(|event| event.reason.as_deref() == Some("stalled"))
            .count() as u32;
        Ok(count)
    }
}

fn resolve_git_dir(project_root: &Path) -> Option<PathBuf> {
    let git_path = project_root.join(".git");
    if git_path.is_dir() {
        return Some(git_path);
    }
    let contents = std::fs::read_to_string(&git_path).ok()?;
    let gitdir = contents.trim().strip_prefix("gitdir:")?.trim();
    let git_dir = PathBuf::from(gitdir);
    Some(if git_dir.is_absolute() {
        git_dir
    } else {
        project_root.join(git_dir)
    })
}

fn path_modified_within(path: &Path, threshold: Duration) -> bool {
    std::fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .is_some_and(|modified| modified_within(modified, threshold))
}

fn tree_modified_within(path: &Path, threshold: Duration) -> bool {
    if path_modified_within(path, threshold) {
        return true;
    }

    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };

    entries.filter_map(|entry| entry.ok()).any(|entry| {
        let child = entry.path();
        if child.is_dir() {
            tree_modified_within(&child, threshold)
        } else {
            path_modified_within(&child, threshold)
        }
    })
}

fn modified_within(modified: SystemTime, threshold: Duration) -> bool {
    SystemTime::now()
        .duration_since(modified)
        .map(|elapsed| elapsed <= threshold)
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::super::super::*;
    use super::super::test_helpers::test_team_config;
    use super::SupervisoryProgress;
    use crate::team::config::{
        AutomationConfig, BoardConfig, OrchestratorPosition, RoleType, StandupConfig, TeamConfig,
        WorkflowMode, WorkflowPolicy,
    };
    use crate::team::events::TeamEvent;
    use crate::team::hierarchy::MemberInstance;
    use crate::team::inbox::{self, InboxMessage};
    use crate::team::standup::MemberState;
    use crate::team::test_helpers::{make_test_daemon, write_event_log};
    use crate::team::test_support::{
        TestDaemonBuilder, architect_member, engineer_member, git_ok, init_git_repo,
        manager_member, setup_fake_claude, write_owned_task_file,
        write_owned_task_file_with_context,
    };
    use chrono::Utc;
    use serial_test::serial;
    use std::collections::HashMap;
    use std::process::Command;
    use std::time::{Duration, Instant};

    fn insert_working_shim_handle(
        daemon: &mut TeamDaemon,
        member: &str,
        working_secs: u64,
        last_activity_secs: u64,
    ) {
        let (parent, _child) = crate::shim::protocol::socketpair().unwrap();
        let mut channel = crate::shim::protocol::Channel::new(parent);
        channel
            .set_read_timeout(Some(Duration::from_millis(5)))
            .unwrap();
        let mut handle = crate::team::daemon::agent_handle::AgentHandle::new(
            member.to_string(),
            channel,
            999,
            "claude".to_string(),
            "claude".to_string(),
            std::path::PathBuf::from("/tmp/test"),
        );
        handle.apply_state_change(crate::shim::protocol::ShimState::Working);
        handle.state_changed_at = Instant::now() - Duration::from_secs(working_secs);
        handle.last_activity_at = Some(Instant::now() - Duration::from_secs(last_activity_secs));
        daemon.shim_handles.insert(member.to_string(), handle);
    }

    #[test]
    fn architect_not_flagged_stalled_while_cherry_picking_recent_commits() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "stall-architect-merge");
        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(vec![
                architect_member("architect"),
                manager_member("lead", Some("architect")),
                engineer_member("eng-1", Some("lead"), false),
            ])
            .states(HashMap::from([(
                "architect".to_string(),
                MemberState::Working,
            )]))
            .build();
        let threshold = daemon
            .config
            .team_config
            .workflow_policy
            .stall_threshold_secs;
        insert_working_shim_handle(&mut daemon, "architect", threshold + 10, threshold + 10);

        git_ok(&repo, &["checkout", "-b", "feature"]);
        for (index, contents) in ["first\n", "second\n", "third\n"].into_iter().enumerate() {
            let path = repo.join(format!("feature-{index}.txt"));
            std::fs::write(path, contents).unwrap();
            git_ok(&repo, &["add", "."]);
            git_ok(&repo, &["commit", "-m", &format!("feature commit {index}")]);
        }
        git_ok(&repo, &["checkout", "main"]);
        for commit in ["feature~2", "feature~1", "feature"] {
            git_ok(&repo, &["cherry-pick", commit]);
        }

        let signal = daemon.supervisory_progress_signal("architect", threshold);
        assert_eq!(signal.short_label(), "main merge activity");
        assert!(!daemon.is_supervisory_lane_stalled("architect", threshold));
    }

    #[test]
    fn manager_not_flagged_stalled_while_advancing_board_state() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "stall-manager-board");
        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(vec![
                architect_member("architect"),
                manager_member("lead", Some("architect")),
                engineer_member("eng-1", Some("lead"), false),
            ])
            .states(HashMap::from([("lead".to_string(), MemberState::Working)]))
            .build();
        let threshold = daemon
            .config
            .team_config
            .workflow_policy
            .stall_threshold_secs;
        insert_working_shim_handle(&mut daemon, "lead", threshold + 10, threshold + 10);

        let completed = Utc::now().to_rfc3339();
        let task_path = repo
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks")
            .join("191-review-task.md");
        std::fs::create_dir_all(task_path.parent().unwrap()).unwrap();
        std::fs::write(
            &task_path,
            format!(
                "---\nid: 191\ntitle: review-task\nstatus: done\npriority: critical\nclaimed_by: eng-1\ncompleted: {completed}\nclass: standard\n---\n\nTask description.\n"
            ),
        )
        .unwrap();

        let signal = daemon.supervisory_progress_signal("lead", threshold);
        assert_eq!(signal.short_label(), "board state transition");
        assert!(!daemon.is_supervisory_lane_stalled("lead", threshold));
    }

    #[test]
    fn stall_restart_count_returns_zero_with_no_events() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let daemon = make_test_daemon(tmp.path(), vec![]);
        let count = daemon.stall_restart_count(42).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn stall_restart_count_counts_only_stalled_reason() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        write_event_log(
            tmp.path(),
            &[
                TeamEvent::agent_restarted("eng-1-1", "42", "context_exhausted", 1),
                TeamEvent::agent_restarted("eng-1-1", "42", "stalled", 1),
                TeamEvent::agent_restarted("eng-1-1", "42", "stalled", 2),
                TeamEvent::agent_restarted("eng-1-1", "99", "stalled", 1),
            ],
        );

        let daemon = make_test_daemon(tmp.path(), vec![]);
        assert_eq!(daemon.stall_restart_count(42).unwrap(), 2);
        assert_eq!(daemon.stall_restart_count(99).unwrap(), 1);
        assert_eq!(daemon.stall_restart_count(100).unwrap(), 0);
    }

    #[test]
    fn stall_detection_config_defaults() {
        let policy = WorkflowPolicy::default();
        assert_eq!(policy.stall_threshold_secs, 300);
        assert_eq!(policy.max_stall_restarts, 2);
    }

    #[test]
    fn supervisory_progress_signal_treats_triage_backlog_as_expected_work() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                architect_member("architect"),
                manager_member("lead", Some("architect")),
                engineer_member("eng-1", Some("lead"), false),
            ])
            .build();
        let threshold = daemon
            .config
            .team_config
            .workflow_policy
            .stall_threshold_secs;
        insert_working_shim_handle(&mut daemon, "lead", threshold + 10, threshold + 10);

        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, "lead").unwrap();
        let mut result = InboxMessage::new_send("eng-1", "lead", "task complete");
        result.timestamp = crate::team::now_unix();
        let id = inbox::deliver_to_inbox(&inbox_root, &result).unwrap();
        inbox::mark_delivered(&inbox_root, "lead", &id).unwrap();

        let signal = daemon.supervisory_progress_signal("lead", threshold);
        assert_eq!(signal.short_label(), "inbox batching");
        assert!(!daemon.is_supervisory_lane_stalled("lead", threshold));
    }

    #[test]
    fn supervisory_progress_signal_treats_review_backlog_as_expected_work() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                architect_member("architect"),
                manager_member("lead", Some("architect")),
                engineer_member("eng-1", Some("lead"), false),
            ])
            .build();
        let threshold = daemon
            .config
            .team_config
            .workflow_policy
            .stall_threshold_secs;
        insert_working_shim_handle(&mut daemon, "lead", threshold + 10, threshold + 10);
        write_owned_task_file(tmp.path(), 191, "review-task", "review", "eng-1");

        let signal = daemon.supervisory_progress_signal("lead", threshold);
        assert_eq!(signal.short_label(), "review waiting");
        assert!(!daemon.is_supervisory_lane_stalled("lead", threshold));
    }

    #[test]
    fn supervisory_progress_signal_treats_recent_live_delivery_as_expected_work() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                architect_member("architect"),
                manager_member("lead", Some("architect")),
            ])
            .build();
        let threshold = daemon
            .config
            .team_config
            .workflow_policy
            .stall_threshold_secs;
        insert_working_shim_handle(&mut daemon, "lead", threshold + 10, threshold + 10);

        let mut delivered = TeamEvent::notification_delivery_sample("architect", "lead", 0, "live");
        delivered.ts = crate::team::now_unix();
        write_event_log(tmp.path(), &[delivered]);

        let signal = daemon.supervisory_progress_signal("lead", threshold);
        assert_eq!(
            signal,
            SupervisoryProgress::Expected("fresh_supervisory_input")
        );
        assert!(!daemon.is_supervisory_lane_stalled("lead", threshold));
    }

    #[test]
    fn supervisory_progress_signal_treats_recent_digest_delivery_as_expected_work() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                architect_member("architect"),
                manager_member("lead", Some("architect")),
                engineer_member("eng-1", Some("lead"), false),
            ])
            .build();
        let threshold = daemon
            .config
            .team_config
            .workflow_policy
            .stall_threshold_secs;
        insert_working_shim_handle(&mut daemon, "lead", threshold + 10, threshold + 10);

        let now = crate::team::now_unix();
        let mut digest = TeamEvent::supervisory_digest_emitted("lead", 3, 1);
        digest.ts = now;
        let mut delivered = TeamEvent::notification_delivery_sample("eng-1", "lead", 0, "digest");
        delivered.ts = now;
        write_event_log(tmp.path(), &[digest, delivered]);

        let signal = daemon.supervisory_progress_signal("lead", threshold);
        assert_eq!(signal, SupervisoryProgress::Expected("supervisory_digest"));
        assert!(!daemon.is_supervisory_lane_stalled("lead", threshold));
    }

    #[test]
    fn record_supervisory_stall_reason_emits_role_specific_summary() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![architect_member("architect")])
            .build();
        let threshold = daemon
            .config
            .team_config
            .workflow_policy
            .stall_threshold_secs;
        insert_working_shim_handle(&mut daemon, "architect", threshold + 12, threshold + 12);

        daemon.record_supervisory_stall_reason("architect", threshold, SupervisoryProgress::None);

        let events =
            crate::team::events::read_events(&crate::team::team_events_path(tmp.path())).unwrap();
        let stall = events
            .iter()
            .find(|event| event.event == "stall_detected")
            .expect("expected supervisory stall event");
        assert_eq!(stall.task.as_deref(), Some("supervisory::architect"));
        assert_eq!(
            stall.details.as_deref(),
            Some("architect (architect) stalled after 5m: no actionable progress")
        );
    }

    #[test]
    fn stall_restart_cooldown_prevents_repeat_restart() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-stall-cd";
        let lead_name = "lead-stall-cd";

        let lead = MemberInstance {
            name: lead_name.to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };
        let engineer = MemberInstance {
            name: member_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some(lead_name.to_string()),
            use_worktrees: false,
            ..Default::default()
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![lead, engineer]);
        daemon.active_tasks.insert(member_name.to_string(), 77);
        daemon
            .config
            .pane_map
            .insert(member_name.to_string(), "%999".to_string());
        write_owned_task_file(tmp.path(), 77, "cooldown-task", "in-progress", member_name);

        daemon
            .intervention_cooldowns
            .insert(format!("stall-restart::{member_name}"), Instant::now());

        daemon.handle_stalled_agent(member_name, 300).unwrap();

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap_or_default();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.event == "stall_detected" || e.event == "agent_restarted")
                .count(),
            0,
            "cooldown should suppress all stall handling"
        );
    }

    #[test]
    fn handle_stalled_agent_no_task_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![engineer_member("eng-1", Some("manager"), false)])
            .build();
        let result = daemon.handle_stalled_agent("eng-1", 600);
        assert!(result.is_ok());
        let events_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        let events = crate::team::events::read_events(&events_path).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn handle_stalled_agent_cooldown_prevents_action() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-1";
        write_owned_task_file(tmp.path(), 42, "test-task", "in-progress", member_name);
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, member_name).unwrap();

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![engineer_member(member_name, Some("manager"), false)])
            .build();
        daemon.active_tasks.insert(member_name.to_string(), 42);
        daemon
            .intervention_cooldowns
            .insert(format!("stall-restart::{member_name}"), Instant::now());

        let result = daemon.handle_stalled_agent(member_name, 600);
        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn stall_restart_relaunches_stalled_agent_with_task_context() {
        let session = format!("batty-test-stall-restart-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-stall";
        let lead_name = "lead-stall";
        let (fake_bin, fake_log) = setup_fake_claude(&tmp, member_name);
        let worktree_path = tmp.path().join("worktree");
        std::fs::create_dir_all(&worktree_path).unwrap();

        crate::tmux::create_session(&session, "bash", &[], tmp.path().to_str().unwrap()).unwrap();
        crate::tmux::create_window(
            &session,
            "keeper",
            "sleep",
            &["30".to_string()],
            tmp.path().to_str().unwrap(),
        )
        .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        Command::new("tmux")
            .args(["set-option", "-p", "-t", &pane_id, "remain-on-exit", "on"])
            .output()
            .unwrap();

        let lead = MemberInstance {
            name: lead_name.to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
            ..Default::default()
        };
        let engineer = MemberInstance {
            name: member_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some(lead_name.to_string()),
            use_worktrees: false,
            ..Default::default()
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                agent: None,
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                external_senders: Vec::new(),
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                grafana: Default::default(),
                use_shim: false,
                use_sdk_mode: false,
                auto_respawn_on_crash: false,
                shim_health_check_interval_secs: 60,
                shim_health_timeout_secs: 120,
                shim_shutdown_timeout_secs: 30,
                shim_working_state_timeout_secs: 1800,
                pending_queue_max_age_secs: 600,
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.clone(),
            members: vec![lead, engineer],
            pane_map: HashMap::from([(member_name.to_string(), pane_id.clone())]),
        })
        .unwrap();
        daemon
            .states
            .insert(member_name.to_string(), MemberState::Working);
        daemon.active_tasks.insert(member_name.to_string(), 42);

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, member_name).unwrap();
        inbox::init_inbox(&root, lead_name).unwrap();
        write_owned_task_file_with_context(
            tmp.path(),
            42,
            "stall-test-task",
            "in-progress",
            member_name,
            "eng-stall/42",
            &worktree_path.display().to_string(),
        );

        daemon.handle_stalled_agent(member_name, 300).unwrap();

        let log = (0..100)
            .find_map(|_| {
                let content = match std::fs::read_to_string(&fake_log) {
                    Ok(content) => content,
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(100));
                        return None;
                    }
                };
                if content.contains("Continuing Task #42") {
                    Some(content)
                } else {
                    std::thread::sleep(Duration::from_millis(100));
                    None
                }
            })
            .unwrap_or_else(|| {
                panic!(
                    "fake claude log was not written by restarted member at {}",
                    fake_log.display()
                )
            });
        assert!(log.contains("stall-test-task"));
        assert!(log.contains("Branch: eng-stall/42"));
        assert!(log.contains(&format!("Worktree: {}", worktree_path.display())));

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();

        let stall_events: Vec<_> = events
            .iter()
            .filter(|e| e.event == "stall_detected")
            .collect();
        assert_eq!(stall_events.len(), 1);
        assert_eq!(stall_events[0].role.as_deref(), Some(member_name));
        assert_eq!(stall_events[0].task.as_deref(), Some("42"));
        assert_eq!(stall_events[0].uptime_secs, Some(300));

        let restart_events: Vec<_> = events
            .iter()
            .filter(|e| e.event == "agent_restarted")
            .collect();
        assert_eq!(restart_events.len(), 1);
        assert_eq!(restart_events[0].role.as_deref(), Some(member_name));
        assert_eq!(restart_events[0].task.as_deref(), Some("42"));
        assert_eq!(restart_events[0].reason.as_deref(), Some("stalled"));
        assert_eq!(restart_events[0].restart_count, Some(1));

        assert!(events.iter().any(|e| {
            e.event == "message_routed"
                && e.from.as_deref() == Some("daemon")
                && e.to.as_deref() == Some(member_name)
        }));

        crate::tmux::kill_session(&session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
    }

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn stall_escalates_after_max_restarts() {
        let session = format!("batty-test-stall-escalate-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-stall-esc";
        let lead_name = "lead-stall-esc";
        let (fake_bin, fake_log) = setup_fake_claude(&tmp, member_name);

        crate::tmux::create_session(&session, "bash", &[], tmp.path().to_str().unwrap()).unwrap();
        crate::tmux::create_window(
            &session,
            "keeper",
            "sleep",
            &["30".to_string()],
            tmp.path().to_str().unwrap(),
        )
        .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();

        let lead = MemberInstance {
            name: lead_name.to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
            ..Default::default()
        };
        let engineer = MemberInstance {
            name: member_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some(lead_name.to_string()),
            use_worktrees: false,
            ..Default::default()
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                agent: None,
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy {
                    max_stall_restarts: 2,
                    ..WorkflowPolicy::default()
                },
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                external_senders: Vec::new(),
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                grafana: Default::default(),
                use_shim: false,
                use_sdk_mode: false,
                auto_respawn_on_crash: false,
                shim_health_check_interval_secs: 60,
                shim_health_timeout_secs: 120,
                shim_shutdown_timeout_secs: 30,
                shim_working_state_timeout_secs: 1800,
                pending_queue_max_age_secs: 600,
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.clone(),
            members: vec![lead, engineer],
            pane_map: HashMap::from([(member_name.to_string(), pane_id)]),
        })
        .unwrap();
        daemon
            .states
            .insert(member_name.to_string(), MemberState::Working);
        daemon.active_tasks.insert(member_name.to_string(), 50);

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, lead_name).unwrap();
        write_owned_task_file(
            tmp.path(),
            50,
            "stall-escalate-task",
            "in-progress",
            member_name,
        );

        write_event_log(
            tmp.path(),
            &[
                TeamEvent::agent_restarted(member_name, "50", "stalled", 1),
                TeamEvent::agent_restarted(member_name, "50", "stalled", 2),
            ],
        );

        daemon.handle_stalled_agent(member_name, 600).unwrap();

        let pending = inbox::pending_messages(&root, lead_name).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].body.contains("Task #50"));
        assert!(pending[0].body.contains("stalled"));
        assert!(pending[0].body.contains("will not restart again"));

        let log = std::fs::read_to_string(&fake_log).unwrap_or_default();
        assert!(!log.contains("Continuing Task #50"));

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.event == "agent_restarted")
                .count(),
            2
        );
        assert!(events.iter().any(|e| {
            e.event == "task_escalated"
                && e.role.as_deref() == Some(member_name)
                && e.reason.as_deref() == Some("stalled")
        }));
        assert!(events.iter().any(|e| e.event == "stall_detected"));

        crate::tmux::kill_session(&session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
    }

    // Checkpoint-related stall tests are in separate files; they cover gather/write/read_checkpoint
    // which are tested via the context_exhaustion and stall flows.

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn stall_checkpoint_restart_resume_full_flow() {
        use crate::team::test_support::init_git_repo;

        let session = format!("batty-test-stall-cp-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-stall-cp");

        let member_name = "eng-stall-cp";
        let lead_name = "lead-stall-cp";
        let (fake_bin, _fake_log) = setup_fake_claude(&tmp, member_name);

        let worktree_dir = repo.join(".batty").join("worktrees").join(member_name);
        let team_config_dir = repo.join(".batty").join("team_config");
        crate::team::task_loop::setup_engineer_worktree(
            &repo,
            &worktree_dir,
            member_name,
            &team_config_dir,
        )
        .unwrap();

        // Create a task branch with some work
        let task_branch = format!("{member_name}/42");
        crate::team::test_support::git_ok(&worktree_dir, &["checkout", "-b", &task_branch]);
        std::fs::write(worktree_dir.join("work.rs"), "fn main() {}\n").unwrap();
        crate::team::test_support::git_ok(&worktree_dir, &["add", "work.rs"]);
        crate::team::test_support::git_ok(&worktree_dir, &["commit", "-m", "partial impl"]);

        let inbox_root = inbox::inboxes_root(&repo);
        inbox::init_inbox(&inbox_root, member_name).unwrap();
        inbox::init_inbox(&inbox_root, lead_name).unwrap();

        write_owned_task_file_with_context(
            &repo,
            42,
            "stall-cp-task",
            "in-progress",
            member_name,
            &task_branch,
            &worktree_dir.display().to_string(),
        );

        crate::tmux::create_session(&session, "bash", &[], repo.to_string_lossy().as_ref())
            .unwrap();
        crate::tmux::create_window(
            &session,
            "keeper",
            "sleep",
            &["60".to_string()],
            repo.to_string_lossy().as_ref(),
        )
        .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        Command::new("tmux")
            .args(["set-option", "-p", "-t", &pane_id, "remain-on-exit", "on"])
            .output()
            .unwrap();

        let lead = MemberInstance {
            name: lead_name.to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
            ..Default::default()
        };
        let engineer = MemberInstance {
            name: member_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some(lead_name.to_string()),
            use_worktrees: true,
            ..Default::default()
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: repo.clone(),
            team_config: test_team_config("stall-cp"),
            session: session.clone(),
            members: vec![lead, engineer],
            pane_map: HashMap::from([(member_name.to_string(), pane_id)]),
        })
        .unwrap();
        daemon
            .states
            .insert(member_name.to_string(), MemberState::Working);
        daemon.active_tasks.insert(member_name.to_string(), 42);

        daemon.handle_stalled_agent(member_name, 300).unwrap();

        // Checkpoint should have been written
        let cp_content = crate::team::checkpoint::read_checkpoint(&repo, member_name);
        assert!(
            cp_content.is_some(),
            "checkpoint should be written before stall restart"
        );
        let cp_text = cp_content.unwrap();
        assert!(cp_text.contains("**Task:** #42"));
        assert!(cp_text.contains(&task_branch));

        // Restart notice should contain checkpoint content
        let msgs = inbox::pending_messages(&inbox_root, member_name).unwrap();
        let restart_msg = msgs
            .iter()
            .find(|m| m.body.contains("Restarted after stall"));
        assert!(restart_msg.is_some(), "restart notice should be queued");
        let body = &restart_msg.unwrap().body;
        assert!(
            body.contains("[RESUMING FROM CHECKPOINT]"),
            "restart notice should include checkpoint"
        );
        assert!(body.contains("partial impl"));

        crate::tmux::kill_session(&session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
        let _ = Command::new("git")
            .current_dir(&repo)
            .args([
                "worktree",
                "remove",
                "--force",
                worktree_dir.to_str().unwrap(),
            ])
            .output();
    }

    #[test]
    fn stall_with_no_active_task_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-stall-no-task";
        let engineer = MemberInstance {
            name: member_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
            ..Default::default()
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);
        daemon
            .config
            .pane_map
            .insert(member_name.to_string(), "%999".to_string());
        // active_tasks does NOT contain member — so active_task returns None.

        daemon.handle_stalled_agent(member_name, 300).unwrap();

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap_or_default();
        assert!(
            events.is_empty(),
            "stall with no active task should be a noop"
        );
    }

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn stall_overwrites_existing_checkpoint() {
        use crate::team::test_support::init_git_repo;

        let session = format!("batty-test-stall-overwrite-cp-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-stall-overwrite");

        let member_name = "eng-stall-ow";
        let lead_name = "lead-stall-ow";
        let (fake_bin, _fake_log) = setup_fake_claude(&tmp, member_name);

        let worktree_dir = repo.join(".batty").join("worktrees").join(member_name);
        let team_config_dir = repo.join(".batty").join("team_config");
        crate::team::task_loop::setup_engineer_worktree(
            &repo,
            &worktree_dir,
            member_name,
            &team_config_dir,
        )
        .unwrap();

        let task_branch = format!("{member_name}/55");
        crate::team::test_support::git_ok(&worktree_dir, &["checkout", "-b", &task_branch]);
        std::fs::write(worktree_dir.join("v1.rs"), "fn v1() {}\n").unwrap();
        crate::team::test_support::git_ok(&worktree_dir, &["add", "v1.rs"]);
        crate::team::test_support::git_ok(&worktree_dir, &["commit", "-m", "first version"]);

        let inbox_root = inbox::inboxes_root(&repo);
        inbox::init_inbox(&inbox_root, member_name).unwrap();
        inbox::init_inbox(&inbox_root, lead_name).unwrap();

        write_owned_task_file_with_context(
            &repo,
            55,
            "overwrite-cp-task",
            "in-progress",
            member_name,
            &task_branch,
            &worktree_dir.display().to_string(),
        );

        // Write an initial checkpoint that should get overwritten.
        let old_cp = crate::team::checkpoint::Checkpoint {
            role: member_name.to_string(),
            task_id: 55,
            task_title: "OLD TITLE".to_string(),
            task_description: "OLD DESC".to_string(),
            branch: Some("old-branch".to_string()),
            last_commit: Some("old-commit".to_string()),
            test_summary: None,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
        };
        crate::team::checkpoint::write_checkpoint(&repo, &old_cp).unwrap();

        crate::tmux::create_session(&session, "bash", &[], repo.to_string_lossy().as_ref())
            .unwrap();
        crate::tmux::create_window(
            &session,
            "keeper",
            "sleep",
            &["60".to_string()],
            repo.to_string_lossy().as_ref(),
        )
        .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        Command::new("tmux")
            .args(["set-option", "-p", "-t", &pane_id, "remain-on-exit", "on"])
            .output()
            .unwrap();

        let lead = MemberInstance {
            name: lead_name.to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
            ..Default::default()
        };
        let engineer = MemberInstance {
            name: member_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some(lead_name.to_string()),
            use_worktrees: true,
            ..Default::default()
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: repo.clone(),
            team_config: test_team_config("stall-ow"),
            session: session.clone(),
            members: vec![lead, engineer],
            pane_map: HashMap::from([(member_name.to_string(), pane_id)]),
        })
        .unwrap();
        daemon
            .states
            .insert(member_name.to_string(), MemberState::Working);
        daemon.active_tasks.insert(member_name.to_string(), 55);

        daemon.handle_stalled_agent(member_name, 300).unwrap();

        let new_cp = crate::team::checkpoint::read_checkpoint(&repo, member_name).unwrap();
        assert!(
            !new_cp.contains("OLD TITLE"),
            "old checkpoint should have been overwritten"
        );
        assert!(
            new_cp.contains("overwrite-cp-task"),
            "new checkpoint should contain current task title"
        );
        assert!(
            new_cp.contains(&task_branch),
            "new checkpoint should contain current branch"
        );

        crate::tmux::kill_session(&session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
        let _ = Command::new("git")
            .current_dir(&repo)
            .args([
                "worktree",
                "remove",
                "--force",
                worktree_dir.to_str().unwrap(),
            ])
            .output();
    }

    #[test]
    #[serial]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn stall_checkpoint_with_missing_worktree() {
        let session = format!("batty-test-stall-no-wt-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-stall-no-wt";
        let lead_name = "lead-stall-no-wt";
        let (fake_bin, _fake_log) = setup_fake_claude(&tmp, member_name);

        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, member_name).unwrap();
        inbox::init_inbox(&inbox_root, lead_name).unwrap();

        write_owned_task_file(tmp.path(), 66, "no-wt-task", "in-progress", member_name);

        crate::tmux::create_session(&session, "bash", &[], tmp.path().to_str().unwrap()).unwrap();
        crate::tmux::create_window(
            &session,
            "keeper",
            "sleep",
            &["60".to_string()],
            tmp.path().to_str().unwrap(),
        )
        .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        Command::new("tmux")
            .args(["set-option", "-p", "-t", &pane_id, "remain-on-exit", "on"])
            .output()
            .unwrap();

        let lead = MemberInstance {
            name: lead_name.to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
            ..Default::default()
        };
        let engineer = MemberInstance {
            name: member_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some(lead_name.to_string()),
            use_worktrees: false,
            ..Default::default()
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: test_team_config("stall-no-wt"),
            session: session.clone(),
            members: vec![lead, engineer],
            pane_map: HashMap::from([(member_name.to_string(), pane_id)]),
        })
        .unwrap();
        daemon
            .states
            .insert(member_name.to_string(), MemberState::Working);
        daemon.active_tasks.insert(member_name.to_string(), 66);

        // Should not panic even without a valid worktree.
        daemon.handle_stalled_agent(member_name, 300).unwrap();

        // A checkpoint should still have been written (with None for branch/last_commit).
        let cp = crate::team::checkpoint::read_checkpoint(tmp.path(), member_name);
        assert!(
            cp.is_some(),
            "checkpoint should be written even without worktree"
        );

        crate::tmux::kill_session(&session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
    }

    #[test]
    fn stall_checkpoint_cleared_on_task_clear() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-stall-clear";
        let engineer = MemberInstance {
            name: member_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
            ..Default::default()
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);
        daemon.active_tasks.insert(member_name.to_string(), 88);

        // Write a checkpoint
        let cp = crate::team::checkpoint::Checkpoint {
            role: member_name.to_string(),
            task_id: 88,
            task_title: "clear-test".to_string(),
            task_description: "desc".to_string(),
            branch: None,
            last_commit: None,
            test_summary: None,
            timestamp: "2026-03-22T00:00:00Z".to_string(),
        };
        crate::team::checkpoint::write_checkpoint(tmp.path(), &cp).unwrap();
        assert!(crate::team::checkpoint::read_checkpoint(tmp.path(), member_name).is_some());

        daemon.clear_active_task(member_name);

        // Checkpoint should be cleared along with the task.
        let cp_after = crate::team::checkpoint::read_checkpoint(tmp.path(), member_name);
        assert!(
            cp_after.is_none(),
            "checkpoint should be cleared when task is cleared"
        );
        assert!(!daemon.active_tasks.contains_key(member_name));
    }
}
