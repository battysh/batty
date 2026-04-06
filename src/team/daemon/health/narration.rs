//! Narration-loop detection for "working" agents that keep producing text
//! without actually invoking tools or commands.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::warn;

use super::super::*;
use super::format_checkpoint_section;
use crate::shim::classifier::AgentType;
use crate::team::events::TeamEvent;

const META_CONVERSATION_THRESHOLD: usize = 3;
const META_CONVERSATION_GRACE_CYCLES: usize = 2;

#[derive(Debug, Clone)]
pub(super) struct NarrationSample {
    line_count: usize,
    has_tool_markers: bool,
    looks_meta_conversation: bool,
}

#[derive(Debug, Clone, Copy)]
struct BreakerState {
    sample_len_at_nudge: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct NarrationTracker {
    samples: HashMap<String, VecDeque<NarrationSample>>,
    breaker_states: HashMap<String, BreakerState>,
    window_size: usize,
    threshold: usize,
}

impl Default for NarrationTracker {
    fn default() -> Self {
        Self::new(12, 6)
    }
}

impl NarrationTracker {
    pub(crate) fn new(window_size: usize, threshold: usize) -> Self {
        Self {
            samples: HashMap::new(),
            breaker_states: HashMap::new(),
            window_size: window_size.max(threshold.max(1)),
            threshold: threshold.max(1),
        }
    }

    pub(super) fn clear_member(&mut self, member: &str) {
        self.samples.remove(member);
        self.breaker_states.remove(member);
    }

    pub(super) fn has_samples(&self, member: &str) -> bool {
        self.samples
            .get(member)
            .is_some_and(|samples| !samples.is_empty())
    }

    pub(super) fn record_sample(
        &mut self,
        member: &str,
        line_count: usize,
        content: &str,
        agent_type: AgentType,
    ) {
        let has_tool_markers = has_tool_markers(content, agent_type);
        if has_tool_markers {
            self.clear_member(member);
            return;
        }
        let looks_meta_conversation =
            crate::shim::classifier::detect_meta_conversation(content, agent_type);

        let sample = NarrationSample {
            line_count,
            has_tool_markers,
            looks_meta_conversation,
        };

        let samples = self.samples.entry(member.to_string()).or_default();
        if samples
            .back()
            .is_some_and(|previous| sample.line_count <= previous.line_count)
        {
            samples.clear();
        }
        samples.push_back(sample);
        while samples.len() > self.window_size {
            samples.pop_front();
        }
    }

    pub(super) fn is_narrating(&self, member: &str) -> bool {
        let Some(samples) = self.samples.get(member) else {
            return false;
        };
        if samples.len() < self.threshold {
            return false;
        }

        let start = samples.len() - self.threshold;
        let window: Vec<&NarrationSample> = samples.iter().skip(start).collect();
        if window.len() < self.threshold {
            return false;
        }

        if window.iter().any(|sample| sample.has_tool_markers) {
            return false;
        }

        if window
            .windows(2)
            .any(|pair| pair[1].line_count <= pair[0].line_count)
        {
            return false;
        }

        true
    }

    pub(super) fn is_meta_conversation(&self, member: &str) -> bool {
        let Some(samples) = self.samples.get(member) else {
            return false;
        };
        if samples.len() < META_CONVERSATION_THRESHOLD {
            return false;
        }

        let start = samples.len() - META_CONVERSATION_THRESHOLD;
        let window: Vec<&NarrationSample> = samples.iter().skip(start).collect();
        window.len() == META_CONVERSATION_THRESHOLD
            && window.iter().all(|sample| sample.looks_meta_conversation)
            && window
                .windows(2)
                .all(|pair| pair[1].line_count > pair[0].line_count)
    }

    pub(super) fn has_active_breaker(&self, member: &str) -> bool {
        self.breaker_states.contains_key(member)
    }

    pub(super) fn note_breaker_nudge(&mut self, member: &str) {
        let sample_len_at_nudge = self
            .samples
            .get(member)
            .map(|samples| samples.len())
            .unwrap_or(0);
        self.breaker_states.insert(
            member.to_string(),
            BreakerState {
                sample_len_at_nudge,
            },
        );
    }

    pub(super) fn clear_breaker(&mut self, member: &str) {
        self.breaker_states.remove(member);
    }

    pub(super) fn should_escalate_breaker(&self, member: &str) -> bool {
        let Some(state) = self.breaker_states.get(member) else {
            return false;
        };
        let Some(samples) = self.samples.get(member) else {
            return false;
        };
        self.is_meta_conversation(member)
            && samples.len()
                >= state
                    .sample_len_at_nudge
                    .saturating_add(META_CONVERSATION_GRACE_CYCLES)
    }
}

pub(super) fn has_tool_markers(content: &str, agent_type: AgentType) -> bool {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return false;
    }

    let common_markers = [
        "*** Begin Patch",
        "*** Update File:",
        "*** Add File:",
        "*** Delete File:",
        "$ ",
        "\n$ ",
        "\n> ",
        "Exit code:",
    ];
    if common_markers.iter().any(|marker| trimmed.contains(marker)) {
        return true;
    }

    match agent_type {
        AgentType::Claude => {
            let claude_markers = [
                "Read(",
                "Edit(",
                "Bash(",
                "Write(",
                "Grep(",
                "Glob(",
                "MultiEdit(",
                "⎿",
            ];
            claude_markers.iter().any(|marker| trimmed.contains(marker))
        }
        AgentType::Codex => {
            let codex_markers = ["$ ", "\n$ ", "apply_patch", "*** Begin Patch", "target/"];
            codex_markers.iter().any(|marker| trimmed.contains(marker))
        }
        AgentType::Kiro | AgentType::Generic => trimmed.contains("$ ") || trimmed.contains("\n> "),
    }
}

impl TeamDaemon {
    pub(in super::super) fn check_narration_loops(&mut self) -> Result<()> {
        let member_names: Vec<String> = self
            .config
            .members
            .iter()
            .map(|member| member.name.clone())
            .collect();

        for member_name in member_names {
            if self.states.get(&member_name) != Some(&MemberState::Working) {
                self.narration_tracker.clear_member(&member_name);
                self.clear_narration_cooldowns(&member_name);
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
            let line_count = capture.lines().count();
            let had_active_breaker = self.narration_tracker.has_active_breaker(&member_name);
            self.narration_tracker
                .record_sample(&member_name, line_count, &capture, agent_type);

            if !self.narration_tracker.has_samples(&member_name) {
                if had_active_breaker {
                    self.emit_event(TeamEvent::meta_conversation_recovered(
                        &member_name,
                        self.active_task_id(&member_name),
                    ));
                }
                self.clear_narration_cooldowns(&member_name);
                continue;
            }

            let is_narrating = self.narration_tracker.is_narrating(&member_name);
            let is_meta_conversation = self.narration_tracker.is_meta_conversation(&member_name);
            if had_active_breaker && !is_meta_conversation {
                self.emit_event(TeamEvent::meta_conversation_recovered(
                    &member_name,
                    self.active_task_id(&member_name),
                ));
                self.narration_tracker.clear_breaker(&member_name);
                self.clear_narration_cooldowns(&member_name);
            }

            if !is_narrating && !is_meta_conversation {
                continue;
            }

            let nudge_key = Self::narration_nudge_cooldown_key(&member_name);
            if is_meta_conversation && !self.intervention_cooldowns.contains_key(&nudge_key) {
                let task_id = self.active_task_id(&member_name);
                warn!(member = %member_name, task_id, "detected narration loop");
                self.emit_event(TeamEvent::narration_detected(&member_name, task_id));
                if let Some(member) = self
                    .config
                    .members
                    .iter()
                    .find(|member| member.name == member_name)
                    .cloned()
                {
                    let message = self.meta_conversation_nudge_message(&member_name, &member);
                    if let Err(error) = self.queue_message("daemon", &member_name, &message) {
                        warn!(member = %member_name, error = %error, "failed to queue narration nudge");
                    }
                }
                self.emit_event(TeamEvent::meta_conversation_nudged(&member_name, task_id));
                self.record_orchestrator_action(format!(
                    "health: nudged {} to break meta-conversation loop",
                    member_name
                ));
                self.narration_tracker.note_breaker_nudge(&member_name);
                self.intervention_cooldowns
                    .insert(nudge_key, Instant::now());
            }

            if self.narration_tracker.should_escalate_breaker(&member_name) {
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
                self.emit_event(TeamEvent::meta_conversation_escalated(
                    &member_name,
                    self.active_task_id(&member_name),
                ));
                self.intervention_cooldowns
                    .insert(restart_key, Instant::now());
                self.narration_tracker.clear_member(&member_name);
                self.clear_narration_nudge_cooldown(&member_name);
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
        if self
            .config
            .team_config
            .workflow_policy
            .context_handoff_enabled
        {
            let recent_output = self.capture_context_handoff_output(&pane_id);
            if let Err(error) =
                crate::shim::runtime::preserve_handoff(&work_dir, &task, recent_output.as_deref())
            {
                warn!(
                    member = %member_name,
                    task_id = task.id,
                    error = %error,
                    "failed to preserve narration restart handoff"
                );
            }
        }

        let checkpoint = super::super::super::checkpoint::gather_checkpoint(
            &self.config.project_root,
            member_name,
            &task,
        );
        if let Err(error) = super::super::super::checkpoint::write_checkpoint(
            &self.config.project_root,
            &checkpoint,
        ) {
            warn!(
                member = %member_name,
                error = %error,
                "failed to write narration restart checkpoint"
            );
        }

        crate::tmux::respawn_pane(&pane_id, "bash")?;
        std::thread::sleep(Duration::from_millis(200));

        let assignment = self.restart_assignment_with_handoff(&task, &work_dir);
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

    fn narration_nudge_cooldown_key(member_name: &str) -> String {
        format!("narration-nudge::{member_name}")
    }

    fn narration_restart_cooldown_key(member_name: &str) -> String {
        format!("narration-restart::{member_name}")
    }

    fn clear_narration_nudge_cooldown(&mut self, member_name: &str) {
        self.intervention_cooldowns
            .remove(&Self::narration_nudge_cooldown_key(member_name));
    }

    fn clear_narration_cooldowns(&mut self, member_name: &str) {
        self.clear_narration_nudge_cooldown(member_name);
        self.intervention_cooldowns
            .remove(&Self::narration_restart_cooldown_key(member_name));
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
            &format!("STOP NARRATING. Execute the next concrete step now.\n{task_context}"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_tool_markers_claude_read() {
        assert!(has_tool_markers("⏺ Read(src/main.rs)", AgentType::Claude));
    }

    #[test]
    fn has_tool_markers_claude_bash() {
        assert!(has_tool_markers("⏺ Bash(cargo test)", AgentType::Claude));
    }

    #[test]
    fn has_tool_markers_codex_shell() {
        assert!(has_tool_markers("$ cargo test", AgentType::Codex));
    }

    #[test]
    fn has_tool_markers_no_markers() {
        assert!(!has_tool_markers(
            "I will inspect the issue and then explain the next step.",
            AgentType::Claude
        ));
    }

    #[test]
    fn record_and_detect_narration() {
        let mut tracker = NarrationTracker::default();
        for line_count in 1..=6 {
            tracker.record_sample(
                "eng-1",
                line_count,
                "narrating without tools",
                AgentType::Claude,
            );
        }
        assert!(tracker.is_narrating("eng-1"));
    }

    #[test]
    fn narration_clears_on_tool_use() {
        let mut tracker = NarrationTracker::default();
        for line_count in 1..=5 {
            tracker.record_sample("eng-1", line_count, "still narrating", AgentType::Claude);
        }
        tracker.record_sample("eng-1", 6, "⏺ Bash(cargo test)", AgentType::Claude);
        assert!(!tracker.is_narrating("eng-1"));
        assert!(!tracker.has_samples("eng-1"));
    }

    #[test]
    fn narration_clears_on_idle() {
        let mut tracker = NarrationTracker::default();
        tracker.record_sample("eng-1", 1, "narrating", AgentType::Claude);
        tracker.clear_member("eng-1");
        assert!(!tracker.has_samples("eng-1"));
        assert!(!tracker.is_narrating("eng-1"));
    }

    #[test]
    fn narration_not_triggered_below_threshold() {
        let mut tracker = NarrationTracker::default();
        for line_count in 1..=3 {
            tracker.record_sample("eng-1", line_count, "narrating", AgentType::Claude);
        }
        assert!(!tracker.is_narrating("eng-1"));
    }

    #[test]
    fn meta_conversation_detected_within_three_cycles() {
        let mut tracker = NarrationTracker::default();
        for line_count in 1..=3 {
            tracker.record_sample(
                "eng-1",
                line_count,
                "I should inspect the issue.\nNext step: I will check the daemon.\nShould I patch narration first?",
                AgentType::Codex,
            );
        }
        assert!(tracker.is_meta_conversation("eng-1"));
    }

    #[test]
    fn meta_conversation_false_positive_avoids_tool_output() {
        let mut tracker = NarrationTracker::default();
        for line_count in 1..=3 {
            tracker.record_sample(
                "eng-1",
                line_count,
                "$ rg -n narration src/team\nExit code: 0",
                AgentType::Codex,
            );
        }
        assert!(!tracker.is_meta_conversation("eng-1"));
    }

    #[test]
    fn breaker_escalates_after_two_more_cycles() {
        let mut tracker = NarrationTracker::default();
        for line_count in 1..=3 {
            tracker.record_sample(
                "eng-1",
                line_count,
                "I should inspect the issue.\nNext step: I will check the daemon.\nShould I patch narration first?",
                AgentType::Codex,
            );
        }
        tracker.note_breaker_nudge("eng-1");
        tracker.record_sample(
            "eng-1",
            4,
            "Maybe I should inspect more state first.\nI will think through the next step.",
            AgentType::Codex,
        );
        assert!(!tracker.should_escalate_breaker("eng-1"));
        tracker.record_sample(
            "eng-1",
            5,
            "Perhaps I should plan a bit more.\nNext step: I will keep reasoning.",
            AgentType::Codex,
        );
        assert!(tracker.should_escalate_breaker("eng-1"));
    }

    #[test]
    fn breaker_clears_after_tool_execution() {
        let mut tracker = NarrationTracker::default();
        for line_count in 1..=3 {
            tracker.record_sample(
                "eng-1",
                line_count,
                "I should inspect the issue.\nNext step: I will check the daemon.\nShould I patch narration first?",
                AgentType::Codex,
            );
        }
        tracker.note_breaker_nudge("eng-1");
        tracker.record_sample(
            "eng-1",
            4,
            "$ sed -n '1,40p' src/team/daemon.rs",
            AgentType::Codex,
        );
        assert!(!tracker.has_active_breaker("eng-1"));
        assert!(!tracker.has_samples("eng-1"));
    }

    #[test]
    fn narration_requires_growing_output() {
        let mut tracker = NarrationTracker::default();
        for _ in 0..6 {
            tracker.record_sample("eng-1", 4, "narrating", AgentType::Claude);
        }
        assert!(!tracker.is_narrating("eng-1"));
    }

    #[test]
    fn default_tracker_empty() {
        let tracker = NarrationTracker::default();
        assert!(!tracker.is_narrating("eng-1"));
        assert!(!tracker.has_samples("eng-1"));
    }
}
