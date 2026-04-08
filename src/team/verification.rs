//! Clean-room equivalence verification driven by `PARITY.md`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::equivalence::{
    CommandBackend, DiffReport, InputSequence, compare_outputs, execute_test_run,
};
use super::events::{EventSink, TeamEvent};
use super::parity::{ParityReport, ParitySummary, VerificationStatus};

const MANIFEST_PATH: &str = ".batty/verification.yml";
const REPORTS_DIR: &str = ".batty/reports/verification";
const LATEST_REPORT: &str = "latest.md";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationPhase {
    Executing,
    Verifying,
    Fixing,
    Complete,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    CommitsAhead,
    FilesChanged,
    CodeFilesChanged,
    TestsPassed,
    TestsFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationEvidence {
    pub kind: EvidenceKind,
    pub detail: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationState {
    pub phase: VerificationPhase,
    pub iteration: u32,
    pub max_iterations: u32,
    pub last_test_output: Option<String>,
    pub last_test_passed: bool,
    pub evidence: Vec<VerificationEvidence>,
}

impl VerificationState {
    pub fn new(max_iterations: u32) -> Self {
        Self {
            phase: VerificationPhase::Executing,
            iteration: 0,
            max_iterations: max_iterations.max(1),
            last_test_output: None,
            last_test_passed: false,
            evidence: Vec::new(),
        }
    }

    pub fn transition(&mut self, phase: VerificationPhase) -> VerificationPhase {
        let previous = self.phase.clone();
        self.phase = phase;
        previous
    }

    pub fn begin_iteration(&mut self) {
        self.iteration = self.iteration.saturating_add(1);
    }

    pub fn record_evidence(&mut self, kind: EvidenceKind, detail: impl Into<String>) {
        self.evidence.push(VerificationEvidence {
            kind,
            detail: detail.into(),
            timestamp: chrono::Utc::now(),
        });
    }

    pub fn reached_max_iterations(&self) -> bool {
        self.iteration >= self.max_iterations
    }

    pub fn clear_evidence(&mut self) {
        self.evidence.clear();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyStatus {
    Skipped,
    Passed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationOutcome {
    pub status: VerifyStatus,
    pub report_path: Option<PathBuf>,
    pub summary: Option<ParitySummary>,
    pub regressions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BehaviorRun {
    behavior: String,
    previous_status: VerificationStatus,
    report: DiffReport,
}

#[derive(Debug, Deserialize)]
struct VerificationManifest {
    behaviors: Vec<VerificationCase>,
}

#[derive(Debug, Deserialize)]
struct VerificationCase {
    behavior: String,
    baseline: String,
    candidate: String,
    #[serde(default)]
    inputs: Vec<String>,
}

pub fn cmd_verify(project_root: &Path) -> Result<()> {
    let outcome = verify_project(project_root, project_root)?;
    let report_path = outcome
        .report_path
        .as_deref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "(none)".to_string());
    match outcome.status {
        VerifyStatus::Skipped => {
            println!("No PARITY.md found. Verification skipped.");
        }
        VerifyStatus::Passed => {
            if let Some(summary) = outcome.summary.as_ref() {
                println!(
                    "Verification passed: {}/{} behaviors verified. Report: {}",
                    summary.verified_pass, summary.total_behaviors, report_path
                );
            }
        }
        VerifyStatus::Failed => {
            if outcome.regressions.is_empty() {
                bail!("verification failed. Report: {report_path}");
            }
            bail!(
                "verification regressions: {}. Report: {}",
                outcome.regressions.join(", "),
                report_path
            );
        }
    }

    Ok(())
}

pub fn verify_project(project_root: &Path, artifact_root: &Path) -> Result<VerificationOutcome> {
    let parity_path = project_root.join("PARITY.md");
    if !parity_path.exists() {
        return Ok(VerificationOutcome {
            status: VerifyStatus::Skipped,
            report_path: None,
            summary: None,
            regressions: Vec::new(),
        });
    }

    let manifest = load_manifest(project_root)?;
    let mut report = ParityReport::load(project_root)?;
    let manifest_by_behavior = manifest_by_behavior(&manifest)?;

    let parity_behaviors: BTreeSet<String> =
        report.rows.iter().map(|row| row.behavior.clone()).collect();
    let manifest_behaviors: BTreeSet<String> = manifest
        .behaviors
        .iter()
        .map(|case| case.behavior.clone())
        .collect();

    let missing_behaviors: Vec<String> = parity_behaviors
        .difference(&manifest_behaviors)
        .cloned()
        .collect();
    if !missing_behaviors.is_empty() {
        bail!(
            "verification manifest missing behaviors: {}",
            missing_behaviors.join(", ")
        );
    }

    let extra_behaviors: Vec<String> = manifest_behaviors
        .difference(&parity_behaviors)
        .cloned()
        .collect();
    if !extra_behaviors.is_empty() {
        bail!(
            "verification manifest has behaviors not present in PARITY.md: {}",
            extra_behaviors.join(", ")
        );
    }

    let mut runs = Vec::new();
    let backend = CommandBackend;
    for row in report.rows.clone() {
        let case = manifest_by_behavior
            .get(row.behavior.as_str())
            .context("verification manifest lookup failed")?;
        let inputs = InputSequence::new(case.inputs.iter().cloned());
        let baseline = project_root.join(&case.baseline);
        let candidate = project_root.join(&case.candidate);
        let expected = execute_test_run(&backend, "baseline", &baseline, inputs.clone())
            .with_context(|| format!("verification baseline failed for `{}`", row.behavior))?;
        let actual = execute_test_run(&backend, "candidate", &candidate, inputs)
            .with_context(|| format!("verification candidate failed for `{}`", row.behavior))?;
        let diff = compare_outputs(&expected.outputs, &actual.outputs);

        let status = if diff.passed() {
            VerificationStatus::Pass
        } else {
            VerificationStatus::Fail
        };
        report.update_verification(&row.behavior, status, &diff.summary())?;
        runs.push(BehaviorRun {
            behavior: row.behavior,
            previous_status: row.verified,
            report: diff,
        });
    }

    std::fs::write(&parity_path, report.render())
        .with_context(|| format!("failed to write {}", parity_path.display()))?;

    let summary = report.summary();
    let regressions: Vec<String> = runs
        .iter()
        .filter(|run| run.previous_status == VerificationStatus::Pass && !run.report.passed())
        .map(|run| run.behavior.clone())
        .collect();
    let report_path = write_report(artifact_root, &summary, &runs, &regressions)?;
    record_summary_event(artifact_root, &summary)?;

    Ok(VerificationOutcome {
        status: if regressions.is_empty() {
            VerifyStatus::Passed
        } else {
            VerifyStatus::Failed
        },
        report_path: Some(report_path),
        summary: Some(summary),
        regressions,
    })
}

fn load_manifest(project_root: &Path) -> Result<VerificationManifest> {
    let path = project_root.join(MANIFEST_PATH);
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_yaml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))
}

fn manifest_by_behavior(
    manifest: &VerificationManifest,
) -> Result<BTreeMap<&str, &VerificationCase>> {
    let mut map = BTreeMap::new();
    for case in &manifest.behaviors {
        if map.insert(case.behavior.as_str(), case).is_some() {
            bail!(
                "duplicate verification manifest behavior `{}`",
                case.behavior
            );
        }
    }
    Ok(map)
}

fn write_report(
    artifact_root: &Path,
    summary: &ParitySummary,
    runs: &[BehaviorRun],
    regressions: &[String],
) -> Result<PathBuf> {
    let report_dir = artifact_root.join(REPORTS_DIR);
    std::fs::create_dir_all(&report_dir)
        .with_context(|| format!("failed to create {}", report_dir.display()))?;

    let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let report_path = report_dir.join(format!("verification-{timestamp}.md"));
    let latest_path = report_dir.join(LATEST_REPORT);
    let content = render_report(summary, runs, regressions);
    std::fs::write(&report_path, &content)
        .with_context(|| format!("failed to write {}", report_path.display()))?;
    std::fs::write(&latest_path, &content)
        .with_context(|| format!("failed to write {}", latest_path.display()))?;
    Ok(report_path)
}

fn render_report(summary: &ParitySummary, runs: &[BehaviorRun], regressions: &[String]) -> String {
    let mut out = String::new();
    out.push_str("# Verification Report\n\n");
    out.push_str(&format!(
        "- Generated: {}\n",
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    ));
    out.push_str(&format!("- Total behaviors: {}\n", summary.total_behaviors));
    out.push_str(&format!("- Verified PASS: {}\n", summary.verified_pass));
    out.push_str(&format!("- Verified FAIL: {}\n", summary.verified_fail));
    out.push_str(&format!(
        "- Overall parity: {}%\n",
        summary.overall_parity_pct
    ));
    if regressions.is_empty() {
        out.push_str("- Regressions: none\n\n");
    } else {
        out.push_str(&format!("- Regressions: {}\n\n", regressions.join(", ")));
    }

    out.push_str("| Behavior | Previous | Result | Summary |\n");
    out.push_str("| --- | --- | --- | --- |\n");
    for run in runs {
        let result = if run.report.passed() { "PASS" } else { "FAIL" };
        out.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            run.behavior,
            run.previous_status,
            result,
            run.report.summary()
        ));
    }

    out
}

fn record_summary_event(project_root: &Path, summary: &ParitySummary) -> Result<()> {
    let event = TeamEvent::parity_updated(summary);
    let mut sink = EventSink::new(&super::team_events_path(project_root))?;
    sink.emit(event.clone())?;

    let conn = super::telemetry_db::open(project_root)?;
    super::telemetry_db::insert_event(&conn, &event)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn parity_fixture(previous_verified: &str) -> String {
        format!(
            r#"---
project: trivial
target: trivial.z80
source_platform: zx-spectrum-z80
target_language: rust
last_verified: 2026-04-05
overall_parity: 100%
---

| Behavior | Spec | Test | Implementation | Verified | Notes |
| --- | --- | --- | --- | --- | --- |
| Screen fill | complete | complete | complete | {previous_verified} | previous |
"#
        )
    }

    fn write_script(path: &Path, lines: &[&str]) {
        let body = format!("#!/bin/sh\nprintf '%s\\n' {}\n", lines.join(" "));
        std::fs::write(path, body).unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    fn write_manifest(root: &Path) {
        let batty_dir = root.join(".batty");
        std::fs::create_dir_all(&batty_dir).unwrap();
        std::fs::write(
            batty_dir.join("verification.yml"),
            r#"behaviors:
  - behavior: Screen fill
    baseline: scripts/baseline.sh
    candidate: scripts/candidate.sh
    inputs:
      - fill
      - flip
"#,
        )
        .unwrap();
    }

    #[test]
    fn verify_project_updates_parity_and_writes_report() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("scripts")).unwrap();
        std::fs::write(tmp.path().join("PARITY.md"), parity_fixture("--")).unwrap();
        write_manifest(tmp.path());
        write_script(
            &tmp.path().join("scripts/baseline.sh"),
            &["frame-a", "frame-b"],
        );
        write_script(
            &tmp.path().join("scripts/candidate.sh"),
            &["frame-a", "frame-b"],
        );

        let outcome = verify_project(tmp.path(), tmp.path()).unwrap();
        assert_eq!(outcome.status, VerifyStatus::Passed);
        assert!(outcome.regressions.is_empty());

        let updated = std::fs::read_to_string(tmp.path().join("PARITY.md")).unwrap();
        assert!(updated.contains("| Screen fill | complete | complete | complete | PASS |"));
        assert!(updated.contains("matching_frames=2"));

        let latest_report =
            std::fs::read_to_string(tmp.path().join(REPORTS_DIR).join(LATEST_REPORT)).unwrap();
        assert!(!latest_report.contains("Repressions"));
        assert!(latest_report.contains("Regressions: none"));
    }

    #[test]
    fn verify_project_detects_regressions_from_previous_pass() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("scripts")).unwrap();
        std::fs::write(tmp.path().join("PARITY.md"), parity_fixture("PASS")).unwrap();
        write_manifest(tmp.path());
        write_script(
            &tmp.path().join("scripts/baseline.sh"),
            &["frame-a", "frame-b"],
        );
        write_script(
            &tmp.path().join("scripts/candidate.sh"),
            &["frame-a", "frame-x"],
        );

        let outcome = verify_project(tmp.path(), tmp.path()).unwrap();
        assert_eq!(outcome.status, VerifyStatus::Failed);
        assert_eq!(outcome.regressions, vec!["Screen fill".to_string()]);

        let updated = std::fs::read_to_string(tmp.path().join("PARITY.md")).unwrap();
        assert!(updated.contains("| Screen fill | complete | complete | complete | FAIL |"));
    }

    #[test]
    fn verify_project_skips_when_parity_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let outcome = verify_project(tmp.path(), tmp.path()).unwrap();
        assert_eq!(outcome.status, VerifyStatus::Skipped);
        assert!(outcome.report_path.is_none());
    }
}
