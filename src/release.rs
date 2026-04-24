use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::team::config::TeamConfig;
use crate::team::daemon::verification::run_automatic_verification;
use crate::team::events::{EventSink, TeamEvent};
use crate::team::github_feedback::{GithubReleaseFeedbackItem, GithubReleaseFeedbackSummary};

const RELEASES_DIR: &str = ".batty/releases";
const RELEASE_HISTORY_FILE: &str = "history.jsonl";
const RELEASE_LATEST_JSON: &str = "latest.json";
const RELEASE_LATEST_MARKDOWN: &str = "latest.md";
const RELEASE_READINESS_JSON: &str = "readiness.json";
const RELEASE_READINESS_MARKDOWN: &str = "readiness.md";

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReleaseMetadata {
    package_name: String,
    version: String,
    tag: String,
    changelog_heading: String,
    changelog_body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReleaseContext {
    metadata: ReleaseMetadata,
    branch: String,
    git_ref: String,
    previous_tag: Option<String>,
    commits_since_previous: usize,
    commit_summaries: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReleaseVerification {
    command: String,
    passed: bool,
    summary: String,
    details: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseRecord {
    pub ts: String,
    pub package_name: Option<String>,
    pub version: Option<String>,
    pub tag: Option<String>,
    pub git_ref: Option<String>,
    pub branch: Option<String>,
    pub previous_tag: Option<String>,
    pub commits_since_previous: Option<usize>,
    pub verification_command: Option<String>,
    pub verification_summary: Option<String>,
    pub success: bool,
    pub reason: String,
    pub details: Option<String>,
    pub notes_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseReadinessReport {
    pub package_name: Option<String>,
    pub version: Option<String>,
    pub proposed_tag: Option<String>,
    pub current_commit: Option<String>,
    pub branch: Option<String>,
    pub previous_tag: Option<String>,
    pub commits_since_previous: Option<usize>,
    pub recently_merged_task_ids: Vec<String>,
    pub verification_command: Option<String>,
    pub verification_summary: Option<String>,
    pub github_feedback: GithubReleaseFeedbackSummary,
    pub blockers: Vec<String>,
}

impl ReleaseReadinessReport {
    fn ready(&self) -> bool {
        self.blockers.is_empty()
    }
}

#[derive(Debug, Clone, Default)]
struct ReleaseDraft {
    package_name: Option<String>,
    version: Option<String>,
    tag: Option<String>,
    git_ref: Option<String>,
    branch: Option<String>,
    previous_tag: Option<String>,
    commits_since_previous: Option<usize>,
    verification_command: Option<String>,
    verification_summary: Option<String>,
    notes_path: Option<String>,
}

#[derive(Debug, Clone)]
struct ReleaseFailure {
    record: ReleaseRecord,
    report_markdown: String,
    message: String,
}

trait VerificationRunner {
    fn run(&self, project_root: &Path) -> Result<ReleaseVerification>;
}

struct ConfiguredVerificationRunner {
    command_override: Option<String>,
}

impl VerificationRunner for ConfiguredVerificationRunner {
    fn run(&self, project_root: &Path) -> Result<ReleaseVerification> {
        let command = resolve_verification_command(project_root, self.command_override.as_deref())?;
        let run = run_automatic_verification(project_root, Some(&command)).with_context(|| {
            format!("failed while running release verification command `{command}`")
        })?;
        let summary = if run.passed {
            run.results
                .summary
                .clone()
                .unwrap_or_else(|| format!("{command} passed"))
        } else if !run.failures.is_empty() {
            run.failures.join("; ")
        } else {
            run.results.failure_summary()
        };
        let details = trimmed_output(&run.output);

        Ok(ReleaseVerification {
            command,
            passed: run.passed,
            summary,
            details,
        })
    }
}

pub fn cmd_release(project_root: &Path, requested_tag: Option<&str>) -> Result<()> {
    let verifier = ConfiguredVerificationRunner {
        command_override: None,
    };

    match run_release_with_verifier(project_root, requested_tag, &verifier) {
        Ok((record, report_markdown)) => {
            persist_release_record(project_root, &record)?;
            write_latest_report(project_root, &report_markdown)?;
            emit_release_record(project_root, &record)?;
            println!(
                "Release succeeded: {} -> {}",
                record.tag.as_deref().unwrap_or("unknown-tag"),
                record.git_ref.as_deref().unwrap_or("unknown-ref")
            );
            if let Some(path) = record.notes_path.as_deref() {
                println!("Release notes: {path}");
            }
            println!(
                "Verification: {}",
                record
                    .verification_summary
                    .as_deref()
                    .unwrap_or("no verification summary recorded")
            );
            Ok(())
        }
        Err(failure) => {
            persist_release_record(project_root, &failure.record)?;
            write_latest_report(project_root, &failure.report_markdown)?;
            emit_release_record(project_root, &failure.record)?;
            bail!("{}", failure.message);
        }
    }
}

pub fn cmd_release_readiness(project_root: &Path, requested_tag: Option<&str>) -> Result<()> {
    let verifier = ConfiguredVerificationRunner {
        command_override: None,
    };
    let (report, markdown) =
        generate_release_readiness_with_verifier(project_root, requested_tag, &verifier)?;
    let (json_path, markdown_path) = persist_release_readiness(project_root, &report, &markdown)?;
    if report.ready() {
        println!("Release readiness: ready");
    } else {
        println!("Release readiness: blocked");
    }
    println!("Readiness report: {}", markdown_path.display());
    println!("Readiness JSON: {}", json_path.display());
    if report.ready() {
        Ok(())
    } else {
        bail!("release readiness blocked: {}", report.blockers.join("; "))
    }
}

pub fn latest_record(project_root: &Path) -> Result<Option<ReleaseRecord>> {
    let path = releases_dir(project_root).join(RELEASE_LATEST_JSON);
    if !path.exists() {
        return Ok(None);
    }
    let content =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let record = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(record))
}

fn generate_release_readiness_with_verifier(
    project_root: &Path,
    requested_tag: Option<&str>,
    verifier: &dyn VerificationRunner,
) -> Result<(ReleaseReadinessReport, String)> {
    let mut report = ReleaseReadinessReport {
        package_name: None,
        version: None,
        proposed_tag: None,
        current_commit: None,
        branch: None,
        previous_tag: None,
        commits_since_previous: None,
        recently_merged_task_ids: collect_recently_merged_task_ids(project_root, 10)?,
        verification_command: None,
        verification_summary: None,
        github_feedback: GithubReleaseFeedbackSummary::default(),
        blockers: Vec::new(),
    };

    match load_release_metadata(project_root, requested_tag) {
        Ok(metadata) => {
            report.package_name = Some(metadata.package_name);
            report.version = Some(metadata.version);
            report.proposed_tag = Some(metadata.tag);
        }
        Err(error) => report
            .blockers
            .push(format!("missing_release_metadata: {error}")),
    }

    match git_stdout(project_root, &["rev-parse", "--abbrev-ref", "HEAD"]) {
        Ok(branch) => {
            if branch != "main" {
                report
                    .blockers
                    .push(format!("not_on_main: current branch is `{branch}`"));
            }
            report.branch = Some(branch);
        }
        Err(error) => report
            .blockers
            .push(format!("git_branch_lookup_failed: {error}")),
    }

    match git_stdout(project_root, &["status", "--porcelain"]) {
        Ok(dirty) => {
            let dirty_entries: Vec<&str> = dirty
                .lines()
                .filter(|line| !line.starts_with("?? .batty/"))
                .collect();
            if !dirty_entries.is_empty() {
                report.blockers.push(format!(
                    "dirty_main: {} uncommitted change(s)",
                    dirty_entries.len()
                ));
            }
        }
        Err(error) => report.blockers.push(format!("git_status_failed: {error}")),
    }

    match git_stdout(project_root, &["rev-parse", "HEAD"]) {
        Ok(git_ref) => report.current_commit = Some(git_ref),
        Err(error) => report
            .blockers
            .push(format!("git_ref_lookup_failed: {error}")),
    }

    match crate::team::github_feedback::summarize_release_github_feedback(
        project_root,
        report.current_commit.as_deref(),
    ) {
        Ok(github_feedback) => {
            if !github_feedback.failing.is_empty() {
                report.blockers.push(format!(
                    "github_feedback_failed: {} failing GitHub check(s) for {}",
                    github_feedback.failing.len(),
                    github_feedback
                        .current_commit
                        .as_deref()
                        .map(short_git_ref)
                        .unwrap_or("unknown HEAD")
                ));
            }
            report.github_feedback = github_feedback;
        }
        Err(error) => report
            .blockers
            .push(format!("github_feedback_unavailable: {error}")),
    }

    if let Some(tag) = report.proposed_tag.as_deref() {
        match git_ref_for_tag(project_root, tag) {
            Ok(Some(_)) => report
                .blockers
                .push(format!("tag_exists: tag `{tag}` already exists")),
            Ok(None) => {}
            Err(error) => report.blockers.push(format!("tag_lookup_failed: {error}")),
        }
    }

    match latest_git_tag(project_root) {
        Ok(previous_tag) => {
            report.previous_tag = previous_tag.clone();
            match count_commits_since(project_root, previous_tag.as_deref()) {
                Ok(count) => report.commits_since_previous = Some(count),
                Err(error) => report
                    .blockers
                    .push(format!("commit_count_failed: {error}")),
            }
        }
        Err(error) => report
            .blockers
            .push(format!("previous_tag_lookup_failed: {error}")),
    }

    match verifier.run(project_root) {
        Ok(verification) => {
            report.verification_command = Some(verification.command);
            let summary = verification.summary.trim().to_string();
            report.verification_summary = Some(summary.clone());
            if !verification.passed {
                report.blockers.push(format!(
                    "verification_failed: {}",
                    verification
                        .details
                        .as_deref()
                        .unwrap_or(summary.as_str())
                        .trim()
                ));
            } else if summary.is_empty() {
                report.blockers.push(
                    "missing_verification_evidence: verification passed without a summary"
                        .to_string(),
                );
            }
        }
        Err(error) => report
            .blockers
            .push(format!("verification_unavailable: {error}")),
    }

    if let Some(blocker) = daemon_binary_blocker(project_root)? {
        report.blockers.push(blocker);
    }

    let markdown = render_release_readiness_report(&report);
    Ok((report, markdown))
}

#[allow(clippy::result_large_err)]
fn run_release_with_verifier(
    project_root: &Path,
    requested_tag: Option<&str>,
    verifier: &dyn VerificationRunner,
) -> std::result::Result<(ReleaseRecord, String), ReleaseFailure> {
    let mut draft = ReleaseDraft::default();

    let metadata = load_release_metadata(project_root, requested_tag).map_err(|error| {
        failure(
            &draft,
            "missing_release_metadata",
            "release metadata is missing or invalid",
            Some(error.to_string()),
        )
    })?;
    draft.package_name = Some(metadata.package_name.clone());
    draft.version = Some(metadata.version.clone());
    draft.tag = Some(metadata.tag.clone());

    let branch =
        git_stdout(project_root, &["rev-parse", "--abbrev-ref", "HEAD"]).map_err(|error| {
            failure(
                &draft,
                "git_branch_lookup_failed",
                "failed to resolve the current branch before releasing",
                Some(error.to_string()),
            )
        })?;
    draft.branch = Some(branch.clone());
    if branch != "main" {
        return Err(failure(
            &draft,
            "not_on_main",
            "release requires the project root to be on `main`",
            Some(format!("current branch is `{branch}`")),
        ));
    }

    let dirty = git_stdout(project_root, &["status", "--porcelain"]).map_err(|error| {
        failure(
            &draft,
            "git_status_failed",
            "failed to inspect the `main` worktree before releasing",
            Some(error.to_string()),
        )
    })?;
    if dirty.lines().any(|line| !line.starts_with("?? .batty/")) {
        return Err(failure(
            &draft,
            "dirty_main",
            "release readiness failed: `main` has uncommitted changes",
            Some("commit, stash, or remove local changes before releasing".to_string()),
        ));
    }

    let git_ref = git_stdout(project_root, &["rev-parse", "main"]).map_err(|error| {
        failure(
            &draft,
            "git_ref_lookup_failed",
            "failed to resolve the `main` git ref before releasing",
            Some(error.to_string()),
        )
    })?;
    draft.git_ref = Some(git_ref.clone());

    if git_ref_for_tag(project_root, &metadata.tag)
        .map_err(|error| {
            failure(
                &draft,
                "tag_lookup_failed",
                "failed to inspect whether the target release tag already exists",
                Some(error.to_string()),
            )
        })?
        .is_some()
    {
        return Err(failure(
            &draft,
            "tag_exists",
            "release readiness failed: the target tag already exists",
            Some(format!(
                "tag `{}` already points at an existing release",
                metadata.tag
            )),
        ));
    }

    let previous_tag = latest_git_tag(project_root).map_err(|error| {
        failure(
            &draft,
            "previous_tag_lookup_failed",
            "failed to resolve the previous release tag",
            Some(error.to_string()),
        )
    })?;
    draft.previous_tag = previous_tag.clone();

    let commits_since_previous = count_commits_since(project_root, previous_tag.as_deref())
        .map_err(|error| {
            failure(
                &draft,
                "commit_count_failed",
                "failed to count commits included in this release",
                Some(error.to_string()),
            )
        })?;
    draft.commits_since_previous = Some(commits_since_previous);

    let verification = verifier.run(project_root).map_err(|error| {
        failure(
            &draft,
            "verification_start_failed",
            "release verification could not start",
            Some(error.to_string()),
        )
    })?;
    draft.verification_command = Some(verification.command.clone());
    draft.verification_summary = Some(verification.summary.clone());
    if !verification.passed {
        return Err(failure(
            &draft,
            "verification_failed",
            "release readiness failed: verification is not green",
            verification
                .details
                .clone()
                .or_else(|| Some(verification.summary.clone())),
        ));
    }

    let commit_summaries = collect_commit_summaries(project_root, previous_tag.as_deref())
        .map_err(|error| {
            failure(
                &draft,
                "commit_summary_failed",
                "failed to assemble the release commit summary",
                Some(error.to_string()),
            )
        })?;

    let context = ReleaseContext {
        metadata,
        branch,
        git_ref,
        previous_tag,
        commits_since_previous,
        commit_summaries,
    };

    let notes = render_release_notes(&context, &verification);
    let notes_path = write_release_notes(project_root, &context, &notes).map_err(|error| {
        failure(
            &draft,
            "notes_write_failed",
            "failed to write release notes",
            Some(error.to_string()),
        )
    })?;
    draft.notes_path = Some(notes_path.display().to_string());

    git_ok(
        project_root,
        &[
            "tag",
            "-a",
            context.metadata.tag.as_str(),
            "-F",
            notes_path.to_string_lossy().as_ref(),
        ],
    )
    .map_err(|error| {
        failure(
            &draft,
            "tag_creation_failed",
            "failed to create the annotated release tag",
            Some(error.to_string()),
        )
    })?;

    let record = success_record(&context, &verification, &notes_path);
    Ok((record, notes))
}

fn releases_dir(project_root: &Path) -> PathBuf {
    project_root.join(RELEASES_DIR)
}

fn resolve_verification_command(
    project_root: &Path,
    override_command: Option<&str>,
) -> Result<String> {
    if let Some(command) = override_command {
        return Ok(command.trim().to_string());
    }

    let team_config_path = crate::team::team_config_path(project_root);
    if !team_config_path.exists() {
        return Ok("cargo test".to_string());
    }

    let config = TeamConfig::load(&team_config_path)
        .with_context(|| format!("failed to load {}", team_config_path.display()))?;
    let command = config
        .workflow_policy
        .verification
        .test_command
        .unwrap_or_else(|| "cargo test".to_string());
    if command.trim().is_empty() {
        bail!(
            "{} sets workflow_policy.verification.test_command to an empty value",
            team_config_path.display()
        );
    }
    Ok(command)
}

fn load_release_metadata(
    project_root: &Path,
    requested_tag: Option<&str>,
) -> Result<ReleaseMetadata> {
    #[derive(Deserialize)]
    struct CargoToml {
        package: Option<CargoPackage>,
    }

    #[derive(Deserialize)]
    struct CargoPackage {
        name: Option<String>,
        version: Option<String>,
    }

    let cargo_toml_path = project_root.join("Cargo.toml");
    let content = fs::read_to_string(&cargo_toml_path)
        .with_context(|| format!("failed to read {}", cargo_toml_path.display()))?;
    let parsed: CargoToml = toml::from_str(&content)
        .with_context(|| format!("failed to parse {}", cargo_toml_path.display()))?;
    let package = parsed
        .package
        .context("Cargo.toml is missing `[package]` release metadata")?;
    let package_name = package
        .name
        .filter(|value| !value.trim().is_empty())
        .context("Cargo.toml package.name is required for releases")?;
    let version = package
        .version
        .filter(|value| !value.trim().is_empty())
        .context("Cargo.toml package.version is required for releases")?;

    let (changelog_heading, changelog_body) = load_changelog_entry(project_root, &version)?;
    let tag = requested_tag
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("v{version}"));

    Ok(ReleaseMetadata {
        package_name,
        version,
        tag,
        changelog_heading,
        changelog_body,
    })
}

fn load_changelog_entry(project_root: &Path, version: &str) -> Result<(String, String)> {
    let changelog_path = project_root.join("CHANGELOG.md");
    let content = fs::read_to_string(&changelog_path)
        .with_context(|| format!("failed to read {}", changelog_path.display()))?;
    let lines: Vec<&str> = content.lines().collect();
    let heading_prefix = format!("## {version}");

    let Some(start_index) = lines
        .iter()
        .position(|line| line.trim_start().starts_with(&heading_prefix))
    else {
        bail!(
            "CHANGELOG.md is missing a release heading for version {}",
            version
        );
    };

    let heading = lines[start_index].trim().to_string();
    let end_index = lines[start_index + 1..]
        .iter()
        .position(|line| line.trim_start().starts_with("## "))
        .map(|offset| start_index + 1 + offset)
        .unwrap_or(lines.len());
    let body = lines[start_index + 1..end_index]
        .join("\n")
        .trim()
        .to_string();
    if body.is_empty() {
        bail!(
            "CHANGELOG.md release entry for version {} is empty",
            version
        );
    }

    Ok((heading, body))
}

fn git_stdout(project_root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(args)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(
            "git {} failed{}",
            args.join(" "),
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            }
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_ok(project_root: &Path, args: &[&str]) -> Result<()> {
    let _ = git_stdout(project_root, args)?;
    Ok(())
}

fn git_ref_for_tag(project_root: &Path, tag: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["rev-parse", "-q", "--verify", &format!("refs/tags/{tag}")])
        .output()
        .with_context(|| format!("failed to inspect tag `{tag}`"))?;
    match output.status.code() {
        Some(0) => Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        )),
        Some(1) => Ok(None),
        _ => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            bail!("git rev-parse tag `{tag}` failed: {stderr}");
        }
    }
}

fn latest_git_tag(project_root: &Path) -> Result<Option<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["describe", "--tags", "--abbrev=0"])
        .output()
        .context("failed to resolve the latest git tag")?;
    if output.status.success() {
        return Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        ));
    }

    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    if stderr.contains("no names found") || stderr.contains("no tags can describe") {
        return Ok(None);
    }

    bail!(
        "git describe --tags --abbrev=0 failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

fn count_commits_since(project_root: &Path, previous_tag: Option<&str>) -> Result<usize> {
    let count = match previous_tag {
        Some(tag) => {
            let range = format!("{tag}..main");
            git_stdout(project_root, &["rev-list", "--count", &range])?
        }
        None => git_stdout(project_root, &["rev-list", "--count", "main"])?,
    };
    count
        .parse::<usize>()
        .with_context(|| format!("failed to parse commit count `{count}`"))
}

fn collect_commit_summaries(
    project_root: &Path,
    previous_tag: Option<&str>,
) -> Result<Vec<String>> {
    let output = match previous_tag {
        Some(tag) => {
            let range = format!("{tag}..main");
            git_stdout(
                project_root,
                &["log", "--no-merges", "--format=%h %s", &range],
            )?
        }
        None => git_stdout(
            project_root,
            &["log", "--no-merges", "--format=%h %s", "-n", "20", "main"],
        )?,
    };
    Ok(output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

fn collect_recently_merged_task_ids(project_root: &Path, limit: usize) -> Result<Vec<String>> {
    let path = crate::team::team_events_path(project_root);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut task_ids = Vec::new();
    for line in content.lines().rev() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let event = value.get("event").and_then(|event| event.as_str());
        if !matches!(event, Some("task_auto_merged" | "task_manual_merged")) {
            continue;
        }
        let Some(task_id) = value.get("task").and_then(|task| task.as_str()) else {
            continue;
        };
        if !task_ids.iter().any(|existing| existing == task_id) {
            task_ids.push(task_id.to_string());
        }
        if task_ids.len() >= limit {
            break;
        }
    }
    task_ids.reverse();
    Ok(task_ids)
}

fn daemon_binary_blocker(project_root: &Path) -> Result<Option<String>> {
    let binary_path = std::env::current_exe().context("failed to resolve current executable")?;
    daemon_binary_blocker_for_path(project_root, &binary_path)
}

fn daemon_binary_blocker_for_path(
    project_root: &Path,
    binary_path: &Path,
) -> Result<Option<String>> {
    let Some(freshness) = crate::team::daemon::health::binary_freshness::evaluate_binary_freshness(
        binary_path,
        project_root,
    )?
    else {
        return Ok(None);
    };
    if freshness.fresh {
        Ok(None)
    } else {
        Ok(Some(format!(
            "stale_daemon_binary: {} commit(s) behind main (last: {} {})",
            freshness.commits_behind, freshness.last_hash, freshness.last_subject
        )))
    }
}

fn render_release_notes(context: &ReleaseContext, verification: &ReleaseVerification) -> String {
    let mut out = String::new();
    out.push_str("# Release Notes\n\n");
    out.push_str(&format!("- Package: {}\n", context.metadata.package_name));
    out.push_str(&format!("- Version: {}\n", context.metadata.version));
    out.push_str(&format!("- Tag: {}\n", context.metadata.tag));
    out.push_str(&format!("- Git Ref: {}\n", context.git_ref));
    out.push_str(&format!("- Branch: {}\n", context.branch));
    out.push_str(&format!(
        "- Previous Tag: {}\n",
        context.previous_tag.as_deref().unwrap_or("none")
    ));
    out.push_str(&format!(
        "- Commits Since Previous Tag: {}\n",
        context.commits_since_previous
    ));
    out.push_str(&format!(
        "- Verification Command: {}\n",
        verification.command
    ));
    out.push_str(&format!(
        "- Verification Summary: {}\n",
        verification.summary
    ));
    out.push_str(&format!(
        "- Generated: {}\n\n",
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    ));

    out.push_str("## Changelog\n\n");
    out.push_str(&context.metadata.changelog_heading);
    out.push_str("\n\n");
    out.push_str(&context.metadata.changelog_body);
    out.push_str("\n\n");

    out.push_str("## Included Commits\n\n");
    if context.commit_summaries.is_empty() {
        out.push_str("- No commits were found in the selected release window.\n");
    } else {
        for commit in &context.commit_summaries {
            out.push_str("- ");
            out.push_str(commit);
            out.push('\n');
        }
    }

    out
}

fn render_release_readiness_report(report: &ReleaseReadinessReport) -> String {
    let mut out = String::new();
    out.push_str("# Release Readiness\n\n");
    out.push_str(&format!(
        "- Status: {}\n",
        if report.ready() { "ready" } else { "blocked" }
    ));
    if let Some(package_name) = report.package_name.as_deref() {
        out.push_str(&format!("- Package: {package_name}\n"));
    }
    if let Some(version) = report.version.as_deref() {
        out.push_str(&format!("- Version: {version}\n"));
    }
    if let Some(tag) = report.proposed_tag.as_deref() {
        out.push_str(&format!("- Proposed Tag: {tag}\n"));
    }
    if let Some(current_commit) = report.current_commit.as_deref() {
        out.push_str(&format!("- Current Commit: {current_commit}\n"));
    }
    if let Some(branch) = report.branch.as_deref() {
        out.push_str(&format!("- Branch: {branch}\n"));
    }
    if let Some(previous_tag) = report.previous_tag.as_deref() {
        out.push_str(&format!("- Previous Tag: {previous_tag}\n"));
    }
    if let Some(commits_since_previous) = report.commits_since_previous {
        out.push_str(&format!(
            "- Commits Since Previous Tag: {commits_since_previous}\n"
        ));
    }
    if let Some(command) = report.verification_command.as_deref() {
        out.push_str(&format!("- Verification Command: {command}\n"));
    }
    if let Some(summary) = report.verification_summary.as_deref() {
        out.push_str(&format!("- Verification Summary: {summary}\n"));
    }

    out.push_str("\n## GitHub Verification Feedback\n\n");
    render_github_feedback_section(&mut out, &report.github_feedback);

    out.push_str("\n## Recently Merged Tasks\n\n");
    if report.recently_merged_task_ids.is_empty() {
        out.push_str("- none recorded\n");
    } else {
        for task_id in &report.recently_merged_task_ids {
            out.push_str(&format!("- #{task_id}\n"));
        }
    }

    out.push_str("\n## Blockers\n\n");
    if report.blockers.is_empty() {
        out.push_str("- none\n");
    } else {
        for blocker in &report.blockers {
            out.push_str("- ");
            out.push_str(blocker);
            out.push('\n');
        }
    }

    out
}

fn render_github_feedback_section(out: &mut String, feedback: &GithubReleaseFeedbackSummary) {
    if let Some(commit) = feedback.current_commit.as_deref() {
        out.push_str(&format!("- Current Commit: {commit}\n"));
    }
    if feedback.clean {
        out.push_str(
            "- Current Feedback: clean (no failing or warning GitHub feedback for HEAD)\n",
        );
    } else {
        out.push_str("- Current Feedback: attention needed\n");
    }

    out.push_str("\n### Failing Checks\n\n");
    render_github_feedback_items(out, &feedback.failing, "none");

    out.push_str("\n### Warning-Only Feedback\n\n");
    render_github_feedback_items(out, &feedback.warnings, "none");

    out.push_str("\n### Stale Feedback\n\n");
    if feedback.stale.is_empty() {
        out.push_str("- none\n");
    } else {
        for item in &feedback.stale {
            out.push_str("- ");
            out.push_str(&format_github_feedback_item(item));
            out.push_str(" (stale: not for HEAD");
            if let Some(current_commit) = feedback.current_commit.as_deref() {
                out.push_str(&format!(" {}", short_git_ref(current_commit)));
            }
            out.push_str(")\n");
        }
    }
}

fn render_github_feedback_items(
    out: &mut String,
    items: &[GithubReleaseFeedbackItem],
    empty_label: &str,
) {
    if items.is_empty() {
        out.push_str("- ");
        out.push_str(empty_label);
        out.push('\n');
        return;
    }
    for item in items {
        out.push_str("- ");
        out.push_str(&format_github_feedback_item(item));
        out.push('\n');
    }
}

fn format_github_feedback_item(item: &GithubReleaseFeedbackItem) -> String {
    let commit = item
        .commit
        .as_deref()
        .map(short_git_ref)
        .unwrap_or("unknown");
    let mut line = format!("{}: {} on {}", item.check_name, item.status, commit);
    if let Some(age_secs) = item.age_secs {
        line.push_str(&format!(" ({})", format_release_feedback_age(age_secs)));
    }
    if let Some(details) = item
        .details
        .as_deref()
        .filter(|details| !details.is_empty())
    {
        line.push_str(&format!(" - {details}"));
    }
    if let Some(next_action) = item
        .next_action
        .as_deref()
        .filter(|next_action| !next_action.is_empty())
    {
        line.push_str(&format!(" Next: {next_action}"));
    }
    line
}

fn format_release_feedback_age(age_secs: u64) -> String {
    match age_secs {
        0..=59 => format!("{age_secs}s old"),
        60..=3_599 => format!("{}m old", age_secs / 60),
        3_600..=86_399 => format!("{}h old", age_secs / 3_600),
        _ => format!("{}d old", age_secs / 86_400),
    }
}

fn short_git_ref(value: &str) -> &str {
    value.get(..7).unwrap_or(value)
}

fn render_attempt_report(record: &ReleaseRecord) -> String {
    let mut out = String::new();
    out.push_str("# Release Attempt\n\n");
    out.push_str(&format!(
        "- Status: {}\n",
        if record.success { "success" } else { "failure" }
    ));
    if let Some(package_name) = record.package_name.as_deref() {
        out.push_str(&format!("- Package: {package_name}\n"));
    }
    if let Some(version) = record.version.as_deref() {
        out.push_str(&format!("- Version: {version}\n"));
    }
    if let Some(tag) = record.tag.as_deref() {
        out.push_str(&format!("- Tag: {tag}\n"));
    }
    if let Some(git_ref) = record.git_ref.as_deref() {
        out.push_str(&format!("- Git Ref: {git_ref}\n"));
    }
    if let Some(branch) = record.branch.as_deref() {
        out.push_str(&format!("- Branch: {branch}\n"));
    }
    if let Some(previous_tag) = record.previous_tag.as_deref() {
        out.push_str(&format!("- Previous Tag: {previous_tag}\n"));
    }
    if let Some(commits_since_previous) = record.commits_since_previous {
        out.push_str(&format!(
            "- Commits Since Previous Tag: {commits_since_previous}\n"
        ));
    }
    if let Some(command) = record.verification_command.as_deref() {
        out.push_str(&format!("- Verification Command: {command}\n"));
    }
    if let Some(summary) = record.verification_summary.as_deref() {
        out.push_str(&format!("- Verification Summary: {summary}\n"));
    }
    out.push_str(&format!("- Timestamp: {}\n\n", record.ts));
    out.push_str("## Outcome\n\n");
    out.push_str(&record.reason);
    out.push_str("\n\n");
    if let Some(details) = record.details.as_deref() {
        out.push_str("## Details\n\n");
        out.push_str("```\n");
        out.push_str(details.trim());
        out.push_str("\n```\n");
    }
    if let Some(path) = record.notes_path.as_deref() {
        out.push_str("\n## Release Notes Path\n\n");
        out.push_str(path);
        out.push('\n');
    }
    out
}

fn persist_release_readiness(
    project_root: &Path,
    report: &ReleaseReadinessReport,
    markdown: &str,
) -> Result<(PathBuf, PathBuf)> {
    let dir = releases_dir(project_root);
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let json_path = dir.join(RELEASE_READINESS_JSON);
    let markdown_path = dir.join(RELEASE_READINESS_MARKDOWN);
    fs::write(&json_path, serde_json::to_vec_pretty(report)?)
        .with_context(|| format!("failed to write {}", json_path.display()))?;
    fs::write(&markdown_path, markdown)
        .with_context(|| format!("failed to write {}", markdown_path.display()))?;
    Ok((json_path, markdown_path))
}

fn write_release_notes(
    project_root: &Path,
    context: &ReleaseContext,
    notes: &str,
) -> Result<PathBuf> {
    let dir = releases_dir(project_root);
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let path = dir.join(format!("{}.md", context.metadata.tag));
    fs::write(&path, notes).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

fn write_latest_report(project_root: &Path, report_markdown: &str) -> Result<()> {
    let dir = releases_dir(project_root);
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let latest_path = dir.join(RELEASE_LATEST_MARKDOWN);
    fs::write(&latest_path, report_markdown)
        .with_context(|| format!("failed to write {}", latest_path.display()))?;
    Ok(())
}

fn persist_release_record(project_root: &Path, record: &ReleaseRecord) -> Result<()> {
    let dir = releases_dir(project_root);
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;

    let history_path = dir.join(RELEASE_HISTORY_FILE);
    let mut history = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&history_path)
        .with_context(|| format!("failed to open {}", history_path.display()))?;
    writeln!(history, "{}", serde_json::to_string(record)?)
        .with_context(|| format!("failed to append {}", history_path.display()))?;

    let latest_path = dir.join(RELEASE_LATEST_JSON);
    fs::write(&latest_path, serde_json::to_vec_pretty(record)?)
        .with_context(|| format!("failed to write {}", latest_path.display()))?;
    Ok(())
}

fn emit_release_record(project_root: &Path, record: &ReleaseRecord) -> Result<()> {
    let event = if record.success {
        TeamEvent::release_succeeded(
            record.version.as_deref().unwrap_or("unknown"),
            record.git_ref.as_deref().unwrap_or("unknown"),
            record.tag.as_deref().unwrap_or("unknown"),
            record.notes_path.as_deref(),
        )
    } else {
        TeamEvent::release_failed(
            record.version.as_deref(),
            record.git_ref.as_deref(),
            record.tag.as_deref(),
            &record.reason,
            record.details.as_deref(),
        )
    };

    let mut sink = EventSink::new(&crate::team::team_events_path(project_root))?;
    sink.emit(event.clone())?;

    let conn = crate::team::telemetry_db::open(project_root)?;
    crate::team::telemetry_db::insert_event(&conn, &event)?;
    Ok(())
}

fn success_record(
    context: &ReleaseContext,
    verification: &ReleaseVerification,
    notes_path: &Path,
) -> ReleaseRecord {
    ReleaseRecord {
        ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        package_name: Some(context.metadata.package_name.clone()),
        version: Some(context.metadata.version.clone()),
        tag: Some(context.metadata.tag.clone()),
        git_ref: Some(context.git_ref.clone()),
        branch: Some(context.branch.clone()),
        previous_tag: context.previous_tag.clone(),
        commits_since_previous: Some(context.commits_since_previous),
        verification_command: Some(verification.command.clone()),
        verification_summary: Some(verification.summary.clone()),
        success: true,
        reason: format!(
            "created annotated tag `{}` from clean green main",
            context.metadata.tag
        ),
        details: Some(
            serde_json::json!({
                "previous_tag": context.previous_tag,
                "commits_since_previous": context.commits_since_previous,
                "notes_path": notes_path.display().to_string(),
            })
            .to_string(),
        ),
        notes_path: Some(notes_path.display().to_string()),
    }
}

fn failure(
    draft: &ReleaseDraft,
    reason_code: &str,
    message: &str,
    details: Option<String>,
) -> ReleaseFailure {
    let record = ReleaseRecord {
        ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        package_name: draft.package_name.clone(),
        version: draft.version.clone(),
        tag: draft.tag.clone(),
        git_ref: draft.git_ref.clone(),
        branch: draft.branch.clone(),
        previous_tag: draft.previous_tag.clone(),
        commits_since_previous: draft.commits_since_previous,
        verification_command: draft.verification_command.clone(),
        verification_summary: draft.verification_summary.clone(),
        success: false,
        reason: reason_code.to_string(),
        details,
        notes_path: draft.notes_path.clone(),
    };
    let report_markdown = render_attempt_report(&record);
    ReleaseFailure {
        record,
        report_markdown,
        message: message.to_string(),
    }
}

fn trimmed_output(output: &str) -> Option<String> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct StubVerifier {
        result: ReleaseVerification,
        calls: Arc<AtomicUsize>,
    }

    impl VerificationRunner for StubVerifier {
        fn run(&self, _project_root: &Path) -> Result<ReleaseVerification> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.result.clone())
        }
    }

    fn git(repo: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {:?} failed", args);
    }

    fn git_output(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {:?} failed", args);
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn init_repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"batty\"\nversion = \"0.10.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("CHANGELOG.md"),
            "# Changelog\n\n## 0.10.0 - 2026-04-10\n\n- Ship release automation.\n\n## 0.9.0 - 2026-04-07\n\n- Previous release.\n",
        )
        .unwrap();
        fs::write(tmp.path().join("README.md"), "hello\n").unwrap();
        git(tmp.path(), &["init", "-b", "main"]);
        git(tmp.path(), &["config", "user.name", "Batty Tests"]);
        git(tmp.path(), &["config", "user.email", "batty@example.com"]);
        git(tmp.path(), &["add", "."]);
        git(tmp.path(), &["commit", "-m", "Initial commit"]);
        git(tmp.path(), &["tag", "v0.9.0"]);
        fs::write(tmp.path().join("src.txt"), "feature\n").unwrap();
        git(tmp.path(), &["add", "src.txt"]);
        git(tmp.path(), &["commit", "-m", "Add release-ready change"]);
        tmp
    }

    fn passing_verifier(calls: Arc<AtomicUsize>) -> StubVerifier {
        StubVerifier {
            result: ReleaseVerification {
                command: "cargo test".to_string(),
                passed: true,
                summary: "cargo test passed".to_string(),
                details: None,
            },
            calls,
        }
    }

    fn write_merge_event(repo: &Path, task_id: &str) {
        let events_dir = repo.join(".batty").join("team_config");
        fs::create_dir_all(&events_dir).unwrap();
        fs::write(
            events_dir.join("events.jsonl"),
            format!(
                "{{\"event\":\"task_auto_merged\",\"task\":\"{}\",\"branch\":\"eng-1/{}\"}}\n",
                task_id, task_id
            ),
        )
        .unwrap();
    }

    fn write_github_feedback(repo: &Path, check_name: &str, status: &str, commit: &str, ts: u64) {
        crate::team::github_feedback::write_github_feedback_record(
            repo,
            &crate::team::github_feedback::GithubVerificationRecord {
                task_id: 723,
                branch: Some("main".to_string()),
                commit: Some(commit.to_string()),
                check_name: check_name.to_string(),
                status: status.to_string(),
                next_action: Some("inspect GitHub checks".to_string()),
                details: Some("GitHub reported feedback".to_string()),
                ts: Some(ts),
            },
        )
        .unwrap();
    }

    #[test]
    fn release_fails_when_changelog_entry_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"batty\"\nversion = \"0.10.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("CHANGELOG.md"),
            "# Changelog\n\n## 0.9.0\n\n- old.\n",
        )
        .unwrap();
        git(tmp.path(), &["init", "-b", "main"]);
        git(tmp.path(), &["config", "user.name", "Batty Tests"]);
        git(tmp.path(), &["config", "user.email", "batty@example.com"]);
        let calls = Arc::new(AtomicUsize::new(0));
        let verifier = StubVerifier {
            result: ReleaseVerification {
                command: "cargo test".to_string(),
                passed: true,
                summary: "ok".to_string(),
                details: None,
            },
            calls: calls.clone(),
        };

        let error = run_release_with_verifier(tmp.path(), None, &verifier).unwrap_err();
        assert_eq!(error.record.reason, "missing_release_metadata");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn release_fails_when_repo_is_dirty() {
        let tmp = init_repo();
        fs::write(tmp.path().join("README.md"), "dirty\n").unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let verifier = StubVerifier {
            result: ReleaseVerification {
                command: "cargo test".to_string(),
                passed: true,
                summary: "ok".to_string(),
                details: None,
            },
            calls: calls.clone(),
        };

        let error = run_release_with_verifier(tmp.path(), None, &verifier).unwrap_err();
        assert_eq!(error.record.reason, "dirty_main");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn release_fails_when_verification_is_not_green() {
        let tmp = init_repo();
        let calls = Arc::new(AtomicUsize::new(0));
        let verifier = StubVerifier {
            result: ReleaseVerification {
                command: "cargo test".to_string(),
                passed: false,
                summary: "1 tests failed: suite::it_breaks".to_string(),
                details: Some("suite::it_breaks".to_string()),
            },
            calls: calls.clone(),
        };

        let error = run_release_with_verifier(tmp.path(), None, &verifier).unwrap_err();
        assert_eq!(error.record.reason, "verification_failed");
        assert_eq!(error.record.details.as_deref(), Some("suite::it_breaks"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn release_readiness_clean_report_includes_evidence_without_tagging() {
        let tmp = init_repo();
        write_merge_event(tmp.path(), "704");
        let calls = Arc::new(AtomicUsize::new(0));
        let verifier = passing_verifier(calls.clone());

        let (report, markdown) =
            generate_release_readiness_with_verifier(tmp.path(), None, &verifier).unwrap();

        assert!(report.ready());
        assert_eq!(report.proposed_tag.as_deref(), Some("v0.10.0"));
        assert_eq!(report.branch.as_deref(), Some("main"));
        assert_eq!(report.recently_merged_task_ids, vec!["704".to_string()]);
        assert!(report.current_commit.is_some());
        assert!(markdown.contains("# Release Readiness"));
        assert!(markdown.contains("- Status: ready"));
        assert!(markdown.contains("## GitHub Verification Feedback"));
        assert!(markdown.contains(
            "- Current Feedback: clean (no failing or warning GitHub feedback for HEAD)"
        ));
        assert!(markdown.contains("- #704"));
        assert!(markdown.contains("- Verification Summary: cargo test passed"));
        assert_eq!(git_output(tmp.path(), &["tag", "--list", "v0.10.0"]), "");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn release_readiness_blocks_on_failing_github_feedback_for_head() {
        let tmp = init_repo();
        let head = git_output(tmp.path(), &["rev-parse", "HEAD"]);
        write_github_feedback(tmp.path(), "ci/test", "failure", &head, 1_000);
        let verifier = passing_verifier(Arc::new(AtomicUsize::new(0)));

        let (report, markdown) =
            generate_release_readiness_with_verifier(tmp.path(), None, &verifier).unwrap();

        assert!(!report.ready());
        assert_eq!(report.github_feedback.failing.len(), 1);
        assert_eq!(
            report.github_feedback.failing[0].commit.as_deref(),
            Some(head.as_str())
        );
        assert!(
            report
                .blockers
                .iter()
                .any(|blocker| blocker.starts_with("github_feedback_failed:"))
        );
        assert!(markdown.contains("### Failing Checks"));
        assert!(markdown.contains("ci/test: failure on"));
        assert!(markdown.contains(&head[..7]));
    }

    #[test]
    fn release_readiness_reports_warning_only_github_feedback_without_blocking() {
        let tmp = init_repo();
        let head = git_output(tmp.path(), &["rev-parse", "HEAD"]);
        write_github_feedback(tmp.path(), "ci/lint", "warning", &head, 1_000);
        let verifier = passing_verifier(Arc::new(AtomicUsize::new(0)));

        let (report, markdown) =
            generate_release_readiness_with_verifier(tmp.path(), None, &verifier).unwrap();

        assert!(report.ready());
        assert_eq!(report.github_feedback.warnings.len(), 1);
        assert!(report.github_feedback.failing.is_empty());
        assert!(markdown.contains("### Warning-Only Feedback"));
        assert!(markdown.contains("ci/lint: warning on"));
    }

    #[test]
    fn release_readiness_marks_non_head_github_feedback_stale() {
        let tmp = init_repo();
        let head = git_output(tmp.path(), &["rev-parse", "HEAD"]);
        write_github_feedback(tmp.path(), "ci/old", "failure", "deadbee", 1_000);
        let verifier = passing_verifier(Arc::new(AtomicUsize::new(0)));

        let (report, markdown) =
            generate_release_readiness_with_verifier(tmp.path(), None, &verifier).unwrap();

        assert!(report.ready());
        assert!(report.github_feedback.failing.is_empty());
        assert_eq!(report.github_feedback.stale.len(), 1);
        assert_eq!(
            report.github_feedback.stale[0].commit.as_deref(),
            Some("deadbee")
        );
        assert!(report.github_feedback.stale[0].age_secs.is_some());
        assert!(markdown.contains("ci/old: failure on deadbee"));
        assert!(markdown.contains("stale: not for HEAD"));
        assert!(markdown.contains(&head[..7]));
    }

    #[test]
    fn release_readiness_reports_dirty_blocker() {
        let tmp = init_repo();
        fs::write(tmp.path().join("README.md"), "dirty\n").unwrap();
        let verifier = passing_verifier(Arc::new(AtomicUsize::new(0)));

        let (report, markdown) =
            generate_release_readiness_with_verifier(tmp.path(), None, &verifier).unwrap();

        assert!(!report.ready());
        assert!(
            report
                .blockers
                .iter()
                .any(|blocker| blocker.starts_with("dirty_main:"))
        );
        assert!(markdown.contains("## Blockers"));
        assert!(markdown.contains("dirty_main"));
    }

    #[test]
    fn release_readiness_reports_missing_verification_evidence() {
        let tmp = init_repo();
        let verifier = StubVerifier {
            result: ReleaseVerification {
                command: "cargo test".to_string(),
                passed: true,
                summary: String::new(),
                details: None,
            },
            calls: Arc::new(AtomicUsize::new(0)),
        };

        let (report, _) =
            generate_release_readiness_with_verifier(tmp.path(), None, &verifier).unwrap();

        assert!(!report.ready());
        assert!(
            report
                .blockers
                .iter()
                .any(|blocker| blocker.starts_with("missing_verification_evidence:"))
        );
    }

    #[test]
    fn release_readiness_report_format_is_stable() {
        let report = ReleaseReadinessReport {
            package_name: Some("batty".to_string()),
            version: Some("0.10.0".to_string()),
            proposed_tag: Some("v0.10.0".to_string()),
            current_commit: Some("abc123".to_string()),
            branch: Some("main".to_string()),
            previous_tag: Some("v0.9.0".to_string()),
            commits_since_previous: Some(2),
            recently_merged_task_ids: vec!["704".to_string(), "706".to_string()],
            verification_command: Some("cargo test".to_string()),
            verification_summary: Some("ok".to_string()),
            github_feedback: GithubReleaseFeedbackSummary {
                current_commit: Some("abc123".to_string()),
                clean: true,
                failing: Vec::new(),
                warnings: Vec::new(),
                stale: Vec::new(),
            },
            blockers: vec!["dirty_main: 1 uncommitted change(s)".to_string()],
        };

        assert_eq!(
            render_release_readiness_report(&report),
            "# Release Readiness\n\n\
- Status: blocked\n\
- Package: batty\n\
- Version: 0.10.0\n\
- Proposed Tag: v0.10.0\n\
- Current Commit: abc123\n\
- Branch: main\n\
- Previous Tag: v0.9.0\n\
- Commits Since Previous Tag: 2\n\
- Verification Command: cargo test\n\
- Verification Summary: ok\n\n\
## GitHub Verification Feedback\n\n\
- Current Commit: abc123\n\
- Current Feedback: clean (no failing or warning GitHub feedback for HEAD)\n\n\
### Failing Checks\n\n\
- none\n\n\
### Warning-Only Feedback\n\n\
- none\n\n\
### Stale Feedback\n\n\
- none\n\n\
## Recently Merged Tasks\n\n\
- #704\n\
- #706\n\n\
## Blockers\n\n\
- dirty_main: 1 uncommitted change(s)\n"
        );
    }

    #[test]
    fn release_readiness_surfaces_stale_daemon_binary_blocker() {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-b", "main"]);
        git(tmp.path(), &["config", "user.name", "Batty Tests"]);
        git(tmp.path(), &["config", "user.email", "batty@example.com"]);
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src").join("lib.rs"), "pub fn old() {}\n").unwrap();
        git(tmp.path(), &["add", "."]);
        git(tmp.path(), &["commit", "-m", "Initial src commit"]);
        let binary = tmp.path().join("batty-bin");
        fs::write(&binary, "binary\n").unwrap();
        filetime::set_file_mtime(&binary, filetime::FileTime::from_unix_time(1, 0)).unwrap();
        fs::write(tmp.path().join("src").join("lib.rs"), "pub fn new() {}\n").unwrap();
        git(tmp.path(), &["add", "."]);
        git(tmp.path(), &["commit", "-m", "Runtime change"]);

        let blocker = daemon_binary_blocker_for_path(tmp.path(), &binary)
            .unwrap()
            .unwrap();

        assert!(blocker.starts_with("stale_daemon_binary:"));
        assert!(blocker.contains("Runtime change"));
    }

    #[test]
    fn render_release_notes_includes_changelog_and_verification() {
        let context = ReleaseContext {
            metadata: ReleaseMetadata {
                package_name: "batty".to_string(),
                version: "0.10.0".to_string(),
                tag: "v0.10.0".to_string(),
                changelog_heading: "## 0.10.0 - 2026-04-10".to_string(),
                changelog_body: "- Ship release automation.".to_string(),
            },
            branch: "main".to_string(),
            git_ref: "abc123".to_string(),
            previous_tag: Some("v0.9.0".to_string()),
            commits_since_previous: 2,
            commit_summaries: vec!["abc123 Improve release command".to_string()],
        };
        let verification = ReleaseVerification {
            command: "cargo test".to_string(),
            passed: true,
            summary: "cargo test passed".to_string(),
            details: None,
        };

        let notes = render_release_notes(&context, &verification);
        assert!(notes.contains("Version: 0.10.0"));
        assert!(notes.contains("Tag: v0.10.0"));
        assert!(notes.contains("Git Ref: abc123"));
        assert!(notes.contains("Verification Summary: cargo test passed"));
        assert!(notes.contains("## 0.10.0 - 2026-04-10"));
        assert!(notes.contains("abc123 Improve release command"));
    }

    #[test]
    fn release_success_creates_tag_notes_and_recordable_context() {
        let tmp = init_repo();
        let calls = Arc::new(AtomicUsize::new(0));
        let verifier = StubVerifier {
            result: ReleaseVerification {
                command: "cargo test".to_string(),
                passed: true,
                summary: "cargo test passed".to_string(),
                details: None,
            },
            calls: calls.clone(),
        };

        let (record, report_markdown) =
            run_release_with_verifier(tmp.path(), None, &verifier).unwrap();
        let tag = record.tag.clone().unwrap();
        let notes_path = PathBuf::from(record.notes_path.clone().unwrap());

        assert!(record.success);
        assert_eq!(record.version.as_deref(), Some("0.10.0"));
        assert_eq!(record.branch.as_deref(), Some("main"));
        assert_eq!(record.previous_tag.as_deref(), Some("v0.9.0"));
        assert!(notes_path.exists());
        assert_eq!(git_output(tmp.path(), &["tag", "--list", &tag]), tag);
        assert!(report_markdown.contains("## Changelog"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        persist_release_record(tmp.path(), &record).unwrap();
        write_latest_report(tmp.path(), &report_markdown).unwrap();
        emit_release_record(tmp.path(), &record).unwrap();

        let events = fs::read_to_string(
            tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        assert!(events.contains("\"event\":\"release_succeeded\""));
        assert!(events.contains("\"version\":\"0.10.0\""));
        assert!(events.contains("\"git_ref\""));
    }

    #[test]
    fn release_uses_tag_override() {
        let tmp = init_repo();
        let calls = Arc::new(AtomicUsize::new(0));
        let verifier = StubVerifier {
            result: ReleaseVerification {
                command: "cargo test".to_string(),
                passed: true,
                summary: "cargo test passed".to_string(),
                details: None,
            },
            calls: calls.clone(),
        };

        let (record, _) =
            run_release_with_verifier(tmp.path(), Some("batty-2026-04-10"), &verifier).unwrap();
        assert_eq!(record.tag.as_deref(), Some("batty-2026-04-10"));
        assert_eq!(
            git_output(tmp.path(), &["tag", "--list", "batty-2026-04-10"]),
            "batty-2026-04-10"
        );
    }

    #[test]
    fn latest_record_reads_latest_json_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let record = ReleaseRecord {
            ts: "2026-04-10T00:00:00Z".to_string(),
            package_name: Some("batty".to_string()),
            version: Some("0.10.0".to_string()),
            tag: Some("v0.10.0".to_string()),
            git_ref: Some("abc123".to_string()),
            branch: Some("main".to_string()),
            previous_tag: Some("v0.9.0".to_string()),
            commits_since_previous: Some(12),
            verification_command: Some("cargo test".to_string()),
            verification_summary: Some("cargo test passed".to_string()),
            success: true,
            reason: "ok".to_string(),
            details: None,
            notes_path: Some("/tmp/v0.10.0.md".to_string()),
        };
        persist_release_record(tmp.path(), &record).unwrap();

        let loaded = latest_record(tmp.path()).unwrap().unwrap();
        assert_eq!(loaded, record);
    }

    #[test]
    fn configured_verification_runner_respects_override_command() {
        let tmp = init_repo();
        let runner = ConfiguredVerificationRunner {
            command_override: Some(
                "printf 'test result: ok. 1 passed; 0 failed; 0 ignored;\\n'".to_string(),
            ),
        };

        let verification = runner.run(tmp.path()).unwrap();
        assert_eq!(
            verification.command,
            "printf 'test result: ok. 1 passed; 0 failed; 0 ignored;\\n'"
        );
        assert!(verification.passed);
        assert!(verification.summary.contains("1 passed"));
    }
}
