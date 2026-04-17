//! Proactive context-pressure tracking based on output growth plus agent behavior.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tracing::{debug, warn};

use super::super::*;
use crate::team::context_management;
use crate::team::watcher::CodexQualitySignals;

const WARNING_PERCENT: u64 = 70;
const NUDGE_PERCENT: u64 = 90;
const CLAUDE_PROACTIVE_RESTART_PCT: u64 = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextPressureAction {
    Warn,
    Nudge,
    Restart,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ContextPressureInputs {
    output_bytes: u64,
    session_uptime_secs: u64,
    proactive_context_usage_pct: Option<u64>,
    narration_detected: bool,
    meta_conversation_detected: bool,
    assistant_message_count: u32,
    tool_call_count: u32,
    unique_tool_names: usize,
    shrinking_responses: bool,
    repeated_identical_outputs: bool,
    tool_failure_message: Option<String>,
    shim_failure_count: u32,
    secs_since_last_commit: Option<u64>,
}

#[derive(Debug, Clone, Default)]
struct ContextPressureState {
    last_output_bytes: u64,
    warning_emitted: bool,
    nudge_sent: bool,
    over_threshold_since: Option<Instant>,
    shim_failure_count: u32,
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
            state.over_threshold_since = None;
        }
    }

    pub(super) fn record_failure(&mut self, member: &str) {
        let state = self.members.entry(member.to_string()).or_default();
        state.shim_failure_count = state.shim_failure_count.saturating_add(1);
    }

    pub(super) fn shim_failure_count(&self, member: &str) -> u32 {
        self.members
            .get(member)
            .map(|state| state.shim_failure_count)
            .unwrap_or(0)
    }

    pub(super) fn observe_at(
        &mut self,
        member: &str,
        inputs: &ContextPressureInputs,
        is_working: bool,
        restart_delay_secs: u64,
        now: Instant,
    ) -> (u64, Vec<ContextPressureAction>) {
        let warn_at = self.threshold.saturating_mul(WARNING_PERCENT) / 100;
        let nudge_at = self.threshold.saturating_mul(NUDGE_PERCENT) / 100;

        let state = self.members.entry(member.to_string()).or_default();
        if inputs.output_bytes < state.last_output_bytes {
            *state = ContextPressureState::default();
        }
        state.last_output_bytes = inputs.output_bytes;

        if !is_working {
            state.over_threshold_since = None;
            return (0, Vec::new());
        }

        let score = compute_pressure_score(inputs, self.threshold_bytes);
        let score =
            if inputs.proactive_context_usage_pct.unwrap_or(0) >= CLAUDE_PROACTIVE_RESTART_PCT {
                score.max(self.threshold)
            } else {
                score
            };
        let mut actions = Vec::new();
        if score >= warn_at && !state.warning_emitted {
            state.warning_emitted = true;
            actions.push(ContextPressureAction::Warn);
        }
        if score >= nudge_at && !state.nudge_sent {
            state.nudge_sent = true;
            actions.push(ContextPressureAction::Nudge);
        }

        if should_force_restart(inputs, restart_delay_secs) {
            actions.push(ContextPressureAction::Restart);
            return (score, actions);
        }

        if score >= self.threshold {
            let since = state.over_threshold_since.get_or_insert(now);
            if now.duration_since(*since) >= Duration::from_secs(restart_delay_secs.max(1)) {
                actions.push(ContextPressureAction::Restart);
            }
        } else {
            state.over_threshold_since = None;
        }

        (score, actions)
    }
}

fn compute_pressure_score(inputs: &ContextPressureInputs, threshold_bytes: u64) -> u64 {
    let mut score = 0;
    score += inputs.output_bytes.saturating_mul(35) / threshold_bytes.max(1);
    score = score.min(35);

    if inputs.proactive_context_usage_pct.is_some() {
        score = score.max(100);
    }

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
    if inputs.shim_failure_count >= 2 {
        score += 10;
    }
    if inputs.shim_failure_count >= 3 {
        score += 15;
    }
    if inputs.assistant_message_count >= 3
        && inputs.output_bytes >= threshold_bytes / 2
        && inputs.secs_since_last_commit.unwrap_or(u64::MAX) >= 900
    {
        score += 15;
    }
    if inputs.session_uptime_secs >= 900 && inputs.secs_since_last_commit.unwrap_or(u64::MAX) >= 900
    {
        score += 15;
    }

    score
}

fn should_force_restart(inputs: &ContextPressureInputs, restart_delay_secs: u64) -> bool {
    let restart_delay_secs = restart_delay_secs.max(1);
    inputs.session_uptime_secs >= restart_delay_secs
        && inputs.shim_failure_count >= 3
        && inputs.secs_since_last_commit.unwrap_or(u64::MAX) >= restart_delay_secs
}

/// Minimum uptime (seconds) before a zero-output agent is considered dead.
const ZERO_OUTPUT_THRESHOLD_SECS: u64 = 600;

impl TeamDaemon {
    pub(in super::super) fn handle_context_pressure_stats(
        &mut self,
        member_name: &str,
        output_bytes: u64,
        uptime_secs: u64,
        _proactive_context_usage_pct: Option<u8>,
    ) -> Result<()> {
        if output_bytes == 0 && uptime_secs >= ZERO_OUTPUT_THRESHOLD_SECS {
            // #685: only treat zero-output as a hang when the daemon
            // actually expects the agent to be producing output. An idle
            // member with an empty inbox and no active task has nothing
            // to say — tearing them down every 10 minutes just burns a
            // fresh startup context for no behavioral change.
            let is_working = self.states.get(member_name) == Some(&MemberState::Working);
            if !is_working {
                return Ok(());
            }
            // #690: also require that the member has been CONTINUOUSLY
            // Working long enough to justify calling this a hang. An
            // Idle shim that just received its first inbox message 8s
            // ago is MemberState::Working, but has had no chance to
            // produce output. Without this guard we saw priya-writer
            // killed at 04:50:02 after transitioning Working at
            // 04:49:54 following an 18-minute Idle window.
            let been_working_long_enough = self
                .working_since
                .get(member_name)
                .map(|since| since.elapsed().as_secs() >= ZERO_OUTPUT_THRESHOLD_SECS)
                .unwrap_or(false);
            if !been_working_long_enough {
                return Ok(());
            }
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

        self.handle_context_pressure_signal(member_name, output_bytes, uptime_secs, None)
    }

    pub(in super::super) fn handle_context_pressure_warning(
        &mut self,
        member_name: &str,
        output_bytes: u64,
        uptime_secs: u64,
        usage_pct: u8,
    ) -> Result<()> {
        self.handle_context_pressure_signal(member_name, output_bytes, uptime_secs, Some(usage_pct))
    }

    fn handle_context_pressure_signal(
        &mut self,
        member_name: &str,
        output_bytes: u64,
        uptime_secs: u64,
        proactive_usage_pct: Option<u8>,
    ) -> Result<()> {
        let is_working = self.states.get(member_name) == Some(&MemberState::Working);
        self.clear_stale_context_pressure_resume_guard(member_name, output_bytes);
        let threshold = self
            .config
            .team_config
            .workflow_policy
            .context_pressure_threshold;
        let restart_delay_secs = self
            .config
            .team_config
            .workflow_policy
            .context_pressure_restart_delay_secs;
        let inputs = self.context_pressure_inputs(
            member_name,
            output_bytes,
            uptime_secs,
            proactive_usage_pct,
        );
        let (score, actions) = self.context_pressure_tracker.observe_at(
            member_name,
            &inputs,
            is_working,
            restart_delay_secs,
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
                        proactive_context_usage_pct = inputs.proactive_context_usage_pct,
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
                        "health: context pressure warning for {} (score={}/{}, output_bytes={}, proactive_context_usage_pct={:?})",
                        member_name, score, threshold, output_bytes, inputs.proactive_context_usage_pct
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
                        "health: nudged {} to wrap up under context pressure (score={}/{}, proactive_context_usage_pct={:?})",
                        member_name, score, threshold, inputs.proactive_context_usage_pct
                    ));
                }
                ContextPressureAction::Restart => {
                    if !self.shim_handles.contains_key(member_name) {
                        continue;
                    }

                    if self.handle_context_pressure_restart(member_name, task_id, output_bytes)? {
                        self.record_orchestrator_action(format!(
                            "health: restarted {} after sustained context pressure (score={}/{})",
                            member_name, score, threshold
                        ));
                        self.context_pressure_tracker.clear_member(member_name);
                    }
                }
            }
        }

        Ok(())
    }

    fn context_pressure_inputs(
        &mut self,
        member_name: &str,
        output_bytes: u64,
        uptime_secs: u64,
        proactive_usage_pct: Option<u8>,
    ) -> ContextPressureInputs {
        let quality = self
            .current_codex_quality_signals(member_name)
            .unwrap_or_default();
        ContextPressureInputs {
            output_bytes,
            session_uptime_secs: uptime_secs,
            proactive_context_usage_pct: proactive_usage_pct.map(u64::from),
            narration_detected: self.narration_tracker.is_narrating(member_name),
            meta_conversation_detected: self.narration_tracker.is_narrating(member_name),
            assistant_message_count: quality.assistant_message_count,
            tool_call_count: quality.tool_call_count,
            unique_tool_names: quality.unique_tool_names.len(),
            shrinking_responses: quality.shrinking_responses,
            repeated_identical_outputs: quality.repeated_identical_outputs,
            tool_failure_message: quality.tool_failure_message,
            shim_failure_count: self
                .context_pressure_tracker
                .shim_failure_count(member_name),
            secs_since_last_commit: self.secs_since_last_commit(member_name),
        }
    }

    pub(super) fn record_context_pressure_failure(&mut self, member_name: &str) {
        self.context_pressure_tracker.record_failure(member_name);
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

    fn clear_stale_context_pressure_resume_guard(&self, member_name: &str, output_bytes: u64) {
        let Some(member) = self
            .config
            .members
            .iter()
            .find(|member| member.name == member_name)
        else {
            return;
        };
        let work_dir = self.member_work_dir(member);
        let _ = context_management::clear_proactive_restart_context_if_stale(
            &work_dir,
            output_bytes,
            super::CONTEXT_RESTART_COOLDOWN,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backdate_score(tracker: &mut ContextPressureTracker, member: &str, output_bytes: u64) {
        let state = tracker.members.entry(member.to_string()).or_default();
        state.last_output_bytes = output_bytes;
        state.over_threshold_since = Some(Instant::now() - Duration::from_secs(121));
    }

    fn pressure_inputs() -> ContextPressureInputs {
        ContextPressureInputs {
            output_bytes: 450_000,
            session_uptime_secs: 2_000,
            proactive_context_usage_pct: None,
            narration_detected: true,
            meta_conversation_detected: true,
            assistant_message_count: 4,
            tool_call_count: 0,
            unique_tool_names: 0,
            shrinking_responses: true,
            repeated_identical_outputs: false,
            tool_failure_message: None,
            shim_failure_count: 0,
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
            session_uptime_secs: 60,
            proactive_context_usage_pct: None,
            narration_detected: false,
            meta_conversation_detected: false,
            assistant_message_count: 2,
            tool_call_count: 4,
            unique_tool_names: 3,
            shrinking_responses: false,
            repeated_identical_outputs: false,
            tool_failure_message: None,
            shim_failure_count: 0,
            secs_since_last_commit: Some(60),
        };
        assert!(compute_pressure_score(&inputs, 512_000) < 30);
    }

    #[test]
    fn threshold_crossing_detection_warns_at_seventy_percent() {
        let mut tracker = ContextPressureTracker::new(100, 512_000);
        let (score, actions) =
            tracker.observe_at("eng-1", &pressure_inputs(), true, 120, Instant::now());
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
            session_uptime_secs: 120,
            proactive_context_usage_pct: None,
            narration_detected: false,
            meta_conversation_detected: false,
            assistant_message_count: 2,
            tool_call_count: 0,
            unique_tool_names: 0,
            shrinking_responses: false,
            repeated_identical_outputs: false,
            tool_failure_message: None,
            shim_failure_count: 0,
            secs_since_last_commit: Some(120),
        };

        let (score, actions) = tracker.observe_at("eng-1", &inputs, true, 120, Instant::now());
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
        let now = Instant::now();
        assert_eq!(
            tracker.observe_at("eng-1", &inputs, true, 120, now).1,
            vec![ContextPressureAction::Warn, ContextPressureAction::Nudge]
        );
        assert_eq!(
            tracker
                .observe_at("eng-1", &inputs, true, 120, now + Duration::from_secs(121))
                .1,
            vec![ContextPressureAction::Restart]
        );
    }

    #[test]
    fn counter_reset_on_session_restart_rewarns_after_output_drops() {
        let mut tracker = ContextPressureTracker::new(100, 512_000);
        let inputs = pressure_inputs();
        assert_eq!(
            tracker
                .observe_at("eng-1", &inputs, true, 120, Instant::now())
                .1,
            vec![ContextPressureAction::Warn, ContextPressureAction::Nudge]
        );
        let reset_inputs = ContextPressureInputs {
            output_bytes: 100,
            session_uptime_secs: 5,
            proactive_context_usage_pct: None,
            narration_detected: false,
            meta_conversation_detected: false,
            assistant_message_count: 0,
            tool_call_count: 0,
            unique_tool_names: 0,
            shrinking_responses: false,
            repeated_identical_outputs: false,
            tool_failure_message: None,
            shim_failure_count: 0,
            secs_since_last_commit: Some(0),
        };
        assert!(
            tracker
                .observe_at("eng-1", &reset_inputs, true, 120, Instant::now())
                .1
                .is_empty()
        );
        assert_eq!(
            tracker
                .observe_at("eng-1", &inputs, true, 120, Instant::now())
                .1,
            vec![ContextPressureAction::Warn, ContextPressureAction::Nudge]
        );
    }

    #[test]
    fn idle_member_clears_restart_counter() {
        let mut tracker = ContextPressureTracker::new(100, 512_000);
        backdate_score(&mut tracker, "eng-1", 512_000);
        assert!(
            tracker
                .observe_at("eng-1", &pressure_inputs(), false, 120, Instant::now())
                .1
                .is_empty()
        );
        assert!(tracker.members["eng-1"].over_threshold_since.is_none());
    }

    #[test]
    fn repeated_shim_failures_force_restart_after_delay_without_progress() {
        let mut tracker = ContextPressureTracker::new(100, 512_000);
        tracker.record_failure("eng-1");
        tracker.record_failure("eng-1");
        tracker.record_failure("eng-1");

        let inputs = ContextPressureInputs {
            output_bytes: 64_000,
            session_uptime_secs: 180,
            proactive_context_usage_pct: None,
            narration_detected: false,
            meta_conversation_detected: false,
            assistant_message_count: 1,
            tool_call_count: 1,
            unique_tool_names: 1,
            shrinking_responses: false,
            repeated_identical_outputs: false,
            tool_failure_message: None,
            shim_failure_count: tracker.shim_failure_count("eng-1"),
            secs_since_last_commit: Some(180),
        };

        let (score, actions) = tracker.observe_at("eng-1", &inputs, true, 120, Instant::now());
        assert!(
            score < 70,
            "forced restart should not depend on score crossing"
        );
        assert_eq!(actions, vec![ContextPressureAction::Restart]);
    }

    #[test]
    fn long_running_session_with_failures_increases_pressure_score() {
        let inputs = ContextPressureInputs {
            output_bytes: 220_000,
            session_uptime_secs: 1_200,
            proactive_context_usage_pct: None,
            narration_detected: false,
            meta_conversation_detected: false,
            assistant_message_count: 3,
            tool_call_count: 1,
            unique_tool_names: 1,
            shrinking_responses: false,
            repeated_identical_outputs: false,
            tool_failure_message: None,
            shim_failure_count: 3,
            secs_since_last_commit: Some(1_200),
        };

        assert!(compute_pressure_score(&inputs, 512_000) >= 55);
    }

    #[test]
    fn zero_output_threshold_is_ten_minutes() {
        assert_eq!(
            ZERO_OUTPUT_THRESHOLD_SECS, 600,
            "zero-output detection should trigger after 10 minutes"
        );
    }

    #[test]
    fn proactive_warning_enters_warn_then_restart_sequence() {
        let mut tracker = ContextPressureTracker::new(100, 512_000);
        let mut inputs = pressure_inputs();
        inputs.output_bytes = 64_000;
        inputs.proactive_context_usage_pct = Some(80);
        inputs.narration_detected = false;
        inputs.meta_conversation_detected = false;
        inputs.assistant_message_count = 1;
        inputs.tool_call_count = 1;
        inputs.unique_tool_names = 1;
        inputs.shrinking_responses = false;
        let now = Instant::now();

        assert_eq!(
            tracker.observe_at("eng-1", &inputs, true, 20, now).1,
            vec![ContextPressureAction::Warn, ContextPressureAction::Nudge]
        );
        assert_eq!(
            tracker
                .observe_at("eng-1", &inputs, true, 20, now + Duration::from_secs(21))
                .1,
            vec![ContextPressureAction::Restart]
        );
    }

    #[test]
    fn proactive_warning_uses_configured_restart_delay_for_poll_threshold() {
        let mut tracker = ContextPressureTracker::new(100, 512_000);
        let mut inputs = pressure_inputs();
        inputs.output_bytes = 64_000;
        inputs.proactive_context_usage_pct = Some(80);
        inputs.narration_detected = false;
        inputs.meta_conversation_detected = false;
        inputs.assistant_message_count = 1;
        inputs.tool_call_count = 1;
        inputs.unique_tool_names = 1;
        inputs.shrinking_responses = false;
        let now = Instant::now();

        assert_eq!(
            tracker.observe_at("eng-1", &inputs, true, 120, now).1,
            vec![ContextPressureAction::Warn, ContextPressureAction::Nudge]
        );
        for second in [10_u64, 20, 30, 40, 50, 60, 70, 80, 90, 100] {
            assert!(
                tracker
                    .observe_at(
                        "eng-1",
                        &inputs,
                        true,
                        120,
                        now + Duration::from_secs(second)
                    )
                    .1
                    .is_empty()
            );
        }
        assert_eq!(
            tracker
                .observe_at("eng-1", &inputs, true, 120, now + Duration::from_secs(121))
                .1,
            vec![ContextPressureAction::Restart]
        );
    }
}
