//! Narration-loop detection for "working" agents that keep producing text
//! without actually invoking tools or commands.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::warn;

use super::super::*;
use super::format_checkpoint_section;
use crate::shim::classifier::{AgentType, NarrationLineKind};
use crate::team::events::TeamEvent;

const DEFAULT_NARRATION_WINDOW_LINES: usize = 50;
const DEFAULT_NARRATION_THRESHOLD: f64 = 0.8;
const DEFAULT_NARRATION_NUDGE_MAX: u32 = 2;
const NARRATION_CONSECUTIVE_CHECKS: usize = 3;

#[derive(Debug, Clone, Default)]
struct NarrationWindow {
    lines: VecDeque<NarrationLineKind>,
    explanation_lines: usize,
    tool_lines: usize,
}

impl NarrationWindow {
    fn push(&mut self, kind: NarrationLineKind, window_size: usize) {
        if matches!(kind, NarrationLineKind::Other) {
            return;
        }

        self.lines.push_back(kind);
        match kind {
            NarrationLineKind::Explanation => self.explanation_lines += 1,
            NarrationLineKind::ToolOrCommand => self.tool_lines += 1,
            NarrationLineKind::Other => {}
        }

        while self.lines.len() > window_size {
            if let Some(evicted) = self.lines.pop_front() {
                match evicted {
                    NarrationLineKind::Explanation => {
                        self.explanation_lines = self.explanation_lines.saturating_sub(1);
                    }
                    NarrationLineKind::ToolOrCommand => {
                        self.tool_lines = self.tool_lines.saturating_sub(1);
                    }
                    NarrationLineKind::Other => {}
                }
            }
        }
    }

    fn explanation_ratio(&self) -> f64 {
        let classified = self.explanation_lines + self.tool_lines;
        if classified == 0 {
            return 0.0;
        }
        self.explanation_lines as f64 / classified as f64
    }
}

#[derive(Debug, Clone, Copy)]
struct BreakerState {
    consecutive_breaches: usize,
    nudges_sent: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct NarrationTracker {
    windows: HashMap<String, NarrationWindow>,
    last_content: HashMap<String, String>,
    breaker_states: HashMap<String, BreakerState>,
    window_size: usize,
    threshold: f64,
    nudge_max: u32,
}

impl Default for NarrationTracker {
    fn default() -> Self {
        Self::new(
            DEFAULT_NARRATION_WINDOW_LINES,
            DEFAULT_NARRATION_THRESHOLD,
            DEFAULT_NARRATION_NUDGE_MAX,
        )
    }
}

impl NarrationTracker {
    pub(crate) fn new(window_size: usize, threshold: f64, nudge_max: u32) -> Self {
        Self {
            windows: HashMap::new(),
            last_content: HashMap::new(),
            breaker_states: HashMap::new(),
            window_size: window_size.max(1),
            threshold: threshold.clamp(0.0, 1.0),
            nudge_max,
        }
    }

    pub(super) fn clear_member(&mut self, member: &str) {
        self.windows.remove(member);
        self.last_content.remove(member);
        self.breaker_states.remove(member);
    }

    pub(super) fn has_samples(&self, member: &str) -> bool {
        self.windows
            .get(member)
            .is_some_and(|window| !window.lines.is_empty())
    }

    pub(super) fn record_sample(&mut self, member: &str, content: &str, agent_type: AgentType) {
        let window = self.windows.entry(member.to_string()).or_default();
        let previous = self.last_content.get(member).cloned().unwrap_or_default();
        let appended = appended_lines(&previous, content);

        if appended.is_empty() && previous == content {
            return;
        }

        if content.lines().count() < previous.lines().count() || previous.len() > content.len() {
            window.lines.clear();
            window.explanation_lines = 0;
            window.tool_lines = 0;
        }

        for line in appended {
            let kind = crate::shim::classifier::classify_narration_line(line, agent_type);
            window.push(kind, self.window_size);
        }

        self.last_content
            .insert(member.to_string(), content.to_string());
    }

    pub(super) fn is_narrating(&self, member: &str) -> bool {
        let Some(window) = self.windows.get(member) else {
            return false;
        };
        if window.lines.len() < self.window_size {
            return false;
        }
        window.explanation_ratio() > self.threshold
    }

    pub(super) fn narration_ratio(&self, member: &str) -> f64 {
        self.windows
            .get(member)
            .map(NarrationWindow::explanation_ratio)
            .unwrap_or(0.0)
    }

    pub(super) fn note_breach(&mut self, member: &str, narrating: bool) -> BreakerState {
        let state = self
            .breaker_states
            .entry(member.to_string())
            .or_insert(BreakerState {
                consecutive_breaches: 0,
                nudges_sent: 0,
            });
        if narrating {
            state.consecutive_breaches = state.consecutive_breaches.saturating_add(1);
        } else {
            state.consecutive_breaches = 0;
            state.nudges_sent = 0;
        }
        *state
    }

    pub(super) fn should_nudge(&self, member: &str) -> bool {
        self.breaker_states.get(member).is_some_and(|state| {
            state.consecutive_breaches >= NARRATION_CONSECUTIVE_CHECKS
                && state.nudges_sent < self.nudge_max
        })
    }

    pub(super) fn note_nudge(&mut self, member: &str) {
        if let Some(state) = self.breaker_states.get_mut(member) {
            state.nudges_sent = state.nudges_sent.saturating_add(1);
            state.consecutive_breaches = 0;
        }
    }

    pub(super) fn should_restart(&self, member: &str) -> bool {
        self.breaker_states.get(member).is_some_and(|state| {
            state.consecutive_breaches >= NARRATION_CONSECUTIVE_CHECKS
                && state.nudges_sent >= self.nudge_max
        })
    }
}

pub(super) fn has_tool_markers(content: &str, agent_type: AgentType) -> bool {
    content.lines().any(|line| {
        matches!(
            crate::shim::classifier::classify_narration_line(line, agent_type),
            NarrationLineKind::ToolOrCommand
        )
    })
}

fn appended_lines<'a>(previous: &'a str, current: &'a str) -> Vec<&'a str> {
    let current_lines: Vec<&str> = current.lines().collect();
    if previous.is_empty() {
        return current_lines;
    }

    let previous_lines: Vec<&str> = previous.lines().collect();
    let shared = previous_lines
        .iter()
        .zip(current_lines.iter())
        .take_while(|(left, right)| left == right)
        .count();
    current_lines.into_iter().skip(shared).collect()
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
            if breaker_state.consecutive_breaches == NARRATION_CONSECUTIVE_CHECKS {
                warn!(member = %member_name, task_id, ratio, "detected narration loop");
                self.emit_event(TeamEvent::narration_loop_detected(&member_name, task_id));
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
                self.emit_event(TeamEvent::meta_conversation_nudged(&member_name, task_id));
                self.record_orchestrator_action(format!(
                    "health: nudged {} after narration ratio {:.2}",
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
                self.emit_event(TeamEvent::meta_conversation_escalated(
                    &member_name,
                    task_id,
                ));
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
        self.preserve_restart_context(
            member_name,
            &task,
            Some(&pane_id),
            &work_dir,
            "narration",
        );

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
            &format!("Stop explaining. Run the command now.\n{task_context}"),
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
        let content = (0..50)
            .map(|idx| format!("I will explain step {idx}."))
            .collect::<Vec<_>>()
            .join("\n");
        tracker.record_sample("eng-1", &content, AgentType::Claude);
        for _ in 0..3 {
            tracker.note_breach("eng-1", tracker.is_narrating("eng-1"));
        }
        assert!(tracker.is_narrating("eng-1"));
        assert!(tracker.should_nudge("eng-1"));
    }

    #[test]
    fn narration_clears_on_tool_use() {
        let mut tracker = NarrationTracker::default();
        let narrating = (0..50)
            .map(|idx| format!("I will explain step {idx}."))
            .collect::<Vec<_>>()
            .join("\n");
        tracker.record_sample("eng-1", &narrating, AgentType::Claude);
        assert!(tracker.is_narrating("eng-1"));

        let with_tools = format!(
            "{narrating}\n{}",
            (0..20)
                .map(|_| "⏺ Bash(cargo test)")
                .collect::<Vec<_>>()
                .join("\n")
        );
        tracker.record_sample("eng-1", &with_tools, AgentType::Claude);
        assert!(tracker.has_samples("eng-1"));
        assert!(tracker.narration_ratio("eng-1") < 0.8);
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
        let content = (0..40)
            .map(|idx| format!("I will explain step {idx}."))
            .collect::<Vec<_>>()
            .join("\n");
        tracker.record_sample("eng-1", &content, AgentType::Claude);
        assert!(!tracker.is_narrating("eng-1"));
    }

    #[test]
    fn restart_triggers_after_two_failed_nudges() {
        let mut tracker = NarrationTracker::default();
        let content = (0..50)
            .map(|idx| format!("I should inspect thing {idx}."))
            .collect::<Vec<_>>()
            .join("\n");
        tracker.record_sample("eng-1", &content, AgentType::Codex);

        for _ in 0..3 {
            tracker.note_breach("eng-1", tracker.is_narrating("eng-1"));
        }
        assert!(tracker.should_nudge("eng-1"));
        tracker.note_nudge("eng-1");

        for _ in 0..3 {
            tracker.note_breach("eng-1", true);
        }
        assert!(tracker.should_nudge("eng-1"));
        tracker.note_nudge("eng-1");

        for _ in 0..3 {
            tracker.note_breach("eng-1", true);
        }
        assert!(tracker.should_restart("eng-1"));
    }

    #[test]
    fn narration_breaker_resets_after_progress() {
        let mut tracker = NarrationTracker::default();
        let narrating = (0..50)
            .map(|idx| format!("I should inspect thing {idx}."))
            .collect::<Vec<_>>()
            .join("\n");
        tracker.record_sample("eng-1", &narrating, AgentType::Codex);
        for _ in 0..3 {
            tracker.note_breach("eng-1", true);
        }
        tracker.note_nudge("eng-1");

        let with_tools = format!(
            "{narrating}\n{}",
            (0..25)
                .map(|_| "$ cargo test")
                .collect::<Vec<_>>()
                .join("\n")
        );
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
