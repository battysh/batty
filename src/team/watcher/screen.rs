//! Screen state detection — classifies tmux pane captures into agent states.
//!
//! Identifies idle prompts, active spinners, context exhaustion messages,
//! and determines the next watcher state based on screen + tracker signals.

use super::{TrackerKind, TrackerState, WatcherState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ScreenState {
    Active,
    Idle,
    ContextExhausted,
    Unknown,
}

/// Check if the captured pane output shows an idle prompt.
///
/// This covers Claude's `❯` prompt, a shell prompt, and Codex's `›` composer.
///
/// Claude Code always renders `❯` at the bottom of the screen — even while
/// actively working.  The reliable differentiator is the status bar at the
/// very bottom: when Claude is processing, it appends `· esc to interrupt`.
/// If we detect that indicator we return `false` immediately.
pub fn is_at_agent_prompt(capture: &str) -> bool {
    // Use 12 non-empty lines to account for Claude's separators and status
    // bar pushing the prompt further up than a tight tail window.
    let trimmed = recent_non_empty_lines(capture, 12);

    // Claude Code shows "esc to interrupt" in the current bottom status bar
    // while working. Restrict this check to the raw bottom window so older
    // non-empty lines higher in the transcript do not pin the watcher active.
    for line in &recent_lines(capture, 6) {
        if is_live_interrupt_footer(line) {
            return false;
        }
    }

    for line in &trimmed {
        let l = line.trim();
        // Claude Code idle prompt
        if starts_with_agent_prompt(l, '❯') {
            return true;
        }
        // Codex idle composer prompt
        if starts_with_agent_prompt(l, '›') {
            return true;
        }
        // Kiro idle prompt
        if looks_like_kiro_prompt(l) {
            return true;
        }
        // Fell back to shell
        if l.ends_with("$ ") || l == "$" {
            return true;
        }
    }
    false
}

fn starts_with_agent_prompt(line: &str, prompt: char) -> bool {
    let Some(rest) = line.strip_prefix(prompt) else {
        return false;
    };
    rest.is_empty()
        || rest
            .chars()
            .next()
            .map(char::is_whitespace)
            .unwrap_or(false)
}

fn looks_like_kiro_prompt(line: &str) -> bool {
    matches!(line, "Kiro>" | "kiro>" | "Kiro >" | "kiro >" | ">")
}

pub(super) fn recent_non_empty_lines(capture: &str, limit: usize) -> Vec<&str> {
    capture
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(limit)
        .collect()
}

pub(super) fn recent_lines(capture: &str, limit: usize) -> Vec<&str> {
    capture.lines().rev().take(limit).collect()
}

pub(super) fn classify_capture_state(capture: &str) -> ScreenState {
    let trimmed = recent_non_empty_lines(capture, 12);

    if recent_lines(capture, 6)
        .iter()
        .any(|line| is_live_interrupt_footer(line))
    {
        return ScreenState::Active;
    }

    if capture_contains_context_exhaustion(capture) {
        return ScreenState::ContextExhausted;
    }

    if is_at_agent_prompt(capture) {
        return ScreenState::Idle;
    }

    if trimmed
        .iter()
        .any(|line| looks_like_claude_spinner_status(line))
    {
        return ScreenState::Active;
    }

    if trimmed
        .iter()
        .any(|line| looks_like_kiro_spinner_status(line))
    {
        return ScreenState::Active;
    }

    ScreenState::Unknown
}

pub(super) fn detect_context_exhausted(capture: &str) -> bool {
    capture_contains_context_exhaustion(capture)
}

fn looks_like_claude_spinner_status(line: &str) -> bool {
    let trimmed = line.trim();
    let Some(first) = trimmed.chars().next() else {
        return false;
    };
    matches!(first, '·' | '✢' | '✳' | '✶' | '✻' | '✽')
        && (trimmed.contains('…') || trimmed.contains("(thinking"))
}

fn looks_like_kiro_spinner_status(line: &str) -> bool {
    let trimmed = line.trim().to_ascii_lowercase();
    (trimmed.contains("kiro") || trimmed.contains("agent"))
        && (trimmed.contains("thinking")
            || trimmed.contains("planning")
            || trimmed.contains("applying")
            || trimmed.contains("working"))
}

fn is_live_interrupt_footer(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.contains("esc to interrupt")
        || trimmed.contains("esc to inter")
        || trimmed.contains("esc to in…")
        || trimmed.contains("esc to in...")
}

fn capture_contains_context_exhaustion(capture: &str) -> bool {
    let lowered = capture.to_ascii_lowercase();
    lowered.contains("context window exceeded")
        || lowered.contains("context window is full")
        || lowered.contains("conversation is too long")
        || lowered.contains("maximum context length")
        || lowered.contains("context limit reached")
        || lowered.contains("truncated due to context limit")
        || lowered.contains("input exceeds the model")
        || lowered.contains("prompt is too long")
}

pub(super) fn next_state_after_capture(
    tracker_kind: TrackerKind,
    screen_state: ScreenState,
    tracker_state: TrackerState,
    previous_state: WatcherState,
) -> WatcherState {
    if screen_state == ScreenState::ContextExhausted {
        return WatcherState::ContextExhausted;
    }

    if tracker_kind == TrackerKind::Claude {
        match screen_state {
            // Claude's live pane state is more reliable than session logs when
            // multiple matching JSONL files exist. A visible spinner or
            // interrupt bar means working; a clean prompt with neither means
            // idle, even if an old session file still looks active.
            ScreenState::Active => return WatcherState::Active,
            ScreenState::Idle => return WatcherState::Idle,
            ScreenState::ContextExhausted => return WatcherState::ContextExhausted,
            ScreenState::Unknown => {}
        }
    }

    match tracker_state {
        TrackerState::Active => return WatcherState::Active,
        TrackerState::Idle | TrackerState::Completed => return WatcherState::Idle,
        TrackerState::Unknown => {}
    }

    match screen_state {
        ScreenState::Active => WatcherState::Active,
        ScreenState::Idle => WatcherState::Idle,
        ScreenState::ContextExhausted => WatcherState::ContextExhausted,
        ScreenState::Unknown => previous_state,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_claude_code_prompt() {
        let capture = "⏺ Done.\n\n❯ \n\n  bypass permissions\n";
        assert!(is_at_agent_prompt(capture));
    }

    #[test]
    fn detects_shell_prompt() {
        let capture = "some output\n$ \n";
        assert!(is_at_agent_prompt(capture));
    }

    #[test]
    fn detects_codex_prompt() {
        let capture =
            "› Improve documentation in @filename\n\n  gpt-5.4 high · 84% left · ~/repo\n";
        assert!(is_at_agent_prompt(capture));
    }

    #[test]
    fn detects_kiro_prompt() {
        let capture = "Kiro>\n";
        assert!(is_at_agent_prompt(capture));
        assert_eq!(classify_capture_state(capture), ScreenState::Idle);
    }

    #[test]
    fn no_prompt_when_working() {
        let capture = "⏺ Bash(python -m pytest)\n  ⎿  running tests...\n";
        assert!(!is_at_agent_prompt(capture));
    }

    #[test]
    fn claude_working_not_idle_despite_prompt_visible() {
        let capture = concat!(
            "✻ Slithering… (4m 12s)\n",
            "  ⎿  Tip: Use /btw to ask a quick side question\n",
            "────────────────────────────\n",
            "❯ \n",
            "────────────────────────────\n",
            "  ⏵⏵ bypass permissions on (shift+tab to cycle) · esc to interrupt\n",
        );
        assert!(!is_at_agent_prompt(capture));
    }

    #[test]
    fn claude_working_not_idle_when_interrupt_footer_is_truncated() {
        let capture = concat!(
            "✢ Cascading… (48s · ↓ 130 tokens · thought for 17s)\n",
            "  ⎿  Tip: Use /btw to ask a quick side question\n",
            "────────────────────────────\n",
            "❯ \n",
            "────────────────────────────\n",
            "  ⏵⏵ bypass permissions on (shift+tab to cycle) · esc to in…\n",
        );
        assert!(!is_at_agent_prompt(capture));
        assert_eq!(classify_capture_state(capture), ScreenState::Active);
    }

    #[test]
    fn claude_idle_detected_without_esc_to_interrupt() {
        let capture = concat!(
            "⏺ Done.\n",
            "────────────────────────────\n",
            "❯ \n",
            "────────────────────────────\n",
            "  ⏵⏵ bypass permissions on (shift+tab to cycle)\n",
        );
        assert!(is_at_agent_prompt(capture));
        assert_eq!(classify_capture_state(capture), ScreenState::Idle);
    }

    #[test]
    fn claude_context_window_message_marks_capture_exhausted() {
        let capture = concat!(
            "Claude cannot continue: conversation is too long.\n",
            "Start a new conversation or clear earlier context.\n",
            "❯ \n",
        );
        assert_eq!(
            classify_capture_state(capture),
            ScreenState::ContextExhausted
        );
    }

    #[test]
    fn codex_context_limit_message_marks_capture_exhausted() {
        let capture = concat!(
            "Request truncated due to context limit.\n",
            "Please start a fresh session with a smaller prompt.\n",
            "› \n",
        );
        assert_eq!(
            classify_capture_state(capture),
            ScreenState::ContextExhausted
        );
    }

    #[test]
    fn kiro_context_limit_message_marks_capture_exhausted() {
        let capture = concat!(
            "Kiro cannot continue because the conversation is too long.\n",
            "Please start a fresh session.\n",
            "Kiro>\n",
        );
        assert_eq!(
            classify_capture_state(capture),
            ScreenState::ContextExhausted
        );
    }

    #[test]
    fn ambiguous_context_wording_does_not_mark_capture_exhausted() {
        let capture = concat!(
            "We should reduce context window usage in the next refactor.\n",
            "That note is informational only.\n",
        );
        assert_eq!(classify_capture_state(capture), ScreenState::Unknown);
    }

    #[test]
    fn claude_pasted_text_prompt_counts_as_idle() {
        let capture = concat!(
            "✻ Crunched for 54s\n",
            "────────────────────────────────────────────────────────\n",
            "❯\u{00a0}[Pasted text #2 +40 lines]\n",
            "  --- Message from human ---\n",
            "  Provide me report of latest development\n",
            "  --- end message ---\n",
            "  To reply, run: batty send human \"<your response>\"\n",
            "────────────────────────────────────────────────────────\n",
            "  ⏵⏵ bypass permissions on (shift+tab to cycle)\n",
        );
        assert!(is_at_agent_prompt(capture));
        assert_eq!(classify_capture_state(capture), ScreenState::Idle);
    }

    #[test]
    fn claude_interrupted_prompt_not_idle() {
        let capture = concat!(
            "■ Conversation interrupted - tell the model what to do differently.\n",
            "  Something went wrong? Hit `/feedback` to report the issue.\n",
            "\n",
            "Interrupted · What should Claude do instead?\n",
            "❯ \n",
            "  ⏵⏵ bypass permissions on (shift+tab to cycle)\n",
        );
        assert!(is_at_agent_prompt(capture));
        assert_eq!(classify_capture_state(capture), ScreenState::Idle);
    }

    #[test]
    fn claude_historical_interruption_does_not_poison_idle_prompt() {
        let capture = concat!(
            "Interrupted · What should Claude do instead?\n",
            "Lots of old output here\n",
            "\n\n\n\n\n\n\n\n\n\n",
            "────────────────────────────\n",
            "❯ \n",
            "────────────────────────────\n",
            "  ⏵⏵ bypass permissions on (shift+tab to cycle)\n",
        );
        assert!(is_at_agent_prompt(capture));
        assert_eq!(classify_capture_state(capture), ScreenState::Idle);
    }

    #[test]
    fn claude_recent_interruption_without_esc_still_counts_as_idle() {
        let capture = concat!(
            "--- Message from manager ---\n",
            "No worries about the interrupted background task.\n",
            "--- end message ---\n",
            "To reply, run: batty send manager \"<your response>\"\n",
            "  ⎿  Interrupted · What should Claude do instead?\n",
            "\n",
            "⏺ Background command stopped\n",
            "────────────────────────────────────────────────────────\n",
            "❯ \n",
            "────────────────────────────────────────────────────────\n",
            "  ⏵⏵ bypass permissions on (shift+tab to cycle)\n",
        );
        assert!(is_at_agent_prompt(capture));
        assert_eq!(classify_capture_state(capture), ScreenState::Idle);
    }

    #[test]
    fn stale_esc_line_above_latest_prompt_does_not_pin_active() {
        let capture = concat!(
            "⏺ Bash(tmux capture-pane -t batty-mafia-adversarial-research:0.5 -p 2>/dev/null | tail -30)\n",
            "  ⎿  • Working (5s • esc to interrupt)\n",
            "     • Messages to be submitted after next tool call (press\n",
            "     … +9 lines (ctrl+o to expand)\n",
            "\n",
            "⏺ Good, the message is queued and will be processed. Let me wait a bit and check back.\n",
            "\n",
            "⏺ Bash(sleep 30 && tmux capture-pane -t batty-mafia-adversarial-research:0.5 -p 2>/dev/null | tail -30)\n",
            "  ⎿  Interrupted · What should Claude do instead?\n",
            "\n",
            "───────────────────────────────────────────────────────────────────────────────────────────────────────────\n",
            "❯\u{00a0}\n",
            "───────────────────────────────────────────────────────────────────────────────────────────────────────────\n",
            "  ⏵⏵ bypass permissions on (shift+tab to cycle)\n",
        );
        assert!(is_at_agent_prompt(capture));
        assert_eq!(classify_capture_state(capture), ScreenState::Idle);
    }

    #[test]
    fn claude_prompt_deep_in_output_still_detected_as_idle() {
        let capture = concat!(
            "⏺ Task merged to main.\n",
            "\n",
            "  ┌──────────┬──────────────────────┬──────────┐\n",
            "  │ Engineer │       Assignment       │  Status  │\n",
            "  ├──────────┼──────────────────────┼──────────┤\n",
            "  │ eng-1-1  │ Add features           │ Assigned │\n",
            "  └──────────┴──────────────────────┴──────────┘\n",
            "\n",
            "✻ Sautéed for 1m 56s\n",
            "\n",
            "────────────────────────────────────────────────────────\n",
            "❯ \n",
            "────────────────────────────────────────────────────────\n",
            "  ⏵⏵ bypass permissions on (shift+tab to cycle)\n",
        );
        assert!(is_at_agent_prompt(capture));
        assert_eq!(classify_capture_state(capture), ScreenState::Idle);
    }

    #[test]
    fn claude_spinner_status_marks_capture_active() {
        let capture = concat!(
            "✶ Envisioning… (thinking with high effort)\n",
            "────────────────────────────\n",
            "❯ \n",
            "────────────────────────────\n",
            "  ⏵⏵ bypass permissions on (shift+tab to cycle) · esc to interrupt\n",
        );
        assert_eq!(classify_capture_state(capture), ScreenState::Active);
    }

    #[test]
    fn kiro_spinner_status_marks_capture_active() {
        let capture = concat!(
            "Kiro Agent: thinking through the implementation plan\n",
            "Reviewing files and preparing edits\n",
        );
        assert_eq!(classify_capture_state(capture), ScreenState::Active);
    }

    #[test]
    fn claude_truncated_interrupt_footer_marks_capture_active() {
        let capture = concat!(
            "✻ Baked for 4m 30s\n",
            "────────────────────────────\n",
            "❯ \n",
            "────────────────────────────\n",
            "  ⏵⏵ bypass permissions on (shift+tab to cycle) · esc to in…\n",
        );
        assert_eq!(classify_capture_state(capture), ScreenState::Active);
    }

    #[test]
    fn codex_prompt_keeps_active_state_until_completion_event() {
        assert_eq!(
            next_state_after_capture(
                TrackerKind::Codex,
                ScreenState::Idle,
                TrackerState::Unknown,
                WatcherState::Idle,
            ),
            WatcherState::Idle
        );
        assert_eq!(
            next_state_after_capture(
                TrackerKind::Codex,
                ScreenState::Idle,
                TrackerState::Active,
                WatcherState::Idle,
            ),
            WatcherState::Active
        );
        assert_eq!(
            next_state_after_capture(
                TrackerKind::Codex,
                ScreenState::Idle,
                TrackerState::Idle,
                WatcherState::Active,
            ),
            WatcherState::Idle
        );
        assert_eq!(
            next_state_after_capture(
                TrackerKind::Codex,
                ScreenState::Unknown,
                TrackerState::Unknown,
                WatcherState::Active,
            ),
            WatcherState::Active
        );
        assert_eq!(
            next_state_after_capture(
                TrackerKind::Codex,
                ScreenState::Active,
                TrackerState::Unknown,
                WatcherState::Idle,
            ),
            WatcherState::Active
        );
        assert_eq!(
            next_state_after_capture(
                TrackerKind::Codex,
                ScreenState::Idle,
                TrackerState::Completed,
                WatcherState::Active,
            ),
            WatcherState::Idle
        );
        assert_eq!(
            next_state_after_capture(
                TrackerKind::Codex,
                ScreenState::ContextExhausted,
                TrackerState::Unknown,
                WatcherState::Active,
            ),
            WatcherState::ContextExhausted
        );
    }

    #[test]
    fn claude_idle_prompt_beats_stale_file_activity() {
        assert_eq!(
            next_state_after_capture(
                TrackerKind::Claude,
                ScreenState::Idle,
                TrackerState::Active,
                WatcherState::Active,
            ),
            WatcherState::Idle
        );
    }

    #[test]
    fn claude_spinner_beats_idle_file_state() {
        assert_eq!(
            next_state_after_capture(
                TrackerKind::Claude,
                ScreenState::Active,
                TrackerState::Idle,
                WatcherState::Idle,
            ),
            WatcherState::Active
        );
    }

    #[test]
    fn ready_state_transitions_to_active_on_work() {
        assert_eq!(
            next_state_after_capture(
                TrackerKind::None,
                ScreenState::Active,
                TrackerState::Unknown,
                WatcherState::Ready,
            ),
            WatcherState::Active
        );
    }

    #[test]
    fn ready_state_transitions_to_idle_on_idle_screen() {
        assert_eq!(
            next_state_after_capture(
                TrackerKind::None,
                ScreenState::Idle,
                TrackerState::Unknown,
                WatcherState::Ready,
            ),
            WatcherState::Idle
        );
    }

    #[test]
    fn ready_state_stays_on_unknown_screen() {
        assert_eq!(
            next_state_after_capture(
                TrackerKind::None,
                ScreenState::Unknown,
                TrackerState::Unknown,
                WatcherState::Ready,
            ),
            WatcherState::Ready
        );
    }
}
