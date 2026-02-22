//! Prompt detection patterns for agent PTY output.
//!
//! Each agent family has different output conventions. This module provides
//! compiled regex patterns and a `PromptKind` classification so the supervisor
//! can decide what to do (auto-answer, escalate, log, etc.).
//!
//! ## Design notes
//!
//! **Claude Code** and **Codex CLI** use full-screen TUIs (alternate screen
//! buffer, cursor positioning). Raw PTY scraping sees ANSI escapes, not clean
//! lines. For reliable automation, prefer Claude's `-p --output-format
//! stream-json` or Codex's `--full-auto` mode. The patterns here target the
//! *text content after ANSI stripping* for cases where interactive mode is used.
//!
//! **Aider** uses a traditional line-oriented interface (`prompt_toolkit`),
//! making it the most amenable to PTY pattern matching.

use regex::Regex;

/// What kind of prompt was detected in the agent's output.
#[derive(Debug, Clone, PartialEq)]
pub enum PromptKind {
    /// Agent is asking for permission to run a command or edit a file.
    Permission { detail: String },
    /// Agent is asking a yes/no confirmation question.
    Confirmation { detail: String },
    /// Agent is asking the user a free-form question.
    Question { detail: String },
    /// Agent has finished its current turn / task.
    Completion,
    /// Agent encountered an error.
    Error { detail: String },
    /// Agent is waiting for user input (idle at prompt).
    WaitingForInput,
}

/// A detected prompt with its kind and the matched text.
#[derive(Debug, Clone)]
pub struct DetectedPrompt {
    pub kind: PromptKind,
    pub matched_text: String,
}

/// Compiled prompt detection patterns for a specific agent.
pub struct PromptPatterns {
    patterns: Vec<(Regex, PromptClassifier)>,
}

type PromptClassifier = fn(&str) -> PromptKind;

impl PromptPatterns {
    /// Scan a line of (ANSI-stripped) PTY output for known prompt patterns.
    /// Returns the first match found.
    pub fn detect(&self, line: &str) -> Option<DetectedPrompt> {
        for (regex, classify) in &self.patterns {
            if let Some(m) = regex.find(line) {
                return Some(DetectedPrompt {
                    kind: classify(m.as_str()),
                    matched_text: m.as_str().to_string(),
                });
            }
        }
        None
    }

    /// Build prompt patterns for Claude Code.
    ///
    /// Claude Code uses a full-screen TUI. These patterns target text content
    /// after ANSI stripping. For production use, prefer `-p --output-format
    /// stream-json` mode where completion is signaled by `"type":"result"`.
    pub fn claude_code() -> Self {
        Self {
            patterns: vec![
                // Permission / tool approval
                (Regex::new(r"(?i)allow\s+tool\b").unwrap(), |s| {
                    PromptKind::Permission {
                        detail: s.to_string(),
                    }
                }),
                // Yes/No confirmation
                (Regex::new(r"(?i)\[y/n\]").unwrap(), |s| {
                    PromptKind::Confirmation {
                        detail: s.to_string(),
                    }
                }),
                // Continue prompt
                (Regex::new(r"(?i)continue\?").unwrap(), |s| {
                    PromptKind::Confirmation {
                        detail: s.to_string(),
                    }
                }),
                // JSON stream error (must be before completion — error results are still results)
                (Regex::new(r#""is_error"\s*:\s*true"#).unwrap(), |s| {
                    PromptKind::Error {
                        detail: s.to_string(),
                    }
                }),
                // JSON stream completion (for -p --output-format stream-json)
                (Regex::new(r#""type"\s*:\s*"result""#).unwrap(), |_| {
                    PromptKind::Completion
                }),
            ],
        }
    }

    /// Build prompt patterns for Codex CLI.
    ///
    /// Codex CLI uses a full-screen ratatui TUI with alternate screen buffer.
    /// These patterns target text after ANSI stripping.
    pub fn codex_cli() -> Self {
        Self {
            patterns: vec![
                // Command execution approval
                (
                    Regex::new(r"Would you like to run the following command\?").unwrap(),
                    |s| PromptKind::Permission {
                        detail: s.to_string(),
                    },
                ),
                // File edit approval
                (
                    Regex::new(r"Would you like to make the following edits\?").unwrap(),
                    |s| PromptKind::Permission {
                        detail: s.to_string(),
                    },
                ),
                // Network access approval
                (
                    Regex::new(r#"Do you want to approve network access to ".*"\?"#).unwrap(),
                    |s| PromptKind::Permission {
                        detail: s.to_string(),
                    },
                ),
                // MCP approval
                (Regex::new(r".+ needs your approval\.").unwrap(), |s| {
                    PromptKind::Permission {
                        detail: s.to_string(),
                    }
                }),
                // Confirm/cancel footer
                (
                    Regex::new(r"Press .* to confirm or .* to cancel").unwrap(),
                    |s| PromptKind::Confirmation {
                        detail: s.to_string(),
                    },
                ),
                // Context window exceeded
                (Regex::new(r"(?i)context.?window.?exceeded").unwrap(), |s| {
                    PromptKind::Error {
                        detail: s.to_string(),
                    }
                }),
            ],
        }
    }

    /// Build prompt patterns for Aider.
    ///
    /// Aider uses a line-oriented interface, making it the most reliable
    /// target for PTY pattern matching.
    pub fn aider() -> Self {
        Self {
            patterns: vec![
                // Yes/No confirmation prompts: "(Y)es/(N)o [Yes]:"
                (
                    Regex::new(r"\(Y\)es/\(N\)o.*\[(Yes|No)\]:\s*$").unwrap(),
                    |s| PromptKind::Confirmation {
                        detail: s.to_string(),
                    },
                ),
                // Input prompt: "code> " or "architect> " or "> "
                (Regex::new(r"^(\w+\s*)?(multi\s+)?>\s$").unwrap(), |_| {
                    PromptKind::WaitingForInput
                }),
                // Edit applied
                (Regex::new(r"^Applied edit to\s+").unwrap(), |_| {
                    PromptKind::Completion
                }),
                // Token limit exceeded
                (Regex::new(r"exceeds the .* token limit").unwrap(), |s| {
                    PromptKind::Error {
                        detail: s.to_string(),
                    }
                }),
                // Empty LLM response
                (
                    Regex::new(r"Empty response received from LLM").unwrap(),
                    |s| PromptKind::Error {
                        detail: s.to_string(),
                    },
                ),
                // File errors
                (
                    Regex::new(r"(?:unable to read|file not found error|Unable to write)").unwrap(),
                    |s| PromptKind::Error {
                        detail: s.to_string(),
                    },
                ),
            ],
        }
    }
}

/// Strip ANSI escape sequences from PTY output.
pub fn strip_ansi(input: &str) -> String {
    // Matches CSI sequences (ESC [ ... final byte), OSC sequences (ESC ] ... ST),
    // and simple two-byte escapes (ESC + one char).
    static ANSI_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[^\[\]]").unwrap()
    });
    ANSI_RE.replace_all(input, "").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ANSI stripping ──

    #[test]
    fn strip_ansi_removes_csi() {
        let input = "\x1b[31mERROR\x1b[0m: something broke";
        assert_eq!(strip_ansi(input), "ERROR: something broke");
    }

    #[test]
    fn strip_ansi_removes_osc() {
        let input = "\x1b]0;title\x07some text";
        assert_eq!(strip_ansi(input), "some text");
    }

    #[test]
    fn strip_ansi_passthrough_clean_text() {
        let input = "just normal text";
        assert_eq!(strip_ansi(input), "just normal text");
    }

    // ── Claude Code patterns ──

    #[test]
    fn claude_detects_allow_tool() {
        let p = PromptPatterns::claude_code();
        let d = p.detect("Allow tool Read on /home/user/file.rs?").unwrap();
        assert!(matches!(d.kind, PromptKind::Permission { .. }));
    }

    #[test]
    fn claude_detects_yn_prompt() {
        let p = PromptPatterns::claude_code();
        let d = p.detect("Continue? [y/n]").unwrap();
        assert!(matches!(d.kind, PromptKind::Confirmation { .. }));
    }

    #[test]
    fn claude_detects_json_completion() {
        let p = PromptPatterns::claude_code();
        let line = r#"{"type": "result", "subtype": "success"}"#;
        let d = p.detect(line).unwrap();
        assert_eq!(d.kind, PromptKind::Completion);
    }

    #[test]
    fn claude_detects_json_error() {
        let p = PromptPatterns::claude_code();
        let line = r#"{"type": "result", "is_error": true}"#;
        let d = p.detect(line).unwrap();
        assert!(matches!(d.kind, PromptKind::Error { .. }));
    }

    #[test]
    fn claude_no_match_on_normal_output() {
        let p = PromptPatterns::claude_code();
        assert!(p.detect("Writing function to parse YAML...").is_none());
    }

    // ── Codex CLI patterns ──

    #[test]
    fn codex_detects_command_approval() {
        let p = PromptPatterns::codex_cli();
        let d = p
            .detect("Would you like to run the following command?")
            .unwrap();
        assert!(matches!(d.kind, PromptKind::Permission { .. }));
    }

    #[test]
    fn codex_detects_edit_approval() {
        let p = PromptPatterns::codex_cli();
        let d = p
            .detect("Would you like to make the following edits?")
            .unwrap();
        assert!(matches!(d.kind, PromptKind::Permission { .. }));
    }

    #[test]
    fn codex_detects_network_approval() {
        let p = PromptPatterns::codex_cli();
        let d = p
            .detect(r#"Do you want to approve network access to "api.example.com"?"#)
            .unwrap();
        assert!(matches!(d.kind, PromptKind::Permission { .. }));
    }

    // ── Aider patterns ──

    #[test]
    fn aider_detects_yn_confirmation() {
        let p = PromptPatterns::aider();
        let d = p
            .detect("Fix lint errors in main.rs? (Y)es/(N)o [Yes]: ")
            .unwrap();
        assert!(matches!(d.kind, PromptKind::Confirmation { .. }));
    }

    #[test]
    fn aider_detects_input_prompt() {
        let p = PromptPatterns::aider();
        let d = p.detect("code> ").unwrap();
        assert_eq!(d.kind, PromptKind::WaitingForInput);
    }

    #[test]
    fn aider_detects_bare_prompt() {
        let p = PromptPatterns::aider();
        let d = p.detect("> ").unwrap();
        assert_eq!(d.kind, PromptKind::WaitingForInput);
    }

    #[test]
    fn aider_detects_edit_completion() {
        let p = PromptPatterns::aider();
        let d = p.detect("Applied edit to src/main.rs").unwrap();
        assert_eq!(d.kind, PromptKind::Completion);
    }

    #[test]
    fn aider_detects_token_limit_error() {
        let p = PromptPatterns::aider();
        let d = p
            .detect(
                "Your estimated chat context of 50k tokens exceeds the 32k token limit for gpt-4!",
            )
            .unwrap();
        assert!(matches!(d.kind, PromptKind::Error { .. }));
    }

    #[test]
    fn aider_no_match_on_cost_report() {
        let p = PromptPatterns::aider();
        // Cost reports are informational, not prompts
        assert!(
            p.detect("Tokens: 4.2k sent, 1.1k received. Cost: $0.02 message, $0.05 session.")
                .is_none()
        );
    }
}
