//! Proactive context-pressure tracking based on output growth plus agent behavior.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use tracing::{debug, warn};

use super::super::*;
use crate::team::watcher::CodexQualitySignals;

const WARNING_PERCENT: u64 = 70;
const NUDGE_PERCENT: u64 = 90;
const RESTART_AFTER_OVER_THRESHOLD_POLLS: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextPressureAction {
    Warn,
    Nudge,
    Restart,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ContextPressureInputs {
    output_bytes: u64,
    narration_detected: bool,
    meta_conversation_detected: bool,
    assistant_message_count: u32,
    tool_call_count: u32,
    unique_tool_names: usize,
    shrinking_responses: bool,
    repeated_identical_outputs: bool,
    tool_failure_message: Option<String>,
    secs_since_last_commit: Option<u64>,
}

#[derive(Debug, Clone, Default)]
struct ContextPressureState {
    last_output_bytes: u64,
    warning_emitted: bool,
    nudge_sent: bool,
    over_threshold_polls: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct ContextPressureTracker {
    members: HashMap<String, ContextPressureState>,
    threshold: u64,
    threshold_bytes: u64,
}

impl Default for ContextPressureTracker {
    fn default() -> Self {
        Self::new(100, 512_000)
    }
}

impl ContextPressureTracker {
    pub(crate) fn new(threshold: u64, threshold_bytes: u64) -> Self {
        Self {
            members: HashMap::new(),
            threshold: threshold.max(1),
            threshold_bytes: threshold_bytes.max(1),
        }
    }

    pub(super) fn clear_member(&mut self, member: &str) {
        self.members.remove(member);
    }

    pub(super) fn mark_not_working(&mut self, member: &str) {
        if let Some(state) = self.members.get_mut(member) {
            state.over_threshold_polls = 0;
        }
    }

    pub(super) fn observe_at(
        &mut self,
        member: &str,
        inputs: &ContextPressureInputs,
        is_working: bool,
        _now: Instant,
    ) -> (u64, Vec<ContextPressureAction>) {
        let warn_at = self.threshold.saturating_mul(WARNING_PERCENT) / 100;
        let nudge_at = self.threshold.saturating_mul(NUDGE_PERCENT) / 100;

        let state = self.members.entry(member.to_string()).or_default();
        if inputs.output_bytes < state.last_output_bytes {
            *state = ContextPressureState::default();
        }
        state.last_output_bytes = inputs.output_bytes;

        if !is_working {
            state.over_threshold_polls = 0;
            return (0, Vec::new());
        }

        let score = compute_pressure_score(inputs, self.threshold_bytes);
        let mut actions = Vec::new();
        if score >= warn_at && !state.warning_emitted {
            state.warning_emitted = true;
            actions.push(ContextPressureAction::Warn);
        }
        if score >= nudge_at && !state.nudge_sent {
            state.nudge_sent = true;
            actions.push(ContextPressureAction::Nudge);
        }

        if score >= self.threshold {
            state.over_threshold_polls = state.over_threshold_polls.saturating_add(1);
            if state.over_threshold_polls >= RESTART_AFTER_OVER_THRESHOLD_POLLS {
                actions.push(ContextPressureAction::Restart);
            }
        } else {
            state.over_threshold_polls = 0;
        }

        (score, actions)
    }
}

fn compute_pressure_score(inputs: &ContextPressureInputs, threshold_bytes: u64) -> u64 {
    let mut score = 0;
    score += inputs.output_bytes.saturating_mul(35) / threshold_bytes.max(1);
    score = score.min(35);

    if inputs.narration_detected {
        score += 25;
    }
    if inputs.meta_conversation_detected {
        score += 30;
    }
    if inputs.assistant_message_count >= 3 && inputs.tool_call_count == 0 {
        score += 25;
    } else if inputs.assistant_message_count >= 4
        && inputs.assistant_message_count >= inputs.tool_call_count.saturating_mul(3)
    {
        score += 15;
    }
    if inputs.tool_call_count >= 3 && inputs.unique_tool_names <= 1 {
        score += 10;
    }
    if inputs.repeated_identical_outputs {
        score += 20;
    }
    if inputs.shrinking_responses {
        score += 15;
    }
    if inputs.tool_failure_message.is_some() {
        score += 10;
    }
    if inputs.assistant_message_count >= 3
        && inputs.output_bytes >= threshold_bytes / 2
        && inputs.secs_since_last_commit.unwrap_or(u64::MAX) >= 900
    {
        score += 15;
    }

    score
}

/// Minimum uptime (seconds) before a zero-output agent is considered dead.
const ZERO_OUTPUT_THRESHOLD_SECS: u64 = 600;

impl TeamDaemon {
    pub(in super::super) fn handle_context_pressure_stats(
        &mut self,
        member_name: &str,
        output_bytes: u64,
        uptime_secs: u64,
    ) -> Result<()> {
        if output_bytes == 0 && uptime_secs >= ZERO_OUTPUT_THRESHOLD_SECS {
            if self.shim_handles.contains_key(member_name) {
                warn!(
                    member = %member_name,
                    uptime_secs,
                    "zero-output agent detected — restarting"
                );
                self.record_orchestrator_action(format!(
                    "health: restarting {} after {}s with zero output",
                    member_name, uptime_secs
                ));
                self.handle_shim_cold_respawn(member_name, "zero output")?;
                self.context_pressure_tracker.clear_member(member_name);
            }
            return Ok(());
        }

        let is_working = self.states.get(member_name) == Some(&MemberState::Working);
        let threshold = self
            .config
            .team_config
            .workflow_policy
            .context_pressure_threshold;
        let inputs = self.context_pressure_inputs(member_name, output_bytes);
        let (score, actions) = self.context_pressure_tracker.observe_at(
            member_name,
            &inputs,
            is_working,
            Instant::now(),
        );
        if actions.is_empty() {
            return Ok(());
        }

        let task_id = self.active_task_id(member_name);
        for action in actions {
            match action {
                ContextPressureAction::Warn => {
                    warn!(
                        member = %member_name,
                        task_id,
                        output_bytes,
                        pressure_score = score,
                        threshold,
                        uptime_secs,
                        "detected rising context pressure"
                    );
                    self.record_context_pressure_warning(
                        member_name,
                        task_id,
                        score,
                        threshold,
                        output_bytes,
                    );
                    self.record_orchestrator_action(format!(
                        "health: context pressure warning for {} (score={}/{}, output_bytes={})",
                        member_name, score, threshold, output_bytes
                    ));
                }
                ContextPressureAction::Nudge => {
                    if let Some(member) = self
                        .config
                        .members
                        .iter()
                        .find(|member| member.name == member_name)
                        .cloned()
                    {
                        let message = self.prepend_member_nudge(
                            &member,
                            format!(
                                "Context pressure is high (score {score}/{threshold}). Commit current work if possible, then move directly to commands and edits."
                            ),
                        );
                        if let Err(error) = self.queue_message("daemon", member_name, &message) {
                            warn!(
                                member = %member_name,
                                error = %error,
                                "failed to queue context-pressure nudge"
                            );
                        }
                    }
                    self.record_orchestrator_action(format!(
                        "health: nudged {} to wrap up under context pressure (score={}/{})",
                        member_name, score, threshold
                    ));
                }
                ContextPressureAction::Restart => {
                    if !self.shim_handles.contains_key(member_name) {
                        continue;
                    }

                    self.handle_context_pressure_restart(member_name)?;
                    if let Some(task_id) = task_id {
                        self.record_agent_restarted(
                            member_name,
                            task_id.to_string(),
                            "context_pressure",
                            1,
                        );
                    }
                    self.record_orchestrator_action(format!(
                        "health: restarted {} after sustained context pressure (score={}/{})",
                        member_name, score, threshold
                    ));
                    self.context_pressure_tracker.clear_member(member_name);
                }
            }
        }

        Ok(())
    }

    fn context_pressure_inputs(
        &mut self,
        member_name: &str,
        output_bytes: u64,
    ) -> ContextPressureInputs {
        let quality = self
            .current_codex_quality_signals(member_name)
            .unwrap_or_default();
        ContextPressureInputs {
            output_bytes,
            narration_detected: self.narration_tracker.is_narrating(member_name),
            meta_conversation_detected: self.narration_tracker.is_narrating(member_name),
            assistant_message_count: quality.assistant_message_count,
            tool_call_count: quality.tool_call_count,
            unique_tool_names: quality.unique_tool_names.len(),
            shrinking_responses: quality.shrinking_responses,
            repeated_identical_outputs: quality.repeated_identical_outputs,
            tool_failure_message: quality.tool_failure_message,
            secs_since_last_commit: self.secs_since_last_commit(member_name),
        }
    }

    fn current_codex_quality_signals(&mut self, member_name: &str) -> Option<CodexQualitySignals> {
        let watcher = self.watchers.get_mut(member_name)?;
        if let Err(error) = watcher.refresh_session_tracking() {
            debug!(
                member = member_name,
                error = %error,
                "failed to refresh watcher session data for context pressure"
            );
        }
        watcher.codex_quality_signals()
    }

    fn secs_since_last_commit(&self, member_name: &str) -> Option<u64> {
        let member = self
            .config
            .members
            .iter()
            .find(|member| member.name == member_name)?;
        let work_dir = self.member_work_dir(member);
        if !work_dir.exists() {
            return None;
        }

        let output = std::process::Command::new("git")
            .args(["log", "-1", "--format=%ct"])
            .current_dir(work_dir)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }

        let commit_ts = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<u64>()
            .ok()?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs();
        Some(now.saturating_sub(commit_ts))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backdate_score(tracker: &mut ContextPressureTracker, member: &str, output_bytes: u64) {
        let state = tracker.members.entry(member.to_string()).or_default();
        state.last_output_bytes = output_bytes;
        state.over_threshold_polls = 1;
    }

    fn pressure_inputs() -> ContextPressureInputs {
        ContextPressureInputs {
            output_bytes: 450_000,
            narration_detected: true,
            meta_conversation_detected: true,
            assistant_message_count: 4,
            tool_call_count: 0,
            unique_tool_names: 0,
            shrinking_responses: true,
            repeated_identical_outputs: false,
            tool_failure_message: None,
            secs_since_last_commit: Some(2_000),
        }
    }

    #[test]
    fn default_tracker_thresholds() {
        let tracker = ContextPressureTracker::default();
        assert_eq!(tracker.threshold, 100);
        assert_eq!(tracker.threshold_bytes, 512_000);
    }

    #[test]
    fn score_rewards_real_action_diversity() {
        let inputs = ContextPressureInputs {
            output_bytes: 250_000,
            narration_detected: false,
            meta_conversation_detected: false,
            assistant_message_count: 2,
            tool_call_count: 4,
            unique_tool_names: 3,
            shrinking_responses: false,
            repeated_identical_outputs: false,
            tool_failure_message: None,
            secs_since_last_commit: Some(60),
        };
        assert!(compute_pressure_score(&inputs, 512_000) < 30);
    }

    #[test]
    fn threshold_crossing_detection_warns_at_seventy_percent() {
        let mut tracker = ContextPressureTracker::new(100, 512_000);
        let (score, actions) =
            tracker.observe_at("eng-1", &pressure_inputs(), true, Instant::now());
        assert!(score >= 70);
        assert_eq!(
            actions,
            vec![ContextPressureAction::Warn, ContextPressureAction::Nudge]
        );
    }

    #[test]
    fn brief_planning_before_tool_use_does_not_trip_pressure_thresholds() {
        let mut tracker = ContextPressureTracker::new(100, 512_000);
        let inputs = ContextPressureInputs {
            output_bytes: 120_000,
            narration_detected: false,
            meta_conversation_detected: false,
            assistant_message_count: 2,
            tool_call_count: 0,
            unique_tool_names: 0,
            shrinking_responses: false,
            repeated_identical_outputs: false,
            tool_failure_message: None,
            secs_since_last_commit: Some(120),
        };

        let (score, actions) = tracker.observe_at("eng-1", &inputs, true, Instant::now());
        assert!(
            score < 70,
            "brief planning should stay below the warning threshold"
        );
        assert!(actions.is_empty());
    }

    #[test]
    fn graduated_response_levels_fire_in_order() {
        let mut tracker = ContextPressureTracker::new(100, 512_000);
        let inputs = pressure_inputs();
        assert_eq!(
            tracker.observe_at("eng-1", &inputs, true, Instant::now()).1,
            vec![ContextPressureAction::Warn, ContextPressureAction::Nudge]
        );
        backdate_score(&mut tracker, "eng-1", inputs.output_bytes);
        assert_eq!(
            tracker.observe_at("eng-1", &inputs, true, Instant::now()).1,
            vec![ContextPressureAction::Restart]
        );
    }

    #[test]
    fn counter_reset_on_session_restart_rewarns_after_output_drops() {
        let mut tracker = ContextPressureTracker::new(100, 512_000);
        let inputs = pressure_inputs();
        assert_eq!(
            tracker.observe_at("eng-1", &inputs, true, Instant::now()).1,
            vec![ContextPressureAction::Warn, ContextPressureAction::Nudge]
        );
        let reset_inputs = ContextPressureInputs {
            output_bytes: 100,
            narration_detected: false,
            meta_conversation_detected: false,
            assistant_message_count: 0,
            tool_call_count: 0,
            unique_tool_names: 0,
            shrinking_responses: false,
            repeated_identical_outputs: false,
            tool_failure_message: None,
            secs_since_last_commit: Some(0),
        };
        assert!(
            tracker
                .observe_at("eng-1", &reset_inputs, true, Instant::now())
                .1
                .is_empty()
        );
        assert_eq!(
            tracker.observe_at("eng-1", &inputs, true, Instant::now()).1,
            vec![ContextPressureAction::Warn, ContextPressureAction::Nudge]
        );
    }

    #[test]
    fn idle_member_clears_restart_counter() {
        let mut tracker = ContextPressureTracker::new(100, 512_000);
        backdate_score(&mut tracker, "eng-1", 512_000);
        assert!(
            tracker
                .observe_at("eng-1", &pressure_inputs(), false, Instant::now())
                .1
                .is_empty()
        );
        assert_eq!(tracker.members["eng-1"].over_threshold_polls, 0);
    }

    #[test]
    fn zero_output_threshold_is_ten_minutes() {
        assert_eq!(
            ZERO_OUTPUT_THRESHOLD_SECS, 600,
            "zero-output detection should trigger after 10 minutes"
        );
    }
}
