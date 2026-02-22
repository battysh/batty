//! Human review gate primitives.
//!
//! Produces a standardized review packet artifact and captures explicit
//! merge/rework/escalate decisions for audit logging.

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewPacket {
    pub path: PathBuf,
    pub diff_command: String,
    pub summary_path: Option<PathBuf>,
    pub statements: Vec<PathBuf>,
    pub execution_log_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewDecision {
    Merge,
    Rework { feedback: String },
    Escalate { feedback: String },
}

impl ReviewDecision {
    pub fn label(&self) -> &'static str {
        match self {
            ReviewDecision::Merge => "merge",
            ReviewDecision::Rework { .. } => "rework",
            ReviewDecision::Escalate { .. } => "escalate",
        }
    }

    pub fn feedback(&self) -> Option<&str> {
        match self {
            ReviewDecision::Merge => None,
            ReviewDecision::Rework { feedback } | ReviewDecision::Escalate { feedback } => {
                Some(feedback.as_str())
            }
        }
    }
}

/// Create/update `review-packet.md` with standard review artifacts.
pub fn generate_review_packet(
    phase: &str,
    execution_root: &Path,
    execution_log_path: &Path,
    branch: &str,
    base_branch: &str,
) -> Result<ReviewPacket> {
    let tasks_dir = crate::paths::resolve_kanban_root(execution_root)
        .join(phase)
        .join("tasks");
    let tasks = crate::task::load_tasks_from_dir(&tasks_dir)
        .with_context(|| format!("failed to load tasks from {}", tasks_dir.display()))?;

    let statements = tasks
        .iter()
        .map(|task| task.source_path.clone())
        .collect::<Vec<_>>();
    let summary_path = locate_phase_summary(execution_root, phase);
    let diff_command = format!("git diff {base_branch}...{branch}");
    let packet_path = execution_root.join("review-packet.md");

    let mut body = String::new();
    body.push_str(&format!("# Review Packet: {phase}\n\n"));
    body.push_str("## Artifacts\n");
    body.push_str(&format!("- Diff against base: `{diff_command}`\n"));
    match &summary_path {
        Some(path) => body.push_str(&format!("- Phase summary: `{}`\n", path.display())),
        None => body.push_str("- Phase summary: `(missing phase-summary.md)`\n"),
    }
    if statements.is_empty() {
        body.push_str("- Task statements of work: `(no task files found)`\n");
    } else {
        for statement in &statements {
            body.push_str(&format!("- Statement of work: `{}`\n", statement.display()));
        }
    }
    body.push_str(&format!(
        "- Execution log: `{}`\n",
        execution_log_path.display()
    ));
    body.push('\n');
    body.push_str("## Review Decision\n");
    body.push_str("Choose exactly one:\n");
    body.push_str("- `merge`\n");
    body.push_str("- `rework` (with required feedback)\n");
    body.push_str("- `escalate` (with required rationale)\n");

    std::fs::write(&packet_path, body)
        .with_context(|| format!("failed to write review packet {}", packet_path.display()))?;

    Ok(ReviewPacket {
        path: packet_path,
        diff_command,
        summary_path,
        statements,
        execution_log_path: execution_log_path.to_path_buf(),
    })
}

/// Capture explicit human decision.
///
/// - Accepts env override `BATTY_REVIEW_DECISION` (`merge|rework|escalate`)
/// - Uses `BATTY_REVIEW_FEEDBACK` for rework/escalate when env override is used
/// - Falls back to interactive prompt when stdin is a TTY
pub fn capture_review_decision() -> Result<ReviewDecision> {
    if let Ok(raw) = std::env::var("BATTY_REVIEW_DECISION") {
        let feedback = std::env::var("BATTY_REVIEW_FEEDBACK").ok();
        return parse_review_decision(raw.as_str(), feedback.as_deref());
    }

    if !io::stdin().is_terminal() {
        bail!(
            "human review decision required but stdin is not interactive; set BATTY_REVIEW_DECISION=merge|rework|escalate"
        );
    }

    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let stdout = io::stdout();
    let mut writer = stdout.lock();
    prompt_review_decision(&mut reader, &mut writer)
}

pub fn parse_review_decision(raw: &str, feedback: Option<&str>) -> Result<ReviewDecision> {
    let decision = raw.trim().to_ascii_lowercase();
    match decision.as_str() {
        "merge" => Ok(ReviewDecision::Merge),
        "rework" => {
            let feedback = required_feedback(feedback)?;
            Ok(ReviewDecision::Rework { feedback })
        }
        "escalate" => {
            let feedback = required_feedback(feedback)?;
            Ok(ReviewDecision::Escalate { feedback })
        }
        other => bail!("unknown review decision '{other}' (expected merge|rework|escalate)"),
    }
}

fn prompt_review_decision<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
) -> Result<ReviewDecision> {
    loop {
        writer.write_all(
            b"[batty] review decision required (merge | rework | escalate)\ndecision> ",
        )?;
        writer.flush()?;

        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            bail!("no review decision provided on stdin");
        }

        let decision = line.trim().to_ascii_lowercase();
        match decision.as_str() {
            "merge" => return Ok(ReviewDecision::Merge),
            "rework" | "escalate" => {
                writer.write_all(b"feedback> ")?;
                writer.flush()?;
                let mut feedback = String::new();
                let bytes = reader.read_line(&mut feedback)?;
                if bytes == 0 {
                    bail!("missing feedback for '{decision}' review decision");
                }
                let feedback = feedback.trim();
                if feedback.is_empty() {
                    writer.write_all(
                        b"[batty] feedback is required for rework/escalate decisions.\n",
                    )?;
                    writer.flush()?;
                    continue;
                }
                return parse_review_decision(&decision, Some(feedback));
            }
            _ => {
                writer
                    .write_all(b"[batty] invalid decision. Enter merge, rework, or escalate.\n")?;
                writer.flush()?;
            }
        }
    }
}

fn required_feedback(feedback: Option<&str>) -> Result<String> {
    let Some(value) = feedback.map(str::trim) else {
        bail!("feedback is required for rework/escalate decisions");
    };
    if value.is_empty() {
        bail!("feedback is required for rework/escalate decisions");
    }
    Ok(value.to_string())
}

fn locate_phase_summary(execution_root: &Path, phase: &str) -> Option<PathBuf> {
    [
        execution_root.join("phase-summary.md"),
        crate::paths::resolve_kanban_root(execution_root)
            .join(phase)
            .join("phase-summary.md"),
    ]
    .into_iter()
    .find(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_review_decision_accepts_merge() {
        let decision = parse_review_decision("merge", None).unwrap();
        assert_eq!(decision, ReviewDecision::Merge);
        assert_eq!(decision.label(), "merge");
        assert!(decision.feedback().is_none());
    }

    #[test]
    fn parse_review_decision_requires_feedback_for_rework_and_escalate() {
        assert!(parse_review_decision("rework", None).is_err());
        assert!(parse_review_decision("escalate", Some("")).is_err());

        let rework = parse_review_decision("rework", Some("fix flaky test")).unwrap();
        assert_eq!(
            rework,
            ReviewDecision::Rework {
                feedback: "fix flaky test".to_string()
            }
        );
        assert_eq!(rework.label(), "rework");
        assert_eq!(rework.feedback(), Some("fix flaky test"));
    }

    #[test]
    fn prompt_review_decision_loops_until_valid() {
        let input = b"invalid\nmerge\n";
        let mut reader = std::io::Cursor::new(input.as_slice());
        let mut output = Vec::new();
        let decision = prompt_review_decision(&mut reader, &mut output).unwrap();
        assert_eq!(decision, ReviewDecision::Merge);
        let transcript = String::from_utf8(output).unwrap();
        assert!(transcript.contains("invalid decision"));
        assert!(transcript.contains("decision>"));
    }

    #[test]
    fn generate_review_packet_writes_expected_sections() {
        let tmp = tempfile::tempdir().unwrap();
        let phase = "phase-3";
        let tasks_dir = tmp.path().join("kanban").join(phase).join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join("001-sample.md"),
            concat!(
                "---\n",
                "id: 1\n",
                "title: sample\n",
                "status: done\n",
                "priority: high\n",
                "tags: []\n",
                "depends_on: []\n",
                "class: standard\n",
                "---\n\n",
                "Task body.\n"
            ),
        )
        .unwrap();
        std::fs::write(tmp.path().join("phase-summary.md"), "summary").unwrap();
        let log_path = tmp
            .path()
            .join(".batty")
            .join("logs")
            .join("execution.jsonl");
        std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        std::fs::write(&log_path, "{}\n").unwrap();

        let packet =
            generate_review_packet(phase, tmp.path(), &log_path, "phase-3-run-001", "main")
                .unwrap();
        let content = std::fs::read_to_string(&packet.path).unwrap();

        assert!(content.contains("# Review Packet: phase-3"));
        assert!(content.contains("git diff main...phase-3-run-001"));
        assert!(content.contains("phase-summary.md"));
        assert!(content.contains("Statement of work"));
        assert!(content.contains("Execution log"));
        assert!(content.contains("Review Decision"));
    }
}
