//! Narration-loop detection for "working" agents that keep producing text
//! without actually invoking tools or commands.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::warn;

use super::super::*;
use crate::shim::classifier::AgentType;
use crate::team::events::TeamEvent;

const MIN_NARRATION_SPAN: Duration = Duration::from_secs(30);
const RESTART_NARRATION_SPAN: Duration = Duration::from_secs(120);

#[derive(Debug, Clone)]
pub(super) struct NarrationSample {
    timestamp: Instant,
    line_count: usize,
    has_tool_markers: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct NarrationTracker {
    samples: HashMap<String, VecDeque<NarrationSample>>,
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
            window_size: window_size.max(threshold.max(1)),
            threshold: threshold.max(1),
        }
    }

    pub(super) fn clear_member(&mut self, member: &str) {
        self.samples.remove(member);
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

        let sample = NarrationSample {
            timestamp: Instant::now(),
            line_count,
            has_tool_markers,
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

        let Some(first) = window.first() else {
            return false;
        };
        let Some(last) = window.last() else {
            return false;
        };
        last.timestamp.duration_since(first.timestamp) >= MIN_NARRATION_SPAN
    }

    fn narration_duration(&self, member: &str) -> Option<Duration> {
        let samples = self.samples.get(member)?;
        let first = samples.front()?;
        let last = samples.back()?;
        Some(last.timestamp.duration_since(first.timestamp))
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
            self.narration_tracker
                .record_sample(&member_name, line_count, &capture, agent_type);

            if !self.narration_tracker.has_samples(&member_name) {
                self.clear_narration_cooldowns(&member_name);
                continue;
            }

            if !self.narration_tracker.is_narrating(&member_name) {
                continue;
            }

            let nudge_key = Self::narration_nudge_cooldown_key(&member_name);
            if !self.intervention_cooldowns.contains_key(&nudge_key) {
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
                    let message = self.prepend_member_nudge(
                        &member,
                        "You appear to be narrating instead of executing commands. Please use your tools to take action.",
                    );
                    if let Err(error) = self.queue_message("daemon", &member_name, &message) {
                        warn!(member = %member_name, error = %error, "failed to queue narration nudge");
                    }
                }
                self.record_orchestrator_action(format!(
                    "health: detected narration loop for {}",
                    member_name
                ));
                self.intervention_cooldowns
                    .insert(nudge_key, Instant::now());
            }

            if self
                .narration_tracker
                .narration_duration(&member_name)
                .is_some_and(|duration| duration >= RESTART_NARRATION_SPAN)
            {
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
                self.intervention_cooldowns
                    .insert(restart_key, Instant::now());
                self.narration_tracker.clear_member(&member_name);
                self.clear_narration_cooldowns(&member_name);
            }
        }

        Ok(())
    }

    fn handle_narration_restart(&mut self, member_name: &str) -> Result<()> {
        let Some(task) = self.active_task(member_name)? else {
            return Ok(());
        };
        let Some(pane_id) = self.config.pane_map.get(member_name).cloned() else {
            return Ok(());
        };

        warn!(member = %member_name, task_id = task.id, "restarting agent after sustained narration loop");
        crate::tmux::respawn_pane(&pane_id, "bash")?;
        std::thread::sleep(Duration::from_millis(200));

        let assignment = Self::restart_assignment_message(&task);
        let launch = self.launch_task_assignment(member_name, &assignment, Some(task.id), false)?;
        let mut restart_notice = format!(
            "Restarted after a narration loop. Continue task #{} from the current worktree state and execute commands instead of narrating.",
            task.id
        );
        if let Some(branch) = launch.branch.as_deref() {
            restart_notice.push_str(&format!("\nBranch: {branch}"));
        }
        restart_notice.push_str(&format!("\nWorktree: {}", launch.work_dir.display()));
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

    fn clear_narration_cooldowns(&mut self, member_name: &str) {
        self.intervention_cooldowns
            .remove(&Self::narration_nudge_cooldown_key(member_name));
        self.intervention_cooldowns
            .remove(&Self::narration_restart_cooldown_key(member_name));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backdate_member_samples(
        tracker: &mut NarrationTracker,
        member: &str,
        start_secs_ago: u64,
        step_secs: u64,
    ) {
        let now = Instant::now();
        let Some(samples) = tracker.samples.get_mut(member) else {
            return;
        };
        for (index, sample) in samples.iter_mut().enumerate() {
            let secs_ago = start_secs_ago.saturating_sub((index as u64) * step_secs);
            sample.timestamp = now - Duration::from_secs(secs_ago);
        }
    }

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
        backdate_member_samples(&mut tracker, "eng-1", 35, 7);
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
        backdate_member_samples(&mut tracker, "eng-1", 40, 10);
        assert!(!tracker.is_narrating("eng-1"));
    }

    #[test]
    fn narration_requires_growing_output() {
        let mut tracker = NarrationTracker::default();
        for _ in 0..6 {
            tracker.record_sample("eng-1", 4, "narrating", AgentType::Claude);
        }
        backdate_member_samples(&mut tracker, "eng-1", 40, 8);
        assert!(!tracker.is_narrating("eng-1"));
    }

    #[test]
    fn default_tracker_empty() {
        let tracker = NarrationTracker::default();
        assert!(!tracker.is_narrating("eng-1"));
        assert!(!tracker.has_samples("eng-1"));
    }
}
