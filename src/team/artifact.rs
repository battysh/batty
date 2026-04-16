#![cfg_attr(not(test), allow(dead_code))]

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::board::WorkflowMetadata;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactType {
    TestResult,
    BuildOutput,
    Documentation,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactRecord {
    pub path: String,
    pub artifact_type: ArtifactType,
    pub created_at: Option<u64>,
    pub verified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MergeRecord {
    pub task_id: u32,
    pub branch: String,
    pub commit: String,
    pub merged_at: u64,
    pub merged_by: String,
    pub artifacts: Vec<ArtifactRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TestTimingRecord {
    pub task_id: u32,
    pub engineer: String,
    pub branch: String,
    pub measured_at: u64,
    pub duration_ms: u64,
    pub rolling_average_ms: Option<u64>,
    pub regression_pct: Option<u32>,
    pub regression_detected: bool,
}

pub fn record_merge(log_path: &Path, record: &MergeRecord) -> Result<()> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("failed to open {}", log_path.display()))?;

    serde_json::to_writer(&mut file, record).with_context(|| {
        format!(
            "failed to serialize merge record for {}",
            log_path.display()
        )
    })?;
    file.write_all(b"\n")
        .with_context(|| format!("failed to append newline to {}", log_path.display()))?;
    Ok(())
}

pub fn read_merge_log(log_path: &Path) -> Result<Vec<MergeRecord>> {
    if !log_path.exists() {
        return Ok(Vec::new());
    }

    let file =
        File::open(log_path).with_context(|| format!("failed to open {}", log_path.display()))?;
    let reader = BufReader::new(file);
    let mut records = Vec::new();

    for (index, line) in reader.lines().enumerate() {
        let line = line.with_context(|| {
            format!(
                "failed to read line {} from {}",
                index + 1,
                log_path.display()
            )
        })?;
        if line.trim().is_empty() {
            continue;
        }
        let record = serde_json::from_str::<MergeRecord>(&line).with_context(|| {
            format!(
                "failed to parse merge record line {} from {}",
                index + 1,
                log_path.display()
            )
        })?;
        records.push(record);
    }

    Ok(records)
}

pub fn record_test_timing(log_path: &Path, record: &TestTimingRecord) -> Result<()> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("failed to open {}", log_path.display()))?;

    serde_json::to_writer(&mut file, record).with_context(|| {
        format!(
            "failed to serialize test timing record for {}",
            log_path.display()
        )
    })?;
    file.write_all(b"\n")
        .with_context(|| format!("failed to append newline to {}", log_path.display()))?;
    Ok(())
}

pub fn read_test_timing_log(log_path: &Path) -> Result<Vec<TestTimingRecord>> {
    if !log_path.exists() {
        return Ok(Vec::new());
    }

    let file =
        File::open(log_path).with_context(|| format!("failed to open {}", log_path.display()))?;
    let reader = BufReader::new(file);
    let mut records = Vec::new();

    for (index, line) in reader.lines().enumerate() {
        let line = line.with_context(|| {
            format!(
                "failed to read line {} from {}",
                index + 1,
                log_path.display()
            )
        })?;
        if line.trim().is_empty() {
            continue;
        }
        let record = serde_json::from_str::<TestTimingRecord>(&line).with_context(|| {
            format!(
                "failed to parse test timing record line {} from {}",
                index + 1,
                log_path.display()
            )
        })?;
        records.push(record);
    }

    Ok(records)
}

pub fn build_test_timing_record(
    history: &[TestTimingRecord],
    task_id: u32,
    engineer: &str,
    branch: &str,
    measured_at: u64,
    duration_ms: u64,
) -> TestTimingRecord {
    let window: Vec<&TestTimingRecord> = history.iter().rev().take(5).collect();
    let (rolling_average_ms, regression_pct, regression_detected) = if window.len() == 5 {
        let total_ms: u64 = window.iter().map(|record| record.duration_ms).sum();
        let average_ms = total_ms / 5;
        let pct = (duration_ms.saturating_sub(average_ms) * 100)
            .checked_div(average_ms)
            .map(|v| v as u32);
        let detected = average_ms > 0 && duration_ms.saturating_mul(100) > average_ms * 120;
        (Some(average_ms), pct, detected)
    } else {
        (None, None, false)
    };

    TestTimingRecord {
        task_id,
        engineer: engineer.to_string(),
        branch: branch.to_string(),
        measured_at,
        duration_ms,
        rolling_average_ms,
        regression_pct,
        regression_detected,
    }
}

pub fn append_test_timing_record(
    log_path: &Path,
    task_id: u32,
    engineer: &str,
    branch: &str,
    measured_at: u64,
    duration_ms: u64,
) -> Result<TestTimingRecord> {
    let history = read_test_timing_log(log_path)?;
    let record = build_test_timing_record(
        &history,
        task_id,
        engineer,
        branch,
        measured_at,
        duration_ms,
    );
    record_test_timing(log_path, &record)?;
    Ok(record)
}

pub fn track_artifact(meta: &mut WorkflowMetadata, artifact: &str) {
    let artifact = artifact.trim();
    if artifact.is_empty() {
        return;
    }
    if !meta.artifacts.iter().any(|existing| existing == artifact) {
        meta.artifacts.push(artifact.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_record(path: &str, artifact_type: ArtifactType) -> ArtifactRecord {
        ArtifactRecord {
            path: path.to_string(),
            artifact_type,
            created_at: Some(1_777_000_000),
            verified: true,
        }
    }

    fn sample_merge_record() -> MergeRecord {
        MergeRecord {
            task_id: 29,
            branch: "eng-1-3/task-29".to_string(),
            commit: "abc1234".to_string(),
            merged_at: 1_777_000_123,
            merged_by: "manager".to_string(),
            artifacts: vec![
                sample_record("target/debug/batty", ArtifactType::BuildOutput),
                sample_record("target/nextest/default.xml", ArtifactType::TestResult),
            ],
        }
    }

    fn sample_test_timing_record(task_id: u32, duration_ms: u64) -> TestTimingRecord {
        TestTimingRecord {
            task_id,
            engineer: "eng-1".to_string(),
            branch: format!("eng-1/task-{task_id}"),
            measured_at: 1_777_000_000 + task_id as u64,
            duration_ms,
            rolling_average_ms: None,
            regression_pct: None,
            regression_detected: false,
        }
    }

    #[test]
    fn record_merge_appends_to_log_file() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join(".batty").join("merge-log.jsonl");
        let first = sample_merge_record();
        let mut second = sample_merge_record();
        second.task_id = 30;
        second.commit = "def5678".to_string();

        record_merge(&log_path, &first).unwrap();
        record_merge(&log_path, &second).unwrap();

        let content = std::fs::read_to_string(&log_path).unwrap();
        assert_eq!(content.lines().count(), 2);
        assert!(content.contains("\"task_id\":29"));
        assert!(content.contains("\"task_id\":30"));
    }

    #[test]
    fn read_merge_log_parses_back() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("merge-log.jsonl");
        let record = sample_merge_record();

        record_merge(&log_path, &record).unwrap();

        let parsed = read_merge_log(&log_path).unwrap();
        assert_eq!(parsed, vec![record]);
    }

    #[test]
    fn record_test_timing_appends_to_log_file() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join(".batty").join("test_timing.jsonl");
        let first = sample_test_timing_record(29, 950);
        let mut second = sample_test_timing_record(30, 1_150);
        second.regression_detected = true;
        second.rolling_average_ms = Some(900);
        second.regression_pct = Some(27);

        record_test_timing(&log_path, &first).unwrap();
        record_test_timing(&log_path, &second).unwrap();

        let content = std::fs::read_to_string(&log_path).unwrap();
        assert_eq!(content.lines().count(), 2);
        assert!(content.contains("\"task_id\":29"));
        assert!(content.contains("\"task_id\":30"));
    }

    #[test]
    fn read_test_timing_log_parses_back() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("test_timing.jsonl");
        let record = sample_test_timing_record(31, 1_250);

        record_test_timing(&log_path, &record).unwrap();

        let parsed = read_test_timing_log(&log_path).unwrap();
        assert_eq!(parsed, vec![record]);
    }

    #[test]
    fn build_test_timing_record_skips_regression_without_five_prior_merges() {
        let history = vec![
            sample_test_timing_record(1, 900),
            sample_test_timing_record(2, 950),
            sample_test_timing_record(3, 1_000),
            sample_test_timing_record(4, 980),
        ];

        let record = build_test_timing_record(&history, 5, "eng-1", "eng-1/task-5", 100, 1_300);

        assert_eq!(record.rolling_average_ms, None);
        assert_eq!(record.regression_pct, None);
        assert!(!record.regression_detected);
    }

    #[test]
    fn build_test_timing_record_detects_regression_against_previous_five_merges() {
        let history = vec![
            sample_test_timing_record(1, 1_000),
            sample_test_timing_record(2, 1_000),
            sample_test_timing_record(3, 1_000),
            sample_test_timing_record(4, 1_000),
            sample_test_timing_record(5, 1_000),
        ];

        let record = build_test_timing_record(&history, 6, "eng-1", "eng-1/task-6", 106, 1_250);

        assert_eq!(record.rolling_average_ms, Some(1_000));
        assert_eq!(record.regression_pct, Some(25));
        assert!(record.regression_detected);
    }

    #[test]
    fn track_artifact_adds_to_metadata_and_deduplicates() {
        let mut meta = WorkflowMetadata::default();

        track_artifact(&mut meta, "target/debug/batty");
        track_artifact(&mut meta, "target/debug/batty");
        track_artifact(&mut meta, "target/doc/index.html");

        assert_eq!(
            meta.artifacts,
            vec![
                "target/debug/batty".to_string(),
                "target/doc/index.html".to_string()
            ]
        );
    }

    #[test]
    fn artifact_record_serde_round_trip() {
        let record = sample_record("docs/workflow.md", ArtifactType::Documentation);

        let json = serde_json::to_string(&record).unwrap();
        let parsed: ArtifactRecord = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed, record);
        assert!(json.contains("\"artifact_type\":\"documentation\""));
    }
}
