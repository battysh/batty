//! Tier 2: on-demand supervisor agent.
//!
//! When Tier 1 can't pattern-match a prompt, Tier 2 makes a single API call
//! to a supervisor agent. The agent receives a structured context snapshot
//! (project docs + event buffer + the question) and returns an answer.
//!
//! This is not a persistent session — one call per question, stateless.
//! The default implementation shells out to `claude -p` for simplicity and
//! composability. The supervisor command is configurable in `.batty/config.toml`.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

/// Configuration for the Tier 2 supervisor agent.
#[derive(Debug, Clone)]
pub struct Tier2Config {
    /// The command to invoke the supervisor agent.
    /// Default: "claude"
    pub program: String,
    /// Arguments template. The context prompt is appended as the last arg.
    /// Default: ["-p", "--output-format", "text"]
    pub args: Vec<String>,
    /// Maximum time to wait for the supervisor response.
    pub timeout: Duration,
    /// System prompt (project docs) to prepend for context.
    pub system_prompt: Option<String>,
    /// Whether to emit detailed supervisor request/response traces.
    pub trace_io: bool,
}

impl Default for Tier2Config {
    fn default() -> Self {
        Self {
            program: "claude".to_string(),
            args: vec![
                "-p".to_string(),
                "--output-format".to_string(),
                "text".to_string(),
            ],
            timeout: Duration::from_secs(60),
            system_prompt: None,
            trace_io: true,
        }
    }
}

/// Result of a Tier 2 supervisor call.
#[derive(Debug, Clone)]
pub enum Tier2Result {
    /// Supervisor provided an answer to inject.
    Answer { response: String },
    /// Supervisor decided to escalate to human.
    Escalate { reason: String },
    /// Supervisor call failed (timeout, error, etc.).
    Failed { error: String },
}

/// Normalize supervisor output into a safe, injectable terminal response.
///
/// The supervisor is instructed to return only the exact input, but models
/// sometimes add explanation. This function extracts a concise response and
/// handles common "press enter" variants.
fn normalize_supervisor_response(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        anyhow::bail!("supervisor returned empty response");
    }

    let lower = trimmed.to_lowercase();
    if lower.contains("press enter")
        || lower.contains("just press enter")
        || lower.contains("empty enter")
        || lower.contains("empty input")
        || lower.contains("empty string")
    {
        return Ok(String::new());
    }

    // Try structured markers first.
    for line in trimmed.lines().map(str::trim).filter(|l| !l.is_empty()) {
        for prefix in [
            "**Answer to send:**",
            "Answer to send:",
            "The exact input to send:",
            "Exact input:",
            "Input:",
            "Response:",
        ] {
            if let Some(rest) = line.strip_prefix(prefix) {
                let candidate = rest.trim().trim_matches('`').trim_matches('"');
                if candidate.is_empty() {
                    return Ok(String::new());
                }
                if candidate.eq_ignore_ascii_case("enter")
                    || candidate.eq_ignore_ascii_case("press enter")
                {
                    return Ok(String::new());
                }
                return Ok(candidate.to_string());
            }
        }
    }

    // Fall back to first non-empty line.
    let first = trimmed
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .trim_matches('`')
        .trim_matches('"')
        .to_string();

    if first.is_empty() {
        return Ok(String::new());
    }

    // Guardrail: don't inject long prose into interactive prompts.
    if first.len() > 120 {
        anyhow::bail!("supervisor response too long to inject safely");
    }

    Ok(first)
}

/// Compose the context prompt for the supervisor.
///
/// The prompt includes:
/// 1. A system-level description of the supervisor's role
/// 2. The event buffer summary (what the executor has done recently)
/// 3. The detected question that needs an answer
pub fn compose_context(event_summary: &str, question: &str, system_prompt: Option<&str>) -> String {
    let mut prompt = String::new();

    // System-level role description
    prompt.push_str(
        "You are a supervisor agent for Batty, a hierarchical agent command system. \
         An executor agent (coding AI) is working on a task and has asked a question \
         that couldn't be auto-answered by pattern matching.\n\n\
         Your job: analyze the context and provide a concise, direct answer that the \
         executor can use to continue its work. If you genuinely cannot determine the \
         right answer, respond with exactly: ESCALATE: <reason>\n\n",
    );

    // Project docs (if provided)
    if let Some(sys) = system_prompt {
        prompt.push_str("## Project context\n\n");
        prompt.push_str(sys);
        prompt.push_str("\n\n");
    }

    // Event buffer
    prompt.push_str("## Recent executor activity\n\n");
    prompt.push_str(event_summary);
    prompt.push_str("\n\n");

    // The question
    prompt.push_str("## Question from executor\n\n");
    prompt.push_str(question);
    prompt.push_str("\n\n");

    prompt.push_str(
        "Respond with ONLY the answer to type into the executor's terminal. \
         Keep it brief — usually one word or one line. \
         If you cannot determine the right answer, respond with: ESCALATE: <reason>",
    );

    prompt
}

/// Call the Tier 2 supervisor agent.
///
/// Shells out to the configured command with the composed context prompt.
/// Parses the response for escalation signals.
pub fn call_supervisor(config: &Tier2Config, context_prompt: &str) -> Result<Tier2Result> {
    info!("calling Tier 2 supervisor agent");
    debug!(prompt_len = context_prompt.len(), "supervisor context");

    let mut cmd = Command::new(&config.program);
    for arg in &config.args {
        cmd.arg(arg);
    }
    cmd.arg(context_prompt);

    // Set a timeout by using the output() method (blocking)
    // For actual timeout, we'd need to spawn and wait with timeout
    let output = cmd
        .output()
        .with_context(|| format!("failed to run supervisor command: {}", config.program))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(stderr = %stderr, "supervisor command failed");
        return Ok(Tier2Result::Failed {
            error: format!("supervisor exited with error: {stderr}"),
        });
    }

    let response = String::from_utf8_lossy(&output.stdout).trim().to_string();

    // Check for escalation signal
    if response.starts_with("ESCALATE:") {
        let reason = response
            .strip_prefix("ESCALATE:")
            .unwrap_or("")
            .trim()
            .to_string();
        return Ok(Tier2Result::Escalate { reason });
    }

    let normalized = match normalize_supervisor_response(&response) {
        Ok(v) => v,
        Err(e) => {
            return Ok(Tier2Result::Failed {
                error: format!("supervisor response not safely injectable: {e}"),
            });
        }
    };

    info!(response = %normalized, "supervisor answered");
    Ok(Tier2Result::Answer {
        response: normalized,
    })
}

/// Load project docs for the system prompt.
///
/// Reads key files from the project root to build cached context:
/// - CLAUDE.md (if exists)
/// - planning/architecture.md (if exists)
pub fn load_project_docs(project_root: &Path) -> String {
    let mut docs = String::new();

    let files = ["CLAUDE.md", "planning/architecture.md"];

    for filename in &files {
        let path = project_root.join(filename);
        if let Ok(content) = std::fs::read_to_string(&path) {
            docs.push_str(&format!("### {filename}\n\n"));
            // Truncate very long files to avoid overwhelming the context
            if content.len() > 4096 {
                docs.push_str(&content[..4096]);
                docs.push_str("\n...(truncated)\n");
            } else {
                docs.push_str(&content);
            }
            docs.push_str("\n\n");
        }
    }

    if docs.is_empty() {
        "(no project documentation found)".to_string()
    } else {
        docs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_context_includes_all_sections() {
        let ctx = compose_context("→ task #3 started\n✓ test passed", "What model?", None);

        assert!(ctx.contains("supervisor agent"));
        assert!(ctx.contains("→ task #3 started"));
        assert!(ctx.contains("What model?"));
        assert!(ctx.contains("ESCALATE"));
    }

    #[test]
    fn compose_context_with_system_prompt() {
        let ctx = compose_context("events", "question?", Some("Project uses Rust"));

        assert!(ctx.contains("Project uses Rust"));
        assert!(ctx.contains("Project context"));
    }

    #[test]
    fn compose_context_without_system_prompt() {
        let ctx = compose_context("events", "question?", None);

        assert!(!ctx.contains("Project context"));
    }

    #[test]
    fn default_config() {
        let config = Tier2Config::default();
        assert_eq!(config.program, "claude");
        assert!(config.args.contains(&"-p".to_string()));
        assert_eq!(config.timeout, Duration::from_secs(60));
        assert!(config.trace_io);
    }

    #[test]
    fn call_supervisor_with_echo() {
        // Use echo as a mock supervisor that just echoes the response
        let config = Tier2Config {
            program: "echo".to_string(),
            args: vec![],
            timeout: Duration::from_secs(5),
            system_prompt: None,
            trace_io: true,
        };

        let result = call_supervisor(&config, "test prompt").unwrap();
        match result {
            Tier2Result::Answer { response } => {
                assert!(response.contains("test prompt"));
            }
            other => panic!("expected Answer, got: {other:?}"),
        }
    }

    #[test]
    fn call_supervisor_escalation() {
        // Use printf to return exact ESCALATE response
        let config = Tier2Config {
            program: "printf".to_string(),
            args: vec!["ESCALATE: too ambiguous".to_string()],
            timeout: Duration::from_secs(5),
            system_prompt: None,
            trace_io: true,
        };

        let result = call_supervisor(&config, "ignored").unwrap();
        match result {
            Tier2Result::Escalate { reason } => {
                assert!(reason.contains("ambiguous"));
            }
            other => panic!("expected Escalate, got: {other:?}"),
        }
    }

    #[test]
    fn normalize_press_enter_sentence_to_empty() {
        let out = normalize_supervisor_response(
            "Press Enter. The executor is waiting at a generic prompt.",
        )
        .unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn normalize_marker_line_extracts_value() {
        let out =
            normalize_supervisor_response("Some context\n**Answer to send:** y\nMore text ignored")
                .unwrap();
        assert_eq!(out, "y");
    }

    #[test]
    fn normalize_rejects_long_prose() {
        let long = "This is a very long paragraph that explains what to do instead of returning a direct terminal input and it should not be injected as-is into the executor prompt because that is unsafe.";
        let err = normalize_supervisor_response(long).unwrap_err().to_string();
        assert!(err.contains("too long"));
    }

    #[test]
    fn call_supervisor_failing_command() {
        let config = Tier2Config {
            program: "false".to_string(),
            args: vec![],
            timeout: Duration::from_secs(5),
            system_prompt: None,
            trace_io: true,
        };

        let result = call_supervisor(&config, "test").unwrap();
        assert!(matches!(result, Tier2Result::Failed { .. }));
    }

    #[test]
    fn load_project_docs_from_project() {
        // Use the actual project root (this test runs from the project)
        let docs = load_project_docs(Path::new("."));
        // Should find CLAUDE.md at minimum
        if Path::new("CLAUDE.md").exists() {
            assert!(docs.contains("CLAUDE.md"));
        }
    }

    #[test]
    fn load_project_docs_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let docs = load_project_docs(tmp.path());
        assert!(docs.contains("no project documentation"));
    }

    #[test]
    fn tier2_result_variants() {
        let r = Tier2Result::Answer {
            response: "y".to_string(),
        };
        assert!(format!("{r:?}").contains("Answer"));

        let r = Tier2Result::Escalate {
            reason: "unclear".to_string(),
        };
        assert!(format!("{r:?}").contains("Escalate"));

        let r = Tier2Result::Failed {
            error: "timeout".to_string(),
        };
        assert!(format!("{r:?}").contains("Failed"));
    }
}
