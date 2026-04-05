//! PARITY.md parsing and reporting.

use std::fmt;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const PARITY_FILE: &str = "PARITY.md";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParityReport {
    pub metadata: ParityMetadata,
    pub rows: Vec<ParityRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ParityMetadata {
    pub project: String,
    pub target: String,
    pub source_platform: String,
    pub target_language: String,
    pub last_verified: String,
    #[serde(default)]
    pub overall_parity: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParityRow {
    pub behavior: String,
    pub spec: ParityStatus,
    pub test: ParityStatus,
    pub implementation: ParityStatus,
    pub verified: VerificationStatus,
    pub notes: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParityStatus {
    NotStarted,
    Draft,
    Complete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationStatus {
    NotStarted,
    Pass,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParitySummary {
    pub total_behaviors: usize,
    pub spec_complete: usize,
    pub tests_complete: usize,
    pub implementation_complete: usize,
    pub verified_pass: usize,
    pub verified_fail: usize,
    pub overall_parity_pct: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GapTaskSpec {
    pub title: String,
    pub body: String,
}

impl ParityReport {
    pub fn load(project_root: &Path) -> Result<Self> {
        let path = project_root.join(PARITY_FILE);
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        Self::parse(&content).with_context(|| format!("failed to parse {}", path.display()))
    }

    pub fn parse(input: &str) -> Result<Self> {
        let (frontmatter, body) = split_frontmatter(input)?;
        let metadata: ParityMetadata =
            serde_yaml::from_str(frontmatter).context("failed to parse PARITY.md frontmatter")?;
        let rows = parse_table_rows(body)?;
        if rows.is_empty() {
            bail!("PARITY.md must include at least one behavior row");
        }
        Ok(Self { metadata, rows })
    }

    pub fn summary(&self) -> ParitySummary {
        let total_behaviors = self.rows.len();
        let spec_complete = self
            .rows
            .iter()
            .filter(|row| row.spec == ParityStatus::Complete)
            .count();
        let tests_complete = self
            .rows
            .iter()
            .filter(|row| row.test == ParityStatus::Complete)
            .count();
        let implementation_complete = self
            .rows
            .iter()
            .filter(|row| row.implementation == ParityStatus::Complete)
            .count();
        let verified_pass = self
            .rows
            .iter()
            .filter(|row| row.verified == VerificationStatus::Pass)
            .count();
        let verified_fail = self
            .rows
            .iter()
            .filter(|row| row.verified == VerificationStatus::Fail)
            .count();
        let overall_parity_pct = if total_behaviors == 0 {
            0
        } else {
            (verified_pass * 100) / total_behaviors
        };

        ParitySummary {
            total_behaviors,
            spec_complete,
            tests_complete,
            implementation_complete,
            verified_pass,
            verified_fail,
            overall_parity_pct,
        }
    }

    pub fn gaps(&self) -> Vec<&ParityRow> {
        self.rows
            .iter()
            .filter(|row| {
                row.spec != ParityStatus::NotStarted
                    && (row.test == ParityStatus::NotStarted
                        || row.implementation == ParityStatus::NotStarted)
            })
            .collect()
    }
}

impl fmt::Display for ParityStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::NotStarted => "--",
            Self::Draft => "draft",
            Self::Complete => "complete",
        };
        f.write_str(text)
    }
}

impl fmt::Display for VerificationStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::NotStarted => "--",
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
        };
        f.write_str(text)
    }
}

pub fn show_parity(project_root: &Path, detail: bool, gaps_only: bool) -> Result<()> {
    let report = ParityReport::load(project_root)?;
    let summary = report.summary();

    println!("Project: {}", report.metadata.project);
    println!("Target: {}", report.metadata.target);
    println!("Source platform: {}", report.metadata.source_platform);
    println!("Target language: {}", report.metadata.target_language);
    println!("Last verified: {}", report.metadata.last_verified);
    println!("Total behaviors: {}", summary.total_behaviors);
    println!("Spec complete: {}", summary.spec_complete);
    println!("Tests complete: {}", summary.tests_complete);
    println!(
        "Implementation complete: {}",
        summary.implementation_complete
    );
    println!("Verified PASS: {}", summary.verified_pass);
    println!("Verified FAIL: {}", summary.verified_fail);
    println!("Overall parity: {}%", summary.overall_parity_pct);

    if detail || gaps_only {
        println!();
        print_parity_table(&report, gaps_only);
    }

    Ok(())
}

pub fn sync_gap_tasks(project_root: &Path) -> Result<Vec<String>> {
    let report = match ParityReport::load(project_root) {
        Ok(report) => report,
        Err(_) => return Ok(Vec::new()),
    };

    let board_dir = crate::team::team_config_dir(project_root).join("board");
    let tasks_dir = board_dir.join("tasks");
    let existing_tasks = if tasks_dir.is_dir() {
        crate::task::load_tasks_from_dir(&tasks_dir)?
    } else {
        Vec::new()
    };

    let specs = report.missing_gap_task_specs(&existing_tasks);
    let mut created = Vec::new();
    for spec in specs {
        crate::team::board_cmd::create_task(
            &board_dir,
            &spec.title,
            &spec.body,
            Some("medium"),
            Some("parity,clean-room"),
            None,
        )
        .with_context(|| format!("failed to create board task '{}'", spec.title))?;
        created.push(spec.title);
    }
    Ok(created)
}

fn print_parity_table(report: &ParityReport, gaps_only: bool) {
    let rows: Vec<&ParityRow> = if gaps_only {
        report.gaps()
    } else {
        report.rows.iter().collect()
    };

    if rows.is_empty() {
        println!("No parity gaps found.");
        return;
    }

    println!(
        "{:<28} {:<10} {:<10} {:<16} {:<10} NOTES",
        "BEHAVIOR", "SPEC", "TEST", "IMPLEMENTATION", "VERIFIED"
    );
    println!("{}", "-".repeat(96));
    for row in rows {
        println!(
            "{:<28} {:<10} {:<10} {:<16} {:<10} {}",
            truncate(&row.behavior, 28),
            row.spec,
            row.test,
            row.implementation,
            row.verified,
            row.notes
        );
    }
}

fn truncate(input: &str, width: usize) -> String {
    let count = input.chars().count();
    if count <= width {
        return input.to_string();
    }
    let mut out: String = input.chars().take(width.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

fn split_frontmatter(input: &str) -> Result<(&str, &str)> {
    let mut lines = input.lines();
    if lines.next() != Some("---") {
        bail!("PARITY.md must start with YAML frontmatter");
    }

    let after_start = input
        .find("\n")
        .map(|idx| idx + 1)
        .context("PARITY.md frontmatter is malformed")?;
    let rest = &input[after_start..];
    let end_offset = rest
        .find("\n---")
        .context("PARITY.md frontmatter is missing closing delimiter")?;
    let frontmatter = &rest[..end_offset];
    let body_start = after_start + end_offset + "\n---".len();
    let body = input
        .get(body_start..)
        .unwrap_or("")
        .trim_start_matches('\n')
        .trim();
    Ok((frontmatter, body))
}

fn parse_table_rows(body: &str) -> Result<Vec<ParityRow>> {
    let table_lines: Vec<&str> = body
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('|'))
        .collect();

    if table_lines.len() < 3 {
        bail!("PARITY.md table must include a header, separator, and at least one row");
    }

    let header = split_table_line(table_lines[0]);
    let expected = [
        "Behavior",
        "Spec",
        "Test",
        "Implementation",
        "Verified",
        "Notes",
    ];
    if header != expected {
        bail!("PARITY.md table columns must be: {}", expected.join(" | "));
    }

    let mut rows = Vec::new();
    for line in table_lines.iter().skip(2) {
        let cols = split_table_line(line);
        if cols.len() != 6 {
            bail!("PARITY.md rows must have exactly 6 columns");
        }
        rows.push(ParityRow {
            behavior: cols[0].to_string(),
            spec: ParityStatus::parse(cols[1])?,
            test: ParityStatus::parse(cols[2])?,
            implementation: ParityStatus::parse(cols[3])?,
            verified: VerificationStatus::parse(cols[4])?,
            notes: cols[5].to_string(),
        });
    }
    Ok(rows)
}

fn split_table_line(line: &str) -> Vec<&str> {
    line.trim_matches('|').split('|').map(str::trim).collect()
}

impl ParityStatus {
    fn parse(input: &str) -> Result<Self> {
        match input {
            "--" => Ok(Self::NotStarted),
            "draft" => Ok(Self::Draft),
            "complete" => Ok(Self::Complete),
            other => bail!("invalid parity status '{other}'"),
        }
    }
}

impl VerificationStatus {
    fn parse(input: &str) -> Result<Self> {
        match input {
            "--" => Ok(Self::NotStarted),
            "PASS" => Ok(Self::Pass),
            "FAIL" => Ok(Self::Fail),
            other => bail!("invalid verification status '{other}'"),
        }
    }
}

impl ParityReport {
    fn missing_gap_task_specs(&self, existing_tasks: &[crate::task::Task]) -> Vec<GapTaskSpec> {
        let existing_titles: std::collections::HashSet<&str> = existing_tasks
            .iter()
            .map(|task| task.title.as_str())
            .collect();

        self.gaps()
            .into_iter()
            .map(GapTaskSpec::from_row)
            .filter(|spec| !existing_titles.contains(spec.title.as_str()))
            .collect()
    }
}

impl GapTaskSpec {
    fn from_row(row: &ParityRow) -> Self {
        let title = format!("Parity gap: {}", row.behavior);
        let body = format!(
            "Close the clean-room parity gap for `{}`.\n\nCurrent parity row:\n- Spec: {}\n- Test: {}\n- Implementation: {}\n- Verified: {}\n- Notes: {}\n",
            row.behavior,
            row.spec,
            row.test,
            row.implementation,
            row.verified,
            if row.notes.is_empty() {
                "(none)"
            } else {
                row.notes.as_str()
            }
        );
        Self { title, body }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"---
project: manic-miner
target: original-binary.z80
source_platform: zx-spectrum-z80
target_language: rust
last_verified: 2026-04-05
overall_parity: 73%
---

| Behavior | Spec | Test | Implementation | Verified | Notes |
| --- | --- | --- | --- | --- | --- |
| Input handling | complete | complete | complete | PASS | parity matched |
| Enemy AI | complete | -- | draft | -- | tests pending |
| Sound timing | draft | -- | -- | FAIL | timing drift |
"#;

    #[test]
    fn parse_report_extracts_frontmatter_and_rows() {
        let report = ParityReport::parse(SAMPLE).unwrap();
        assert_eq!(report.metadata.project, "manic-miner");
        assert_eq!(report.rows.len(), 3);
        assert_eq!(report.rows[1].behavior, "Enemy AI");
        assert_eq!(report.rows[1].test, ParityStatus::NotStarted);
        assert_eq!(report.rows[2].verified, VerificationStatus::Fail);
    }

    #[test]
    fn summary_counts_completed_and_verified_rows() {
        let report = ParityReport::parse(SAMPLE).unwrap();
        let summary = report.summary();
        assert_eq!(summary.total_behaviors, 3);
        assert_eq!(summary.spec_complete, 2);
        assert_eq!(summary.tests_complete, 1);
        assert_eq!(summary.implementation_complete, 1);
        assert_eq!(summary.verified_pass, 1);
        assert_eq!(summary.verified_fail, 1);
        assert_eq!(summary.overall_parity_pct, 33);
    }

    #[test]
    fn gaps_only_returns_specified_rows_missing_tests_or_implementation() {
        let report = ParityReport::parse(SAMPLE).unwrap();
        let gaps = report.gaps();
        assert_eq!(gaps.len(), 2);
        assert_eq!(gaps[0].behavior, "Enemy AI");
        assert_eq!(gaps[1].behavior, "Sound timing");
    }

    #[test]
    fn parse_rejects_invalid_status_values() {
        let bad = SAMPLE.replace(
            "| Enemy AI | complete | -- | draft | -- | tests pending |",
            "| Enemy AI | started | -- | draft | -- | tests pending |",
        );
        let err = ParityReport::parse(&bad).unwrap_err().to_string();
        assert!(err.contains("invalid parity status"));
    }

    #[test]
    fn load_reads_project_parity_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(PARITY_FILE), SAMPLE).unwrap();
        let report = ParityReport::load(tmp.path()).unwrap();
        assert_eq!(report.metadata.project, "manic-miner");
    }

    #[test]
    fn missing_gap_task_specs_skips_existing_titles() {
        let report = ParityReport::parse(SAMPLE).unwrap();
        let existing = vec![crate::task::Task {
            id: 1,
            title: "Parity gap: Enemy AI".to_string(),
            status: "todo".to_string(),
            priority: "medium".to_string(),
            claimed_by: None,
            blocked: None,
            tags: Vec::new(),
            depends_on: Vec::new(),
            review_owner: None,
            blocked_on: None,
            worktree_path: None,
            branch: None,
            commit: None,
            artifacts: Vec::new(),
            next_action: None,
            scheduled_for: None,
            cron_schedule: None,
            cron_last_run: None,
            completed: None,
            description: String::new(),
            batty_config: None,
            source_path: std::path::PathBuf::new(),
        }];

        let specs = report.missing_gap_task_specs(&existing);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].title, "Parity gap: Sound timing");
        assert!(specs[0].body.contains("Implementation: --"));
    }
}
