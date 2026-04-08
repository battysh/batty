//! State classifiers: determine agent state from virtual screen content.
//!
//! Each agent type (Claude, Codex, Kiro, Generic) has different prompt
//! patterns, spinner indicators, and context exhaustion messages.

use serde::{Deserialize, Serialize};

/// What the classifier thinks the agent is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenVerdict {
    /// Agent is at its input prompt, waiting for a message.
    AgentIdle,
    /// Agent is actively processing (producing output).
    AgentWorking,
    /// Agent reported conversation/context too large.
    ContextExhausted,
    /// Can't determine — keep previous state.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Classification {
    pub verdict: ScreenVerdict,
    pub confidence: f32,
}

impl Classification {
    const fn exact(verdict: ScreenVerdict) -> Self {
        Self {
            verdict,
            confidence: 1.0,
        }
    }

    const fn ambiguous(verdict: ScreenVerdict) -> Self {
        Self {
            verdict,
            confidence: 0.45,
        }
    }

    const fn unknown() -> Self {
        Self {
            verdict: ScreenVerdict::Unknown,
            confidence: 0.0,
        }
    }
}

pub const MIN_CLASSIFIER_CONFIDENCE: f32 = 0.75;

/// How a single output line should be treated by narration enforcement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NarrationLineKind {
    Explanation,
    ToolOrCommand,
    Other,
}

/// Agent type selector for the shim classifier.
///
/// This operates on vt100::Screen content, independent of the AgentType
/// in src/agent/ which works with tmux capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentType {
    Claude,
    Codex,
    Kiro,
    Generic,
}

impl std::str::FromStr for AgentType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "kiro" => Ok(Self::Kiro),
            "generic" | "bash" | "shell" => Ok(Self::Generic),
            _ => Err(format!("unknown agent type: {s}")),
        }
    }
}

impl std::fmt::Display for AgentType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Claude => write!(f, "claude"),
            Self::Codex => write!(f, "codex"),
            Self::Kiro => write!(f, "kiro"),
            Self::Generic => write!(f, "generic"),
        }
    }
}

// ---------------------------------------------------------------------------
// Classifier dispatch
// ---------------------------------------------------------------------------

/// Classify screen content based on agent type.
pub fn classify(agent_type: AgentType, screen: &vt100::Screen) -> ScreenVerdict {
    let result = classify_with_confidence(agent_type, screen);
    if result.confidence >= MIN_CLASSIFIER_CONFIDENCE {
        result.verdict
    } else {
        ScreenVerdict::Unknown
    }
}

/// Classify screen content and return a confidence score for the match.
pub fn classify_with_confidence(agent_type: AgentType, screen: &vt100::Screen) -> Classification {
    let content = screen.contents();
    if content.trim().is_empty() {
        return Classification::unknown();
    }

    // Context exhaustion check (common across all types)
    if detect_context_exhausted(&content) {
        return Classification::exact(ScreenVerdict::ContextExhausted);
    }

    match agent_type {
        AgentType::Claude => classify_claude(&content),
        AgentType::Codex => classify_codex(&content),
        AgentType::Kiro => classify_kiro(&content),
        AgentType::Generic => classify_generic(&content),
    }
}

/// Detect meta-conversation patterns where the agent keeps planning or
/// narrating without moving to concrete execution.
pub fn detect_meta_conversation(content: &str, agent_type: AgentType) -> bool {
    let lower = content.to_lowercase();
    let trimmed = lower.trim();
    if trimmed.is_empty() {
        return false;
    }

    let tool_markers: &[&str] = match agent_type {
        AgentType::Claude => &[
            "read(",
            "edit(",
            "bash(",
            "write(",
            "grep(",
            "glob(",
            "multiedit(",
            "⎿",
        ],
        AgentType::Codex => &[
            "apply_patch",
            "*** begin patch",
            "$ ",
            "\n$ ",
            "exit code:",
            "target/",
        ],
        AgentType::Kiro => &["applying", "$ ", "\n$ ", "running…", "running..."],
        AgentType::Generic => &["$ ", "\n$ ", "exit code:"],
    };
    if tool_markers.iter().any(|marker| trimmed.contains(marker)) {
        return false;
    }

    let meta_patterns = [
        "i should",
        "i will",
        "i'll",
        "let me",
        "next step",
        "need to",
        "we need to",
        "should i",
        "maybe i should",
        "perhaps i should",
        "i can",
        "plan:",
        "thinking through",
        "first, i'll",
        "then i'll",
        "instead of",
    ];
    let question_patterns = [
        "should i",
        "what should i",
        "do i need to",
        "am i supposed to",
        "would it make sense",
    ];

    let meta_hits = meta_patterns
        .iter()
        .filter(|pattern| trimmed.contains(**pattern))
        .count();
    let question_hits = question_patterns
        .iter()
        .filter(|pattern| trimmed.contains(**pattern))
        .count();
    let line_hits = trimmed
        .lines()
        .filter(|line| {
            let line = line.trim();
            !line.is_empty()
                && (meta_patterns.iter().any(|pattern| line.contains(pattern))
                    || question_patterns
                        .iter()
                        .any(|pattern| line.contains(pattern))
                    || line.ends_with('?'))
        })
        .count();

    (meta_hits + question_hits) >= 2 || line_hits >= 2
}

/// Classify a single output line for narration-loop detection.
pub fn classify_narration_line(line: &str, agent_type: AgentType) -> NarrationLineKind {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return NarrationLineKind::Other;
    }

    if has_command_or_tool_signal(trimmed, agent_type) {
        return NarrationLineKind::ToolOrCommand;
    }

    let lower = trimmed.to_ascii_lowercase();
    let explanation_patterns = [
        "i should",
        "i will",
        "i'll",
        "let me",
        "next step",
        "need to",
        "we need to",
        "should i",
        "maybe i should",
        "perhaps i should",
        "i can",
        "plan:",
        "thinking through",
        "first, i'll",
        "then i'll",
        "instead of",
        "i'm going to",
        "before i",
    ];
    if explanation_patterns
        .iter()
        .any(|pattern| lower.contains(pattern))
        || lower.ends_with('?')
    {
        return NarrationLineKind::Explanation;
    }

    NarrationLineKind::Other
}

/// Detect whether the current screen content looks like narration instead of
/// concrete execution. This is intentionally screen-level so daemon health
/// checks can count consecutive narration polls.
pub fn detect_narration_pattern(content: &str, agent_type: AgentType) -> bool {
    let mut explanation_lines = 0usize;

    for line in content.lines() {
        match classify_narration_line(line, agent_type) {
            NarrationLineKind::ToolOrCommand => return false,
            NarrationLineKind::Explanation => explanation_lines += 1,
            NarrationLineKind::Other => {}
        }
    }

    explanation_lines > 0
}

fn has_command_or_tool_signal(line: &str, agent_type: AgentType) -> bool {
    let common_markers = [
        "*** Begin Patch",
        "*** Update File:",
        "*** Add File:",
        "*** Delete File:",
        "$ ",
        "> ",
        "Exit code:",
        "apply_patch",
    ];
    if common_markers.iter().any(|marker| line.contains(marker)) {
        return true;
    }

    let trimmed = line.trim_start();
    let shell_prefixes = [
        "git ",
        "cargo ",
        "rg ",
        "sed ",
        "ls ",
        "cat ",
        "grep ",
        "find ",
        "npm ",
        "pnpm ",
        "yarn ",
        "pytest",
        "go test",
        "make ",
        "batty ",
        "kanban-md ",
    ];
    if shell_prefixes
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
    {
        return true;
    }

    match agent_type {
        AgentType::Claude => [
            "Read(",
            "Edit(",
            "Bash(",
            "Write(",
            "Grep(",
            "Glob(",
            "MultiEdit(",
            "⎿",
        ]
        .iter()
        .any(|marker| line.contains(marker)),
        AgentType::Codex => ["target/", "apply_patch"]
            .iter()
            .any(|marker| line.contains(marker)),
        AgentType::Kiro => ["applying", "running…", "running..."]
            .iter()
            .any(|marker| line.to_ascii_lowercase().contains(marker)),
        AgentType::Generic => false,
    }
}

// ---------------------------------------------------------------------------
// Context exhaustion (shared)
// ---------------------------------------------------------------------------

const EXHAUSTION_PATTERNS: &[&str] = &[
    "context window exceeded",
    "context window is full",
    "conversation is too long",
    "maximum context length",
    "context limit reached",
    "truncated due to context limit",
    "input exceeds the model",
    "prompt is too long",
];

fn detect_context_exhausted(content: &str) -> bool {
    let lower = content.to_lowercase();
    EXHAUSTION_PATTERNS.iter().any(|p| lower.contains(p))
}

// ---------------------------------------------------------------------------
// Claude Code classifier
// ---------------------------------------------------------------------------

/// Claude Code prompt characters.
// Claude prompt and spinner chars retained for use in tests and other classifiers.
#[allow(dead_code)]
const CLAUDE_PROMPT_CHARS: &[char] = &['\u{276F}']; // ❯

/// Claude spinner prefixes.
#[allow(dead_code)]
const CLAUDE_SPINNER_CHARS: &[char] = &[
    '\u{00B7}', // ·
    '\u{2722}', // ✢
    '\u{2733}', // ✳
    '\u{2736}', // ✶
    '\u{273B}', // ✻
    '\u{273D}', // ✽
];

fn classify_claude(content: &str) -> Classification {
    // Claude Code classification based on the status bar and tool execution indicators.
    //
    // Working signals (any of these = Working):
    //   - "esc to interrupt" in status bar (agent thinking/generating)
    //   - "ctrl+b to run in background" (tool/bash command executing)
    //   - "Waiting…" or "Running…" (tool execution in progress)
    // Idle signal:
    //   - Status bar present with none of the above
    //
    // The status bar is always in the last few lines of the terminal.
    let lines: Vec<&str> = content.lines().collect();
    let bottom: Vec<&str> = lines.iter().rev().take(6).copied().collect();

    let working_confidence = bottom
        .iter()
        .filter_map(|line| claude_working_confidence(line))
        .max_by(f32::total_cmp);

    if let Some(confidence) = working_confidence {
        return if confidence >= MIN_CLASSIFIER_CONFIDENCE {
            Classification {
                verdict: ScreenVerdict::AgentWorking,
                confidence,
            }
        } else {
            Classification::ambiguous(ScreenVerdict::AgentWorking)
        };
    }

    let idle_confidence = bottom
        .iter()
        .filter_map(|line| {
            best_phrase_confidence(line, &["bypass permissions", "shift+tab", "ctrl+g to edit"])
        })
        .max_by(f32::total_cmp);

    if let Some(confidence) = idle_confidence {
        return Classification {
            verdict: ScreenVerdict::AgentIdle,
            confidence,
        };
    }

    Classification::unknown()
}

// ---------------------------------------------------------------------------
// Startup dialog detection (for auto-dismiss during startup)
// ---------------------------------------------------------------------------

/// Patterns that indicate an agent trust/consent dialog requiring auto-dismiss.
/// Shared across agent types — both Claude and Codex show trust dialogs.
const STARTUP_DIALOG_PATTERNS: &[&str] = &[
    // Claude
    "is this a project you created",
    "quick safety check",
    "enter to confirm",
    "yes, i trust this folder",
    // Codex
    "do you trust the contents",
    "press enter to continue",
    "yes, continue",
    "working with untrusted contents",
];

/// Detect known startup dialogs that should be auto-dismissed by the shim.
/// Works for Claude, Codex, and other agents that show trust prompts.
pub fn detect_startup_dialog(content: &str) -> bool {
    let lower = content.to_lowercase();
    STARTUP_DIALOG_PATTERNS.iter().any(|p| lower.contains(p))
}

/// Legacy alias for backward compatibility in tests.
pub fn detect_claude_dialog(content: &str) -> bool {
    detect_startup_dialog(content)
}

#[allow(dead_code)]
fn looks_like_claude_spinner(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    let first = trimmed.chars().next().unwrap();
    CLAUDE_SPINNER_CHARS.contains(&first)
        && (trimmed.contains('\u{2026}') || trimmed.contains("(thinking"))
}

// ---------------------------------------------------------------------------
// Codex classifier
// ---------------------------------------------------------------------------

fn classify_codex(content: &str) -> Classification {
    let lines: Vec<&str> = content.lines().collect();
    let recent_nonempty: Vec<&str> = lines
        .iter()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(12)
        .copied()
        .collect();

    // Check for Codex working/loading indicators before idle check.
    // "esc to interrupt" means Codex is actively working or loading.
    for line in &recent_nonempty {
        if let Some(confidence) =
            best_phrase_confidence(line, &["esc to interrupt", "starting mcp", "executing"])
        {
            return if confidence >= MIN_CLASSIFIER_CONFIDENCE {
                Classification {
                    verdict: ScreenVerdict::AgentWorking,
                    confidence,
                }
            } else {
                Classification::ambiguous(ScreenVerdict::AgentWorking)
            };
        }
    }

    // Codex prompt: › at the start of a recent line.
    // Codex shows placeholder text after › (e.g., "› Explain this codebase")
    // which is greyed-out suggestion text — still idle.
    // Only idle when no working indicators are present.
    for line in &recent_nonempty {
        let trimmed = line.trim();
        if trimmed.starts_with('\u{203A}') {
            let confidence =
                if trimmed.strip_prefix('\u{203A}').is_some_and(|r| r.trim().is_empty()) {
                    1.0
                } else {
                    0.92
                };
            return Classification {
                verdict: ScreenVerdict::AgentIdle,
                confidence,
            };
        }
    }

    Classification::unknown()
}

// ---------------------------------------------------------------------------
// Kiro classifier
// ---------------------------------------------------------------------------

fn classify_kiro(content: &str) -> Classification {
    let lines: Vec<&str> = content.lines().collect();
    let recent_nonempty: Vec<&str> = lines
        .iter()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(12)
        .copied()
        .collect();

    // Check for working/loading indicators first
    for line in &recent_nonempty {
        // Kiro-cli uses ● spinner during initialization and ⠉/⠋ braille
        // spinners during processing
        if let Some(confidence) = best_phrase_confidence(
            line,
            &[
                "initializing",
                "esc to interrupt",
                "thinking",
                "planning",
                "applying",
            ],
        ) {
            return Classification {
                verdict: ScreenVerdict::AgentWorking,
                confidence,
            };
        }
    }

    // Kiro-cli prompt: "ask a question, or describe a task"
    // This is the placeholder text shown when kiro-cli is idle.
    let lower_content = content.to_lowercase();
    if lower_content.contains("ask a question") || lower_content.contains("describe a task") {
        return Classification::exact(ScreenVerdict::AgentIdle);
    }

    // Kiro prompts: Kiro>, kiro>, Kiro >, kiro >, or bare >
    // Only match when the prompt has no typed content after it.
    for line in &recent_nonempty {
        let trimmed = line.trim();
        let lower = trimmed.to_lowercase();
        if trimmed == ">" || trimmed == "> " {
            return Classification::exact(ScreenVerdict::AgentIdle);
        }
        if lower.starts_with("kiro>") {
            let after = &trimmed["kiro>".len()..];
            if after.trim().is_empty() {
                return Classification::exact(ScreenVerdict::AgentIdle);
            }
        } else if lower.starts_with("kiro >") {
            let after = &trimmed["kiro >".len()..];
            if after.trim().is_empty() {
                return Classification::exact(ScreenVerdict::AgentIdle);
            }
        }
        if trimmed.ends_with("> ") || trimmed.ends_with('>') {
            let before_gt = trimmed.trim_end_matches(['>', ' ']);
            if before_gt.len() < trimmed.len() {
                return Classification::ambiguous(ScreenVerdict::AgentIdle);
            }
        }
    }

    Classification::unknown()
}

// ---------------------------------------------------------------------------
// Generic classifier (bash / shell / REPL)
// ---------------------------------------------------------------------------

fn classify_generic(content: &str) -> Classification {
    let lines: Vec<&str> = content.lines().collect();
    let recent_nonempty: Vec<&str> = lines
        .iter()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(6)
        .copied()
        .collect();

    for line in &recent_nonempty {
        let trimmed = line.trim();
        // Shell prompts: ends with "$ " or "$", or "% " or "%", or "> " or ">"
        if trimmed.ends_with("$ ")
            || trimmed.ends_with('$')
            || trimmed.ends_with("% ")
            || trimmed.ends_with('%')
            || (trimmed.ends_with("> ") && trimmed.len() > 1)
            || (trimmed.ends_with('>') && trimmed.len() > 1)
        {
            return Classification::exact(ScreenVerdict::AgentIdle);
        }
        if trimmed == ">" || trimmed == "> " {
            return Classification::ambiguous(ScreenVerdict::AgentIdle);
        }
    }

    Classification::unknown()
}

fn best_phrase_confidence(line: &str, phrases: &[&str]) -> Option<f32> {
    phrases
        .iter()
        .filter_map(|phrase| phrase_match_confidence(line, phrase))
        .max_by(f32::total_cmp)
}

fn phrase_match_confidence(line: &str, phrase: &str) -> Option<f32> {
    let normalized_line = normalize_match_text(line);
    let normalized_phrase = normalize_match_text(phrase);
    if normalized_line.is_empty() || normalized_phrase.is_empty() {
        return None;
    }

    if normalized_line.contains(&normalized_phrase) {
        return Some(1.0);
    }

    let line_tokens = normalized_line.split_whitespace().collect::<Vec<_>>();
    let phrase_tokens = normalized_phrase.split_whitespace().collect::<Vec<_>>();
    if line_tokens.is_empty() || phrase_tokens.is_empty() {
        return None;
    }

    let max_start = line_tokens.len().saturating_sub(1);
    for start in 0..=max_start {
        let score = token_prefix_score(&line_tokens[start..], &phrase_tokens);
        if score > 0.0 {
            return Some(score);
        }
    }

    None
}

fn claude_working_confidence(line: &str) -> Option<f32> {
    let lower = normalize_match_text(line);
    if lower.contains("esc to interrupt")
        || lower.contains("ctrl+b to run in background")
        || lower.contains("waiting")
        || lower.contains("running")
    {
        return Some(1.0);
    }

    if lower.contains("esc to inter")
        || lower.contains("esc to in...")
        || lower.contains("esc t...")
        || lower.contains("ctrl+b to run")
        || lower.contains("ctrl+b to r")
        || (lower.contains("esc t")
            && (lower.contains("bypass")
                || lower.contains("shift+tab")
                || lower.contains("ctrl+g")))
    {
        return Some(0.84);
    }

    None
}

fn token_prefix_score(line_tokens: &[&str], phrase_tokens: &[&str]) -> f32 {
    let mut matched = 0usize;
    let mut consumed_chars = 0usize;
    let mut used_prefix = false;

    for (line_token, phrase_token) in line_tokens.iter().zip(phrase_tokens.iter()) {
        if *line_token == *phrase_token {
            matched += 1;
            consumed_chars += phrase_token.len();
            continue;
        }
        if phrase_token.starts_with(*line_token) && !line_token.is_empty() {
            matched += 1;
            consumed_chars += line_token.len();
            used_prefix = true;
            break;
        }
        return 0.0;
    }

    if matched == 0 {
        return 0.0;
    }

    let phrase_chars = phrase_tokens.iter().map(|token| token.len()).sum::<usize>();
    let coverage = consumed_chars as f32 / phrase_chars as f32;

    if matched == phrase_tokens.len() && !used_prefix {
        return 1.0;
    }

    if matched >= 2 && coverage >= 0.45 {
        return 0.84;
    }

    if matched == 1 && coverage >= 0.25 {
        return 0.45;
    }

    0.0
}

fn normalize_match_text(value: &str) -> String {
    value
        .to_ascii_lowercase()
        .replace('\u{2026}', "...")
        .replace("…", "...")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_screen(content: &str) -> vt100::Parser {
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(content.as_bytes());
        parser
    }

    fn classify_result(agent_type: AgentType, content: &str) -> Classification {
        let parser = make_screen(content);
        classify_with_confidence(agent_type, parser.screen())
    }

    // -- Claude --

    #[test]
    fn claude_idle_prompt() {
        // Status bar without "esc to interrupt" = idle
        let parser =
            make_screen("Some output\n\u{276F}\n  bypass permissions on (shift+tab to cycle)");
        assert_eq!(
            classify(AgentType::Claude, parser.screen()),
            ScreenVerdict::AgentIdle
        );
    }

    #[test]
    fn claude_idle_bare_prompt() {
        // Status bar with "ctrl+g to edit" but no interrupt = idle
        let parser = make_screen("Some output\n\u{276F}\n  ctrl+g to edit in Vim");
        assert_eq!(
            classify(AgentType::Claude, parser.screen()),
            ScreenVerdict::AgentIdle
        );
    }

    #[test]
    fn claude_working_spinner() {
        // Status bar with "esc to interrupt" = working
        let parser =
            make_screen("\u{00B7} Thinking\u{2026}\n  bypass permissions on · esc to interrupt");
        assert_eq!(
            classify(AgentType::Claude, parser.screen()),
            ScreenVerdict::AgentWorking
        );
    }

    #[test]
    fn claude_working_interrupt_footer() {
        let parser = make_screen("Some output\nesc to interrupt\n");
        assert_eq!(
            classify(AgentType::Claude, parser.screen()),
            ScreenVerdict::AgentWorking
        );
    }

    #[test]
    fn claude_working_interrupt_truncated() {
        let parser = make_screen("Some output\nesc to inter\n");
        assert_eq!(
            classify(AgentType::Claude, parser.screen()),
            ScreenVerdict::AgentWorking
        );
    }

    #[test]
    fn claude_working_interrupt_narrow_pane_ellipsis() {
        // Narrow pane truncates to "esc t…" with ellipsis
        let parser =
            make_screen("output\n  bypass permissions on (shift+tab) \u{00B7} esc t\u{2026}");
        assert_eq!(
            classify(AgentType::Claude, parser.screen()),
            ScreenVerdict::AgentWorking
        );
    }

    #[test]
    fn claude_working_interrupt_narrow_pane_cutoff() {
        // Narrow pane cuts mid-word with bypass context
        let parser = make_screen("output\n  bypass permissions on · esc t");
        assert_eq!(
            classify(AgentType::Claude, parser.screen()),
            ScreenVerdict::AgentWorking
        );
    }

    #[test]
    fn claude_exact_match_has_full_confidence() {
        let result = classify_result(
            AgentType::Claude,
            "Some output\n\u{276F}\n  bypass permissions on (shift+tab to cycle)",
        );
        assert_eq!(result.verdict, ScreenVerdict::AgentIdle);
        assert_eq!(result.confidence, 1.0);
    }

    #[test]
    fn claude_fuzzy_match_detects_truncated_footer() {
        let result = classify_result(
            AgentType::Claude,
            "output\n  bypass permissions on · esc to inter",
        );
        assert_eq!(result.verdict, ScreenVerdict::AgentWorking);
        assert!(result.confidence >= MIN_CLASSIFIER_CONFIDENCE);
        assert!(result.confidence < 1.0);
    }

    #[test]
    fn claude_truncated_idle_status_bar_matches_fuzzily() {
        let result = classify_result(AgentType::Claude, "output\n  bypass permiss");
        assert_eq!(result.verdict, ScreenVerdict::AgentIdle);
        assert!(result.confidence >= MIN_CLASSIFIER_CONFIDENCE);
        assert!(result.confidence < 1.0);
    }

    #[test]
    fn claude_ambiguous_status_returns_low_confidence() {
        let result = classify_result(AgentType::Claude, "output\n  shift");
        assert_eq!(result.verdict, ScreenVerdict::AgentIdle);
        assert!(result.confidence < MIN_CLASSIFIER_CONFIDENCE);

        let parser = make_screen("output\n  shift");
        assert_eq!(
            classify(AgentType::Claude, parser.screen()),
            ScreenVerdict::Unknown
        );
    }

    #[test]
    fn claude_context_exhausted() {
        let parser = make_screen("Error: context window is full\n\u{276F} ");
        assert_eq!(
            classify(AgentType::Claude, parser.screen()),
            ScreenVerdict::ContextExhausted
        );
    }

    // -- Codex --

    #[test]
    fn codex_idle_prompt() {
        let parser = make_screen("Done.\n\n\u{203A} ");
        assert_eq!(
            classify(AgentType::Codex, parser.screen()),
            ScreenVerdict::AgentIdle
        );
    }

    #[test]
    fn codex_idle_bare_prompt() {
        let parser = make_screen("Done.\n\n\u{203A}");
        assert_eq!(
            classify(AgentType::Codex, parser.screen()),
            ScreenVerdict::AgentIdle
        );
    }

    #[test]
    fn codex_unknown_no_prompt() {
        let parser = make_screen("Running something...\n");
        assert_eq!(
            classify(AgentType::Codex, parser.screen()),
            ScreenVerdict::Unknown
        );
    }

    #[test]
    fn codex_idle_with_placeholder() {
        // Codex shows placeholder text after › — still idle
        let parser = make_screen("Output\n\u{203A} Explain this codebase\n");
        assert_eq!(
            classify(AgentType::Codex, parser.screen()),
            ScreenVerdict::AgentIdle,
            "placeholder text after › should be Idle"
        );
    }

    #[test]
    fn codex_truncated_interrupt_footer_matches_fuzzily() {
        let result = classify_result(AgentType::Codex, "loading\nesc to inter");
        assert_eq!(result.verdict, ScreenVerdict::AgentWorking);
        assert!(result.confidence >= MIN_CLASSIFIER_CONFIDENCE);
        assert!(result.confidence < 1.0);
    }

    #[test]
    fn detect_meta_conversation_flags_repeated_planning_without_tools() {
        let content = "I should inspect the daemon first.\nNext step: I will review the health loop.\nShould I patch narration or the classifier?";
        assert!(detect_meta_conversation(content, AgentType::Codex));
    }

    #[test]
    fn detect_meta_conversation_ignores_tool_execution_output() {
        let content = "I will inspect the daemon.\n$ rg -n narration src/team\nExit code: 0";
        assert!(!detect_meta_conversation(content, AgentType::Codex));
    }

    #[test]
    fn classify_narration_line_marks_explanations() {
        assert_eq!(
            classify_narration_line(
                "I will inspect the runtime before changing anything.",
                AgentType::Codex
            ),
            NarrationLineKind::Explanation
        );
    }

    #[test]
    fn classify_narration_line_marks_tool_output() {
        assert_eq!(
            classify_narration_line("$ cargo test -p batty", AgentType::Codex),
            NarrationLineKind::ToolOrCommand
        );
    }

    #[test]
    fn classify_narration_line_ignores_plain_output() {
        assert_eq!(
            classify_narration_line("src/team/daemon/health/narration.rs", AgentType::Codex),
            NarrationLineKind::Other
        );
    }

    #[test]
    fn detect_narration_pattern_matches_planning_without_tools() {
        let content = "I will inspect the daemon.\nLet me review the health loop.\nMy plan is to patch narration handling.";
        assert!(detect_narration_pattern(content, AgentType::Codex));
    }

    #[test]
    fn detect_narration_pattern_rejects_tool_execution() {
        let content = "I will inspect the daemon.\n$ rg -n narration src/team\nExit code: 0";
        assert!(!detect_narration_pattern(content, AgentType::Codex));
    }

    // -- Kiro --

    #[test]
    fn kiro_idle_prompt() {
        let parser = make_screen("Result\nKiro> ");
        assert_eq!(
            classify(AgentType::Kiro, parser.screen()),
            ScreenVerdict::AgentIdle
        );
    }

    #[test]
    fn kiro_idle_bare_gt() {
        let parser = make_screen("Result\n>");
        assert_eq!(
            classify(AgentType::Kiro, parser.screen()),
            ScreenVerdict::AgentIdle
        );
    }

    #[test]
    fn kiro_working() {
        let parser = make_screen("Kiro is thinking...\n");
        assert_eq!(
            classify(AgentType::Kiro, parser.screen()),
            ScreenVerdict::AgentWorking
        );
    }

    #[test]
    fn kiro_working_agent_planning() {
        let parser = make_screen("Agent is planning...\n");
        assert_eq!(
            classify(AgentType::Kiro, parser.screen()),
            ScreenVerdict::AgentWorking
        );
    }

    // -- Generic --

    #[test]
    fn generic_shell_prompt_dollar() {
        let parser = make_screen("user@host:~$ ");
        assert_eq!(
            classify(AgentType::Generic, parser.screen()),
            ScreenVerdict::AgentIdle
        );
    }

    #[test]
    fn generic_shell_prompt_percent() {
        let parser = make_screen("user@host:~% ");
        assert_eq!(
            classify(AgentType::Generic, parser.screen()),
            ScreenVerdict::AgentIdle
        );
    }

    #[test]
    fn generic_shell_prompt_gt() {
        let parser = make_screen("prompt> ");
        assert_eq!(
            classify(AgentType::Generic, parser.screen()),
            ScreenVerdict::AgentIdle
        );
    }

    #[test]
    fn generic_bare_gt_prompt_is_ambiguous_at_low_confidence() {
        let result = classify_result(AgentType::Generic, ">");
        assert_eq!(result.verdict, ScreenVerdict::AgentIdle);
        assert!(result.confidence < MIN_CLASSIFIER_CONFIDENCE);
    }

    #[test]
    fn generic_empty_unknown() {
        let parser = make_screen("");
        assert_eq!(
            classify(AgentType::Generic, parser.screen()),
            ScreenVerdict::Unknown
        );
    }

    #[test]
    fn unknown_pattern_returns_unknown_with_zero_confidence() {
        let result = classify_result(AgentType::Codex, "plain output with no known prompt");
        assert_eq!(result.verdict, ScreenVerdict::Unknown);
        assert_eq!(result.confidence, 0.0);
    }

    // -- Shared --

    #[test]
    fn exhaustion_all_types() {
        for agent_type in [
            AgentType::Claude,
            AgentType::Codex,
            AgentType::Kiro,
            AgentType::Generic,
        ] {
            let parser = make_screen("Error: conversation is too long to continue\n$ ");
            assert_eq!(
                classify(agent_type, parser.screen()),
                ScreenVerdict::ContextExhausted,
                "failed for {agent_type}",
            );
        }
    }

    #[test]
    fn exhaustion_maximum_context_length() {
        let parser = make_screen("Error: maximum context length exceeded\n$ ");
        assert_eq!(
            classify(AgentType::Generic, parser.screen()),
            ScreenVerdict::ContextExhausted
        );
    }

    #[test]
    fn agent_type_from_str() {
        assert_eq!("claude".parse::<AgentType>().unwrap(), AgentType::Claude);
        assert_eq!("CODEX".parse::<AgentType>().unwrap(), AgentType::Codex);
        assert_eq!("Kiro".parse::<AgentType>().unwrap(), AgentType::Kiro);
        assert_eq!("generic".parse::<AgentType>().unwrap(), AgentType::Generic);
        assert_eq!("bash".parse::<AgentType>().unwrap(), AgentType::Generic);
        assert_eq!("shell".parse::<AgentType>().unwrap(), AgentType::Generic);
        assert!("unknown".parse::<AgentType>().is_err());
    }

    #[test]
    fn agent_type_display() {
        assert_eq!(AgentType::Claude.to_string(), "claude");
        assert_eq!(AgentType::Codex.to_string(), "codex");
        assert_eq!(AgentType::Kiro.to_string(), "kiro");
        assert_eq!(AgentType::Generic.to_string(), "generic");
    }

    #[test]
    fn claude_dialog_not_idle() {
        // Trust dialog with ❯ as selection indicator — NOT an idle prompt
        let parser = make_screen(
            "Quick safety check: Is this a project you created or one you trust?\n\n\
             \u{276F} 1. Yes, I trust this folder\n\
             2. No, exit\n\n\
             Enter to confirm \u{00B7} Esc to cancel\n",
        );
        assert_ne!(
            classify(AgentType::Claude, parser.screen()),
            ScreenVerdict::AgentIdle,
            "trust dialog should NOT be classified as Idle"
        );
    }

    #[test]
    fn claude_dialog_detected() {
        let content = "Quick safety check: Is this a project you created or one you trust?\n\
                       \u{276F} 1. Yes, I trust this folder\n\
                       Enter to confirm";
        assert!(
            detect_claude_dialog(content),
            "should detect Claude trust dialog"
        );
    }

    #[test]
    fn claude_dialog_not_detected_normal() {
        let content = "Some response\n\u{276F} ";
        assert!(
            !detect_claude_dialog(content),
            "normal prompt should not trigger dialog detection"
        );
    }

    #[test]
    fn codex_dialog_detected() {
        let content = "Do you trust the contents of this directory?\n\
                       \u{203A} 1. Yes, continue\n\
                       Press enter to continue";
        assert!(
            detect_startup_dialog(content),
            "should detect Codex trust dialog"
        );
    }

    #[test]
    fn claude_idle_with_trailing_spaces() {
        // Status bar present, no interrupt = idle
        let parser =
            make_screen("Output\n\u{276F}    \n  bypass permissions on (shift+tab to cycle)    ");
        assert_eq!(
            classify(AgentType::Claude, parser.screen()),
            ScreenVerdict::AgentIdle
        );
    }

    #[test]
    fn all_exhaustion_patterns_trigger() {
        for pattern in EXHAUSTION_PATTERNS {
            let parser = make_screen(&format!("Error: {pattern}\n$ "));
            assert_eq!(
                classify(AgentType::Generic, parser.screen()),
                ScreenVerdict::ContextExhausted,
                "pattern '{pattern}' did not trigger exhaustion",
            );
        }
    }
}
