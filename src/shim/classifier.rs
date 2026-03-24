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
    let content = screen.contents();
    if content.trim().is_empty() {
        return ScreenVerdict::Unknown;
    }

    // Context exhaustion check (common across all types)
    if detect_context_exhausted(&content) {
        return ScreenVerdict::ContextExhausted;
    }

    match agent_type {
        AgentType::Claude => classify_claude(&content),
        AgentType::Codex => classify_codex(&content),
        AgentType::Kiro => classify_kiro(&content),
        AgentType::Generic => classify_generic(&content),
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
const CLAUDE_PROMPT_CHARS: &[char] = &['\u{276F}']; // ❯

/// Claude spinner prefixes.
const CLAUDE_SPINNER_CHARS: &[char] = &[
    '\u{00B7}', // ·
    '\u{2722}', // ✢
    '\u{2733}', // ✳
    '\u{2736}', // ✶
    '\u{273B}', // ✻
    '\u{273D}', // ✽
];

fn classify_claude(content: &str) -> ScreenVerdict {
    let lines: Vec<&str> = content.lines().collect();
    let recent_raw: Vec<&str> = lines.iter().rev().take(6).copied().collect();

    // "esc to interrupt" means Claude is actively working
    let has_interrupt_footer = recent_raw.iter().any(|line| {
        let trimmed = line.trim().to_lowercase();
        trimmed.contains("esc to interrupt")
            || trimmed.contains("esc to inter")
            || trimmed.contains("esc to in\u{2026}")
            || trimmed.contains("esc to in...")
    });

    if has_interrupt_footer {
        return ScreenVerdict::AgentWorking;
    }

    // Check for spinner in recent non-empty lines
    let recent_nonempty: Vec<&str> = lines
        .iter()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(12)
        .copied()
        .collect();

    for line in &recent_nonempty {
        if looks_like_claude_spinner(line) {
            return ScreenVerdict::AgentWorking;
        }
    }

    // Check for idle prompt: ❯ followed by whitespace or EOL
    for line in &recent_nonempty {
        let trimmed = line.trim();
        for &prompt_char in CLAUDE_PROMPT_CHARS {
            if trimmed.starts_with(prompt_char) {
                let after = &trimmed[prompt_char.len_utf8()..];
                if after.is_empty() || after.starts_with(|c: char| c.is_whitespace()) {
                    return ScreenVerdict::AgentIdle;
                }
            }
        }
    }

    ScreenVerdict::Unknown
}

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

fn classify_codex(content: &str) -> ScreenVerdict {
    let lines: Vec<&str> = content.lines().collect();
    let recent_nonempty: Vec<&str> = lines
        .iter()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(12)
        .copied()
        .collect();

    // Codex prompt: › followed by whitespace or EOL
    for line in &recent_nonempty {
        let trimmed = line.trim();
        if trimmed.starts_with('\u{203A}')
            && (trimmed.len() <= '\u{203A}'.len_utf8()
                || trimmed['\u{203A}'.len_utf8()..].starts_with(|c: char| c.is_whitespace()))
        {
            return ScreenVerdict::AgentIdle;
        }
    }

    ScreenVerdict::Unknown
}

// ---------------------------------------------------------------------------
// Kiro classifier
// ---------------------------------------------------------------------------

fn classify_kiro(content: &str) -> ScreenVerdict {
    let lines: Vec<&str> = content.lines().collect();
    let recent_nonempty: Vec<&str> = lines
        .iter()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(12)
        .copied()
        .collect();

    // Check for working indicators first
    for line in &recent_nonempty {
        let lower = line.to_lowercase();
        if (lower.contains("kiro") || lower.contains("agent"))
            && (lower.contains("thinking")
                || lower.contains("planning")
                || lower.contains("applying")
                || lower.contains("working"))
        {
            return ScreenVerdict::AgentWorking;
        }
    }

    // Kiro prompts: Kiro>, kiro>, Kiro >, kiro >, or bare >
    for line in &recent_nonempty {
        let trimmed = line.trim();
        if trimmed == ">"
            || trimmed.ends_with("> ")
            || trimmed.to_lowercase().starts_with("kiro>")
            || trimmed.to_lowercase().starts_with("kiro >")
        {
            return ScreenVerdict::AgentIdle;
        }
    }

    ScreenVerdict::Unknown
}

// ---------------------------------------------------------------------------
// Generic classifier (bash / shell / REPL)
// ---------------------------------------------------------------------------

fn classify_generic(content: &str) -> ScreenVerdict {
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
            || trimmed.ends_with("> ")
            || trimmed.ends_with('>')
        {
            return ScreenVerdict::AgentIdle;
        }
    }

    ScreenVerdict::Unknown
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

    // -- Claude --

    #[test]
    fn claude_idle_prompt() {
        let parser = make_screen("Some output\n\n\u{276F} ");
        assert_eq!(
            classify(AgentType::Claude, parser.screen()),
            ScreenVerdict::AgentIdle
        );
    }

    #[test]
    fn claude_idle_bare_prompt() {
        let parser = make_screen("Some output\n\n\u{276F}");
        assert_eq!(
            classify(AgentType::Claude, parser.screen()),
            ScreenVerdict::AgentIdle
        );
    }

    #[test]
    fn claude_working_spinner() {
        let parser = make_screen("\u{00B7} Thinking\u{2026}\n");
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
    fn generic_empty_unknown() {
        let parser = make_screen("");
        assert_eq!(
            classify(AgentType::Generic, parser.screen()),
            ScreenVerdict::Unknown
        );
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
