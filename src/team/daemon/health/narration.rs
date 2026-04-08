//! Narration-loop detection for "working" agents that keep producing text
//! without actually invoking tools or commands.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::warn;

use super::super::*;
use super::format_checkpoint_section;
use crate::shim::classifier::AgentType;
use crate::team::events::TeamEvent;

const DEFAULT_NARRATION_THRESHOLD_POLLS: u32 = 5;

#[derive(Debug, Clone, Copy, Default)]
struct BreakerState {
    last_poll_was_narration: bool,
    narration_polls: u32,
    post_nudge_polls: u32,
    nudged: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct NarrationTracker {
    breaker_states: HashMap<String, BreakerState>,
    detection_enabled: bool,
    threshold_polls: u32,
}

impl Default for NarrationTracker {
    fn default() -> Self {
        Self::new(true, DEFAULT_NARRATION_THRESHOLD_POLLS)
    }
}

impl NarrationTracker {
    pub(crate) fn new(detection_enabled: bool, threshold_polls: u32) -> Self {
        Self {
            breaker_states: HashMap::new(),
            detection_enabled,
            threshold_polls: threshold_polls.max(1),
        }
    }

    pub(super) fn clear_member(&mut self, member: &str) {
        self.breaker_states.remove(member);
    }

    pub(super) fn has_samples(&self, member: &str) -> bool {
        self.breaker_states.contains_key(member)
    }

    pub(super) fn record_sample(&mut self, member: &str, content: &str, agent_type: AgentType) {
        let narrating = self.detection_enabled
            && crate::shim::classifier::detect_narration_pattern(content, agent_type);
        let state = self.breaker_states.entry(member.to_string()).or_default();
        state.last_poll_was_narration = narrating;
    }

    pub(super) fn is_narrating(&self, member: &str) -> bool {
        self.breaker_states
            .get(member)
            .is_some_and(|state| state.last_poll_was_narration)
    }

    pub(super) fn narration_ratio(&self, member: &str) -> f64 {
        if self.is_narrating(member) { 1.0 } else { 0.0 }
    }

    pub(super) fn note_breach(&mut self, member: &str, narrating: bool) -> BreakerState {
        let state = self.breaker_states.entry(member.to_string()).or_default();
        if narrating {
            state.narration_polls = state.narration_polls.saturating_add(1);
            if state.nudged {
                state.post_nudge_polls = state.post_nudge_polls.saturating_add(1);
            }
        } else {
            *state = BreakerState::default();
        }
        *state
    }

    pub(super) fn should_nudge(&self, member: &str) -> bool {
        self.breaker_states
            .get(member)
            .is_some_and(|state| !state.nudged && state.narration_polls >= self.threshold_polls)
    }

    pub(super) fn note_nudge(&mut self, member: &str) {
        if let Some(state) = self.breaker_states.get_mut(member) {
            state.nudged = true;
            state.post_nudge_polls = 0;
        }
    }

    pub(super) fn should_restart(&self, member: &str) -> bool {
        self.breaker_states
            .get(member)
            .is_some_and(|state| state.nudged && state.post_nudge_polls >= self.threshold_polls)
    }
}

impl TeamDaemon {
    pub(in super::super) fn check_narration_loops(&mut self) -> Result<()> {
        // Narration detection disabled — it was killing agents that were
        // actively coding (319 uncommitted lines) because they described
        // their approach in the PTY output alongside real code changes.
        // The detector can't distinguish "planning then executing" from
        // "narrating without executing." Re-enable when the detector
        // checks for actual worktree changes before flagging narration.
        return Ok(());

        #[allow(unreachable_code)]
        let member_names: Vec<String> = self
            .config
            .members
            .iter()
            .map(|member| member.name.clone())
            .collect();

        for member_name in member_names {
            if self.states.get(&member_name) != Some(&MemberState::Working) {
                self.narration_tracker.clear_member(&member_name);
                continue;
            }

            let Some(pane_id) = self.config.pane_map.get(&member_name) else {
                continue;
            };
            let capture = match crate::tmux::capture_pane(pane_id) {
                Ok(capture) => capture,
                Err(_) => continue,
            };

            let agent_type = self.member_agent_type(&member_name);
            self.narration_tracker
                .record_sample(&member_name, &capture, agent_type);

            if !self.narration_tracker.has_samples(&member_name) {
                continue;
            }

            let is_narrating = self.narration_tracker.is_narrating(&member_name);
            let breaker_state = self
                .narration_tracker
                .note_breach(&member_name, is_narrating);
            if !is_narrating {
                continue;
            }

            let task_id = self.active_task_id(&member_name);
            let ratio = self.narration_tracker.narration_ratio(&member_name);
            if breaker_state.narration_polls == self.narration_tracker.threshold_polls {
                warn!(member = %member_name, task_id, ratio, "detected narration loop");
                self.emit_event(TeamEvent::narration_detected(&member_name, task_id));
            }

            if self.narration_tracker.should_nudge(&member_name) {
                if let Some(member) = self
                    .config
                    .members
                    .iter()
                    .find(|member| member.name == member_name)
                    .cloned()
                {
                    let message = self.meta_conversation_nudge_message(&member_name, &member);
                    if let Err(error) = self.queue_message("daemon", &member_name, &message) {
                        warn!(
                            member = %member_name,
                            error = %error,
                            "failed to queue narration nudge"
                        );
                    }
                }
                self.emit_event(TeamEvent::narration_nudged(&member_name, task_id));
                self.record_orchestrator_action(format!(
                    "health: nudged {} after narration detection {:.2}",
                    member_name, ratio
                ));
                self.narration_tracker.note_nudge(&member_name);
            }

            if self.narration_tracker.should_restart(&member_name) {
                let restart_key = Self::narration_restart_cooldown_key(&member_name);
                let on_cooldown = self
                    .intervention_cooldowns
                    .get(&restart_key)
                    .is_some_and(|last| last.elapsed() < super::CONTEXT_RESTART_COOLDOWN);
                if on_cooldown {
                    continue;
                }

                if self.active_task_id(&member_name).is_some() {
                    self.handle_narration_restart(&member_name)?;
                } else {
                    self.restart_member(&member_name)?;
                }
                self.emit_event(TeamEvent::narration_restart(&member_name, task_id));
                self.intervention_cooldowns
                    .insert(restart_key, Instant::now());
                self.narration_tracker.clear_member(&member_name);
            }
        }

        Ok(())
    }

    fn handle_narration_restart(&mut self, member_name: &str) -> Result<()> {
        let Some(task) = self.active_task(member_name)? else {
            return Ok(());
        };
        let Some(member) = self
            .config
            .members
            .iter()
            .find(|member| member.name == member_name)
            .cloned()
        else {
            return Ok(());
        };
        let Some(pane_id) = self.config.pane_map.get(member_name).cloned() else {
            return Ok(());
        };
        let work_dir = self.member_work_dir(&member);

        warn!(member = %member_name, task_id = task.id, "restarting agent after sustained narration loop");
        self.preserve_worktree_before_restart(member_name, &work_dir, "narration loop");
        self.preserve_restart_context(member_name, &task, Some(&pane_id), &work_dir, "narration");

        crate::tmux::respawn_pane(&pane_id, "bash")?;
        std::thread::sleep(Duration::from_millis(200));

        let assignment = self.restart_assignment_with_handoff(member_name, &task, &work_dir);
        let launch = self.launch_task_assignment(member_name, &assignment, Some(task.id), false)?;
        let mut restart_notice = format!(
            "Restarted after a narration loop. Continue task #{} from the current worktree state and execute commands instead of narrating.",
            task.id
        );
        if let Some(branch) = launch.branch.as_deref() {
            restart_notice.push_str(&format!("\nBranch: {branch}"));
        }
        restart_notice.push_str(&format!("\nWorktree: {}", launch.work_dir.display()));
        if let Some(cp_content) =
            super::super::super::checkpoint::read_checkpoint(&self.config.project_root, member_name)
        {
            restart_notice.push_str(&format_checkpoint_section(&cp_content));
        }
        if let Err(error) = self.queue_message("daemon", member_name, &restart_notice) {
            warn!(member = %member_name, error = %error, "failed to inject narration restart notice");
        }
        self.record_agent_restarted(member_name, task.id.to_string(), "narration", 1);
        self.record_orchestrator_action(format!(
            "health: restarted {} on task #{} after narration loop",
            member_name, task.id
        ));
        Ok(())
    }

    fn member_agent_type(&self, member_name: &str) -> AgentType {
        let agent_name = self
            .config
            .members
            .iter()
            .find(|member| member.name == member_name)
            .and_then(|member| member.agent.as_deref())
            .unwrap_or("claude");
        match agent_name {
            "claude" | "claude-code" => AgentType::Claude,
            "codex" | "codex-cli" => AgentType::Codex,
            "kiro" | "kiro-cli" => AgentType::Kiro,
            _ => AgentType::Generic,
        }
    }

    fn narration_restart_cooldown_key(member_name: &str) -> String {
        format!("narration-restart::{member_name}")
    }

    fn meta_conversation_nudge_message(
        &self,
        member_name: &str,
        member: &MemberInstance,
    ) -> String {
        let task_context = self
            .active_task(member_name)
            .ok()
            .flatten()
            .map(|task| format!("Task #{}: {}", task.id, task.title))
            .unwrap_or_else(|| "Continue the current assignment.".to_string());
        self.prepend_member_nudge(
            member,
            &format!("Stop narrating. Run the command now.\n{task_context}"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_detect_narration() {
        let mut tracker = NarrationTracker::default();
        let content = "I will explain the next step.";
        tracker.record_sample("eng-1", &content, AgentType::Claude);
        for _ in 0..5 {
            tracker.note_breach("eng-1", tracker.is_narrating("eng-1"));
        }
        assert!(tracker.is_narrating("eng-1"));
        assert!(tracker.should_nudge("eng-1"));
    }

    #[test]
    fn narration_clears_on_tool_use() {
        let mut tracker = NarrationTracker::default();
        let narrating = "I will explain step one.";
        tracker.record_sample("eng-1", &narrating, AgentType::Claude);
        assert!(tracker.is_narrating("eng-1"));

        let with_tools = format!("{narrating}\n⏺ Bash(cargo test)");
        tracker.record_sample("eng-1", &with_tools, AgentType::Claude);
        assert!(tracker.has_samples("eng-1"));
        assert_eq!(tracker.narration_ratio("eng-1"), 0.0);
        assert!(!tracker.is_narrating("eng-1"));
    }

    #[test]
    fn narration_clears_on_idle() {
        let mut tracker = NarrationTracker::default();
        tracker.record_sample("eng-1", "I will keep narrating.", AgentType::Claude);
        tracker.clear_member("eng-1");
        assert!(!tracker.has_samples("eng-1"));
        assert!(!tracker.is_narrating("eng-1"));
    }

    #[test]
    fn narration_not_triggered_below_threshold() {
        let mut tracker = NarrationTracker::default();
        tracker.record_sample("eng-1", "src/team/daemon.rs", AgentType::Claude);
        assert!(!tracker.is_narrating("eng-1"));
    }

    #[test]
    fn restart_triggers_after_sustained_narration_post_nudge() {
        let mut tracker = NarrationTracker::default();
        let content = "I should inspect the daemon.";
        tracker.record_sample("eng-1", &content, AgentType::Codex);

        for _ in 0..5 {
            tracker.note_breach("eng-1", tracker.is_narrating("eng-1"));
        }
        assert!(tracker.should_nudge("eng-1"));
        tracker.note_nudge("eng-1");

        for _ in 0..5 {
            tracker.note_breach("eng-1", true);
        }
        assert!(tracker.should_restart("eng-1"));
    }

    #[test]
    fn narration_breaker_resets_after_progress() {
        let mut tracker = NarrationTracker::default();
        let narrating = "I should inspect the daemon.";
        tracker.record_sample("eng-1", &narrating, AgentType::Codex);
        for _ in 0..5 {
            tracker.note_breach("eng-1", true);
        }
        tracker.note_nudge("eng-1");

        let with_tools = format!("{narrating}\n$ cargo test");
        tracker.record_sample("eng-1", &with_tools, AgentType::Codex);
        tracker.note_breach("eng-1", tracker.is_narrating("eng-1"));
        assert!(!tracker.is_narrating("eng-1"));
        assert!(!tracker.should_nudge("eng-1"));
        assert!(!tracker.should_restart("eng-1"));
    }

    #[test]
    fn default_tracker_empty() {
        let tracker = NarrationTracker::default();
        assert!(!tracker.is_narrating("eng-1"));
        assert!(!tracker.has_samples("eng-1"));
    }
}
