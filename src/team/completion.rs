use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::task::{Task, load_tasks_from_dir};

use super::board::{WorkflowMetadata, read_workflow_metadata, write_workflow_metadata};
use super::daemon::verification;
use super::team_config_dir;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionPacket {
    pub task_id: u32,
    pub branch: Option<String>,
    pub worktree_path: Option<String>,
    pub commit: Option<String>,
    #[serde(default)]
    pub changed_paths: Vec<String>,
    pub tests_run: bool,
    pub tests_passed: bool,
    #[serde(default)]
    pub artifacts: Vec<String>,
    pub outcome: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionValidation {
    pub is_complete: bool,
    pub missing_fields: Vec<String>,
    pub warnings: Vec<String>,
}

pub fn parse_completion(text: &str) -> Result<CompletionPacket> {
    let content = extract_packet_text(text).unwrap_or(text).trim();

    serde_json::from_str(content)
        .or_else(|_| serde_yaml::from_str(content))
        .context("failed to parse completion packet as JSON or YAML")
}

pub fn validate_completion(packet: &CompletionPacket) -> CompletionValidation {
    let mut missing_fields = Vec::new();
    let mut warnings = Vec::new();

    if packet.task_id == 0 {
        missing_fields.push("task_id".to_string());
    }
    if packet.branch.as_deref().is_none_or(str::is_empty) {
        missing_fields.push("branch".to_string());
    }
    if packet.commit.as_deref().is_none_or(str::is_empty) {
        missing_fields.push("commit".to_string());
    }
    if !packet.tests_run {
        missing_fields.push("tests_run".to_string());
    }
    if packet.worktree_path.as_deref().is_none_or(str::is_empty) {
        warnings.push("worktree_path missing".to_string());
    }
    if !packet.tests_passed {
        warnings.push("tests_passed is false".to_string());
    }
    if packet.outcome.trim().is_empty() {
        warnings.push("outcome missing".to_string());
    }

    CompletionValidation {
        is_complete: missing_fields.is_empty(),
        missing_fields,
        warnings,
    }
}

pub fn apply_completion_to_metadata(packet: &CompletionPacket, metadata: &mut WorkflowMetadata) {
    metadata.branch = packet.branch.clone();
    metadata.worktree_path = packet.worktree_path.clone();
    metadata.commit = packet.commit.clone();
    metadata.changed_paths = packet.changed_paths.clone();
    metadata.tests_run = Some(packet.tests_run);
    metadata.tests_passed = Some(packet.tests_passed);
    metadata.artifacts = packet.artifacts.clone();
    metadata.outcome = Some(packet.outcome.clone());
}

pub(crate) fn validate_completion_metadata_target(
    project_root: &Path,
    task_path: &Path,
    expected_task_id: u32,
    branch: Option<&str>,
    artifacts: &[String],
) -> Result<()> {
    let reasons = completion_metadata_drift_reasons(
        project_root,
        task_path,
        expected_task_id,
        branch,
        artifacts,
    )?;
    if reasons.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(
            "completion metadata target mismatch for task #{expected_task_id}: {}",
            reasons.join("; ")
        )
    }
}

pub(crate) fn completion_metadata_drift_reasons(
    project_root: &Path,
    task_path: &Path,
    expected_task_id: u32,
    branch: Option<&str>,
    artifacts: &[String],
) -> Result<Vec<String>> {
    let task = Task::from_file(task_path).with_context(|| {
        format!(
            "failed to read task frontmatter from {}",
            task_path.display()
        )
    })?;
    let mut reasons = Vec::new();

    if task.id != expected_task_id {
        reasons.push(format!(
            "task path {} has frontmatter id #{} instead of #{}",
            task_path.display(),
            task.id,
            expected_task_id
        ));
    }

    if let Some(branch) = branch.map(str::trim).filter(|value| !value.is_empty()) {
        reasons.extend(crate::team::review::task_reference_mismatch_blockers(
            expected_task_id,
            branch,
            &[],
        ));
    }

    for artifact in artifacts {
        reasons.extend(artifact_task_id_drift_reasons(
            project_root,
            expected_task_id,
            artifact,
        )?);
    }

    Ok(reasons)
}

fn artifact_task_id_drift_reasons(
    project_root: &Path,
    expected_task_id: u32,
    artifact: &str,
) -> Result<Vec<String>> {
    let artifact = artifact.trim();
    if artifact.is_empty() {
        return Ok(Vec::new());
    }

    let mut reasons = Vec::new();
    if let Some(task_id) = artifact_task_id_from_path(artifact)
        && task_id != expected_task_id
    {
        reasons.push(format!(
            "artifact `{artifact}` references task #{task_id} instead of #{expected_task_id}"
        ));
    }

    let artifact_path = PathBuf::from(artifact);
    let artifact_path = if artifact_path.is_absolute() {
        artifact_path
    } else {
        project_root.join(artifact_path)
    };
    if artifact_path.extension().is_some_and(|ext| ext == "json") && artifact_path.exists() {
        let content = std::fs::read_to_string(&artifact_path)
            .with_context(|| format!("failed to read artifact {}", artifact_path.display()))?;
        let value: serde_json::Value = serde_json::from_str(&content)
            .with_context(|| format!("failed to parse artifact {}", artifact_path.display()))?;
        if let Some(task_id) = value.get("task_id").and_then(|value| value.as_u64())
            && task_id != u64::from(expected_task_id)
        {
            reasons.push(format!(
                "artifact `{artifact}` contains task_id #{task_id} instead of #{expected_task_id}"
            ));
        }
    }

    Ok(reasons)
}

fn artifact_task_id_from_path(artifact: &str) -> Option<u32> {
    let name = Path::new(artifact).file_name()?.to_str()?;
    let (_, after_marker) = name.split_once("task-")?;
    let digits = after_marker
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    (!digits.is_empty())
        .then(|| digits.parse::<u32>().ok())
        .flatten()
}

fn scope_review_blockers(
    project_root: &Path,
    task_text: &str,
    packet: &CompletionPacket,
) -> Result<Vec<String>> {
    let worktree_dir = resolve_worktree_path(project_root, packet)?;
    if !worktree_dir.exists() {
        return Ok(Vec::new());
    }

    let changed_files = verification::changed_files_from_main(&worktree_dir)?;
    let scope = verification::validate_declared_scope(task_text, &changed_files);
    if scope.declared_scope.is_empty() || scope.out_of_scope_files.is_empty() {
        return Ok(Vec::new());
    }

    Ok(vec![format!(
        "scope fence violation: changed files outside declared scope: {}",
        scope.out_of_scope_files.join(", ")
    )])
}

pub(crate) fn ingest_completion_message(project_root: &Path, message: &str) -> Result<Option<u32>> {
    if !message.contains("Completion Packet") {
        return Ok(None);
    }

    let packet = parse_completion(message)?;
    if !packet.tests_passed {
        anyhow::bail!("completion packet rejected: tests_passed must be true");
    }
    let validation = validate_completion(&packet);
    let task_path = find_task_path(project_root, packet.task_id)?;
    let task_text = std::fs::read_to_string(&task_path)
        .with_context(|| format!("failed to read {}", task_path.display()))?;
    validate_completion_metadata_target(
        project_root,
        &task_path,
        packet.task_id,
        packet.branch.as_deref(),
        &packet.artifacts,
    )?;
    let mut metadata = read_workflow_metadata(&task_path)?;
    apply_completion_to_metadata(&packet, &mut metadata);
    let mut review_blockers = validation.missing_fields;
    review_blockers.extend(scope_review_blockers(project_root, &task_text, &packet)?);
    if packet.outcome.trim() == "ready_for_review"
        && review_blockers.is_empty()
        && let Ok(worktree_path) = resolve_worktree_path(project_root, &packet)
        && worktree_path.exists()
    {
        review_blockers.extend(crate::team::task_loop::validate_review_ready_worktree(
            &worktree_path,
            &task_text,
        )?);
    }
    metadata.review_blockers = review_blockers;
    write_workflow_metadata(&task_path, &metadata)?;
    Ok(Some(packet.task_id))
}

fn resolve_worktree_path(project_root: &Path, packet: &CompletionPacket) -> Result<PathBuf> {
    let raw_path = packet
        .worktree_path
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .context("worktree_path missing for commit validation")?;
    let path = PathBuf::from(raw_path);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(project_root.join(path))
    }
}

fn extract_packet_text(text: &str) -> Option<&str> {
    if let Some(start) = text.find("```") {
        let after_fence = &text[start + 3..];
        let inner_start = after_fence.find('\n').map(|i| i + 1).unwrap_or(0);
        let inner = &after_fence[inner_start..];
        if let Some(end) = inner.find("```") {
            return Some(inner[..end].trim());
        }
    }

    text.find("## Completion Packet")
        .map(|idx| &text[idx + "## Completion Packet".len()..])
        .map(str::trim)
        .filter(|content| !content.is_empty())
}

fn find_task_path(project_root: &Path, task_id: u32) -> Result<PathBuf> {
    let tasks_dir = team_config_dir(project_root).join("board").join("tasks");
    let tasks = load_tasks_from_dir(&tasks_dir)
        .with_context(|| format!("failed to load tasks from {}", tasks_dir.display()))?;
    tasks
        .into_iter()
        .find(|task| task.id == task_id)
        .map(|task| task.source_path)
        .with_context(|| format!("task #{task_id} not found in {}", tasks_dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_completion_parses_json() {
        let packet = parse_completion(
            r#"{"task_id":27,"branch":"eng-1-4/task-27","worktree_path":".batty/worktrees/eng-1-4","commit":"abc1234","changed_paths":["src/team/completion.rs"],"tests_run":true,"tests_passed":true,"artifacts":["docs/workflow.md"],"outcome":"ready_for_review"}"#,
        )
        .unwrap();

        assert_eq!(packet.task_id, 27);
        assert_eq!(packet.branch.as_deref(), Some("eng-1-4/task-27"));
        assert!(packet.tests_run);
        assert!(packet.tests_passed);
    }

    #[test]
    fn parse_completion_parses_fenced_yaml_block() {
        let packet = parse_completion(
            r#"Done.

## Completion Packet

```yaml
task_id: 27
branch: eng-1-4/task-27
worktree_path: .batty/worktrees/eng-1-4
commit: abc1234
changed_paths:
  - src/team/completion.rs
tests_run: true
tests_passed: false
artifacts:
  - docs/workflow.md
outcome: ready_for_review
```"#,
        )
        .unwrap();

        assert_eq!(packet.task_id, 27);
        assert_eq!(packet.commit.as_deref(), Some("abc1234"));
        assert_eq!(packet.artifacts, vec!["docs/workflow.md"]);
        assert!(!packet.tests_passed);
    }

    #[test]
    fn parse_completion_returns_error_for_malformed_packet() {
        let error = parse_completion("{not valid").unwrap_err().to_string();
        assert!(error.contains("failed to parse completion packet"));
    }

    #[test]
    fn validate_completion_reports_complete_packet() {
        let validation = validate_completion(&CompletionPacket {
            task_id: 27,
            branch: Some("eng-1-4/task-27".to_string()),
            worktree_path: Some(".batty/worktrees/eng-1-4".to_string()),
            commit: Some("abc1234".to_string()),
            changed_paths: vec!["src/team/completion.rs".to_string()],
            tests_run: true,
            tests_passed: true,
            artifacts: vec!["docs/workflow.md".to_string()],
            outcome: "ready_for_review".to_string(),
        });

        assert!(validation.is_complete);
        assert!(validation.missing_fields.is_empty());
        assert!(validation.warnings.is_empty());
    }

    #[test]
    fn validate_completion_reports_missing_required_fields() {
        let validation = validate_completion(&CompletionPacket {
            task_id: 0,
            branch: None,
            worktree_path: None,
            commit: None,
            changed_paths: Vec::new(),
            tests_run: false,
            tests_passed: false,
            artifacts: Vec::new(),
            outcome: String::new(),
        });

        assert!(!validation.is_complete);
        assert_eq!(
            validation.missing_fields,
            vec!["task_id", "branch", "commit", "tests_run"]
        );
        assert!(
            validation
                .warnings
                .contains(&"worktree_path missing".to_string())
        );
        assert!(
            validation
                .warnings
                .contains(&"tests_passed is false".to_string())
        );
    }

    #[test]
    fn apply_completion_to_metadata_copies_fields() {
        let packet = CompletionPacket {
            task_id: 27,
            branch: Some("eng-1-4/task-27".to_string()),
            worktree_path: Some(".batty/worktrees/eng-1-4".to_string()),
            commit: Some("abc1234".to_string()),
            changed_paths: vec!["src/team/completion.rs".to_string()],
            tests_run: true,
            tests_passed: true,
            artifacts: vec!["docs/workflow.md".to_string()],
            outcome: "ready_for_review".to_string(),
        };
        let mut metadata = WorkflowMetadata::default();

        apply_completion_to_metadata(&packet, &mut metadata);

        assert_eq!(metadata.branch, packet.branch);
        assert_eq!(metadata.worktree_path, packet.worktree_path);
        assert_eq!(metadata.commit, packet.commit);
        assert_eq!(metadata.changed_paths, packet.changed_paths);
        assert_eq!(metadata.tests_run, Some(true));
        assert_eq!(metadata.tests_passed, Some(true));
        assert_eq!(metadata.artifacts, packet.artifacts);
        assert_eq!(metadata.outcome.as_deref(), Some("ready_for_review"));
    }

    #[test]
    fn ingest_completion_message_adds_scope_fence_review_blocker() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = team_config_dir(tmp.path()).join("board").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let task_path = tasks_dir.join("027-task.md");
        std::fs::write(
            &task_path,
            "---\nid: 27\ntitle: Completion packets\nstatus: review\npriority: medium\nclaimed_by: eng-1-4\nclass: standard\n---\n\nTask body.\nSCOPE FENCE: src/team/completion.rs, src/team/review.rs\n",
        )
        .unwrap();

        let worktree = tmp.path().join(".batty").join("worktrees").join("eng-1-4");
        std::fs::create_dir_all(worktree.join("src/team")).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&worktree)
                .output()
                .unwrap()
        };
        assert!(git(&["init"]).status.success());
        assert!(
            git(&["config", "user.email", "test@example.com"])
                .status
                .success()
        );
        assert!(git(&["config", "user.name", "Test"]).status.success());
        std::fs::write(worktree.join("src/team/completion.rs"), "base\n").unwrap();
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-m", "base"]).status.success());
        assert!(git(&["branch", "-M", "main"]).status.success());
        assert!(git(&["checkout", "-b", "eng-1-4"]).status.success());

        std::fs::write(worktree.join("src/team/review.rs"), "in scope\n").unwrap();
        std::fs::write(worktree.join("src/team/daemon.rs"), "out of scope\n").unwrap();
        assert!(git(&["add", "."]).status.success());
        assert!(git(&["commit", "-m", "change"]).status.success());

        let updated = ingest_completion_message(
            tmp.path(),
            r#"Done.

## Completion Packet

```json
{"task_id":27,"branch":"eng-1-4/task-27","worktree_path":".batty/worktrees/eng-1-4","commit":"abc1234","changed_paths":["src/team/review.rs","src/team/daemon.rs"],"tests_run":true,"tests_passed":true,"artifacts":[],"outcome":"ready_for_review"}
```"#,
        )
        .unwrap();

        assert_eq!(updated, Some(27));
        let metadata = read_workflow_metadata(&task_path).unwrap();
        assert!(
            metadata
                .review_blockers
                .iter()
                .any(|blocker| blocker.contains("src/team/daemon.rs"))
        );
    }

    #[test]
    fn ingest_completion_message_updates_task_workflow_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = team_config_dir(tmp.path()).join("board").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let task_path = tasks_dir.join("027-task.md");
        std::fs::write(
            &task_path,
            "---\nid: 27\ntitle: Completion packets\nstatus: review\npriority: medium\nclaimed_by: eng-1-4\nclass: standard\n---\n\nTask body.\n",
        )
        .unwrap();

        let updated = ingest_completion_message(
            tmp.path(),
            r#"Done.

## Completion Packet

```json
{"task_id":27,"branch":"eng-1-4/task-27","worktree_path":".batty/worktrees/eng-1-4","commit":"abc1234","changed_paths":["src/team/completion.rs"],"tests_run":true,"tests_passed":true,"artifacts":["docs/workflow.md"],"outcome":"ready_for_review"}
```"#,
        )
        .unwrap();

        assert_eq!(updated, Some(27));
        let metadata = read_workflow_metadata(&task_path).unwrap();
        assert_eq!(metadata.branch.as_deref(), Some("eng-1-4/task-27"));
        assert_eq!(metadata.commit.as_deref(), Some("abc1234"));
        assert_eq!(metadata.tests_run, Some(true));
        assert!(metadata.review_blockers.is_empty());
    }

    #[test]
    fn ingest_completion_message_rejects_branch_task_mismatch_without_writing_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = team_config_dir(tmp.path()).join("board").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let task_path = tasks_dir.join("027-task.md");
        std::fs::write(
            &task_path,
            "---\nid: 27\ntitle: Completion packets\nstatus: review\npriority: medium\nclaimed_by: eng-1-4\nclass: standard\n---\n\nTask body.\n",
        )
        .unwrap();

        let error = ingest_completion_message(
            tmp.path(),
            r#"Done.

## Completion Packet

```json
{"task_id":27,"branch":"eng-1-4/task-99","worktree_path":".batty/worktrees/eng-1-4","commit":"abc1234","changed_paths":["src/team/completion.rs"],"tests_run":true,"tests_passed":true,"artifacts":[],"outcome":"ready_for_review"}
```"#,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("branch `eng-1-4/task-99` references task(s) #99"));
        let metadata = read_workflow_metadata(&task_path).unwrap();
        assert!(metadata.branch.is_none());
        assert!(metadata.commit.is_none());
    }

    #[test]
    fn completion_metadata_validator_rejects_artifact_task_id_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = team_config_dir(tmp.path()).join("board").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let task_path = tasks_dir.join("027-task.md");
        std::fs::write(
            &task_path,
            "---\nid: 27\ntitle: Completion packets\nstatus: review\npriority: medium\nclass: standard\n---\n\nTask body.\n",
        )
        .unwrap();
        let artifact = ".batty/reports/verification/completion/task-099-eng-1-attempt-1.json";
        let artifact_path = tmp.path().join(artifact);
        std::fs::create_dir_all(artifact_path.parent().unwrap()).unwrap();
        std::fs::write(&artifact_path, r#"{"task_id":99,"passed":true}"#).unwrap();

        let error = validate_completion_metadata_target(
            tmp.path(),
            &task_path,
            27,
            Some("eng-1-4/task-27"),
            &[artifact.to_string()],
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("artifact"));
        assert!(error.contains("#99"));
        assert!(error.contains("#27"));
    }

    #[test]
    fn ingest_completion_message_rejects_failed_tests() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = team_config_dir(tmp.path()).join("board").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let task_path = tasks_dir.join("027-task.md");
        std::fs::write(
            &task_path,
            "---\nid: 27\ntitle: Completion packets\nstatus: review\npriority: medium\nclaimed_by: eng-1-4\nclass: standard\n---\n\nTask body.\n",
        )
        .unwrap();

        let error = ingest_completion_message(
            tmp.path(),
            r#"Done.

## Completion Packet

```json
{"task_id":27,"branch":"eng-1-4/task-27","worktree_path":".batty/worktrees/eng-1-4","commit":"abc1234","changed_paths":["src/team/completion.rs"],"tests_run":true,"tests_passed":false,"artifacts":[],"outcome":"ready_for_review"}
```"#,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("tests_passed must be true"));
        let metadata = read_workflow_metadata(&task_path).unwrap();
        assert!(metadata.branch.is_none());
        assert!(metadata.review_blockers.is_empty());
    }
}
