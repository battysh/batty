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

    serde_json::to_writer(&mut file, record)
        .with_context(|| format!("failed to serialize merge record for {}", log_path.display()))?;
    file.write_all(b"\n")
        .with_context(|| format!("failed to append newline to {}", log_path.display()))?;
    Ok(())
}

pub fn read_merge_log(log_path: &Path) -> Result<Vec<MergeRecord>> {
    if !log_path.exists() {
        return Ok(Vec::new());
    }

    let file = File::open(log_path).with_context(|| format!("failed to open {}", log_path.display()))?;
    let reader = BufReader::new(file);
    let mut records = Vec::new();

    for (index, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("failed to read line {} from {}", index + 1, log_path.display()))?;
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
