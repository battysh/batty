//! Autonomous evaluator-driven research missions.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::parity::ParityReport;

const RESEARCH_DIR: &str = "research";
const CURRENT_MISSION_FILE: &str = "current.json";
const MISSION_STATE_FILE: &str = "mission.json";
const LEDGER_FILE: &str = "ledger.jsonl";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvaluatorFormat {
    Json,
    ExitCode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KeepPolicy {
    PassOnly,
    ScoreImprovement,
    ParityImprovement,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvaluationResult {
    pub pass: bool,
    pub score: Option<f64>,
    pub parity_pct: Option<u32>,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_secs: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResearchDecision {
    Baseline,
    Keep,
    Discard,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchMission {
    pub id: String,
    pub hypothesis: String,
    pub evaluator_command: String,
    pub evaluator_format: EvaluatorFormat,
    pub keep_policy: KeepPolicy,
    pub max_iterations: u32,
    pub worktree_dir: PathBuf,
    pub baseline: Option<EvaluationResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub iteration: u32,
    pub decision: ResearchDecision,
    pub evaluation: EvaluationResult,
    pub commit: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone)]
pub struct ResearchLedger {
    pub entries: Vec<LedgerEntry>,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MissionPointer {
    mission_id: String,
}

#[derive(Debug, Clone)]
pub struct ResearchStatus {
    pub mission: ResearchMission,
    pub latest_entry: Option<LedgerEntry>,
}

impl ResearchLedger {
    pub fn load(path: PathBuf) -> Result<Self> {
        if !path.exists() {
            return Ok(Self {
                entries: Vec::new(),
                path,
            });
        }

        let file = fs::File::open(&path).with_context(|| format!("failed to open {}", path.display()))?;
        let reader = BufReader::new(file);
        let mut entries = Vec::new();
        for line in reader.lines() {
            let line = line.with_context(|| format!("failed to read {}", path.display()))?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            entries.push(
                serde_json::from_str(trimmed)
                    .with_context(|| format!("failed to parse {}", path.display()))?,
            );
        }
        Ok(Self { entries, path })
    }

    pub fn record(&mut self, entry: LedgerEntry) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("failed to open {}", self.path.display()))?;
        serde_json::to_writer(&mut file, &entry)
            .with_context(|| format!("failed to write {}", self.path.display()))?;
        writeln!(file).with_context(|| format!("failed to write {}", self.path.display()))?;
        self.entries.push(entry);
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn last_kept_commit(&self) -> Option<&str> {
        self.entries
            .iter()
            .rev()
            .find(|entry| matches!(entry.decision, ResearchDecision::Baseline | ResearchDecision::Keep))
            .map(|entry| entry.commit.as_str())
    }

    pub fn latest(&self) -> Option<&LedgerEntry> {
        self.entries.last()
    }
}

#[derive(Debug, Clone)]
pub struct StartResearchOptions {
    pub hypothesis: String,
    pub evaluator_command: String,
    pub evaluator_format: EvaluatorFormat,
    pub keep_policy: KeepPolicy,
    pub max_iterations: u32,
    pub worktree_dir: PathBuf,
}

pub fn start_research(project_root: &Path, options: StartResearchOptions) -> Result<ResearchMission> {
    if !options.worktree_dir.exists() {
        bail!("research worktree does not exist: {}", options.worktree_dir.display());
    }

    let mission_id = mission_id(&options.hypothesis);
    let mission_dir = mission_dir(project_root, &mission_id);
    fs::create_dir_all(&mission_dir)
        .with_context(|| format!("failed to create {}", mission_dir.display()))?;

    let mut mission = ResearchMission {
        id: mission_id.clone(),
        hypothesis: options.hypothesis,
        evaluator_command: options.evaluator_command,
        evaluator_format: options.evaluator_format,
        keep_policy: options.keep_policy,
        max_iterations: options.max_iterations.max(1),
        worktree_dir: options.worktree_dir,
        baseline: None,
    };

    let baseline = run_evaluator(&mission.evaluator_command, &mission.worktree_dir, &mission.evaluator_format)?;
    mission.baseline = Some(baseline.clone());
    let mut ledger = ResearchLedger::load(ledger_path(project_root, &mission.id))?;
    ledger.record(LedgerEntry {
        iteration: 0,
        decision: ResearchDecision::Baseline,
        evaluation: baseline,
        commit: git_head(&mission.worktree_dir)?,
        timestamp: chrono::Utc::now(),
    })?;
    save_mission(project_root, &mission)?;
    set_current_mission(project_root, &mission.id)?;
    Ok(mission)
}

pub fn run_research_iteration(
    mission: &mut ResearchMission,
    ledger: &mut ResearchLedger,
) -> Result<ResearchDecision> {
    let result = run_evaluator(
        &mission.evaluator_command,
        &mission.worktree_dir,
        &mission.evaluator_format,
    )?;

    let decision = match mission.keep_policy {
        KeepPolicy::PassOnly => {
            if result.pass {
                ResearchDecision::Keep
            } else {
                ResearchDecision::Discard
            }
        }
        KeepPolicy::ScoreImprovement => {
            let baseline_score = mission
                .baseline
                .as_ref()
                .and_then(|baseline| baseline.score)
                .unwrap_or(0.0);
            if result.score.unwrap_or(0.0) > baseline_score {
                ResearchDecision::Keep
            } else {
                ResearchDecision::Discard
            }
        }
        KeepPolicy::ParityImprovement => {
            let baseline_parity = mission
                .baseline
                .as_ref()
                .and_then(|baseline| baseline.parity_pct)
                .unwrap_or(0);
            if result.parity_pct.unwrap_or(0) > baseline_parity {
                ResearchDecision::Keep
            } else {
                ResearchDecision::Discard
            }
        }
    };

    match decision {
        ResearchDecision::Keep => {
            git_commit_all(
                &mission.worktree_dir,
                &format!("research: iteration {}", ledger.len()),
            )?;
            mission.baseline = Some(result.clone());
        }
        ResearchDecision::Discard => {
            let Some(commit) = ledger.last_kept_commit() else {
                bail!("cannot discard without a kept or baseline commit");
            };
            git_reset_hard(&mission.worktree_dir, commit)?;
        }
        ResearchDecision::Baseline | ResearchDecision::Error => {}
    }

    let commit = git_head(&mission.worktree_dir)?;
    ledger.record(LedgerEntry {
        iteration: ledger.len() as u32,
        decision: decision.clone(),
        evaluation: result,
        commit,
        timestamp: chrono::Utc::now(),
    })?;

    Ok(decision)
}

pub fn current_status(project_root: &Path) -> Result<Option<ResearchStatus>> {
    let Some(mission) = load_current_mission(project_root)? else {
        return Ok(None);
    };
    let ledger = ResearchLedger::load(ledger_path(project_root, &mission.id))?;
    Ok(Some(ResearchStatus {
        mission,
        latest_entry: ledger.latest().cloned(),
    }))
}

pub fn read_current_ledger(project_root: &Path) -> Result<Option<ResearchLedger>> {
    let Some(mission) = load_current_mission(project_root)? else {
        return Ok(None);
    };
    Ok(Some(ResearchLedger::load(ledger_path(project_root, &mission.id))?))
}

pub fn stop_current_research(project_root: &Path) -> Result<Option<ResearchMission>> {
    let mission = load_current_mission(project_root)?;
    let path = current_mission_path(project_root);
    if path.exists() {
        fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
    }
    Ok(mission)
}

pub fn print_status(project_root: &Path) -> Result<()> {
    let Some(status) = current_status(project_root)? else {
        println!("No active research mission.");
        return Ok(());
    };
    println!("Mission: {}", status.mission.id);
    println!("Hypothesis: {}", status.mission.hypothesis);
    println!("Worktree: {}", status.mission.worktree_dir.display());
    println!("Keep policy: {}", keep_policy_name(&status.mission.keep_policy));
    println!(
        "Baseline: {}",
        status
            .mission
            .baseline
            .as_ref()
            .map(summary_line)
            .unwrap_or_else(|| "none".to_string())
    );
    if let Some(entry) = status.latest_entry {
        println!(
            "Latest: iteration={} decision={} commit={} {}",
            entry.iteration,
            decision_name(&entry.decision),
            entry.commit,
            summary_line(&entry.evaluation)
        );
    }
    Ok(())
}

pub fn print_ledger(project_root: &Path) -> Result<()> {
    let Some(ledger) = read_current_ledger(project_root)? else {
        println!("No active research mission.");
        return Ok(());
    };
    println!("iteration  commit   pass  score  parity  decision");
    for entry in ledger.entries {
        println!(
            "{:<10} {:<8} {:<5} {:<6} {:<7} {}",
            entry.iteration,
            shorten_commit(&entry.commit),
            entry.evaluation.pass,
            entry
                .evaluation
                .score
                .map(|score| format!("{score:.2}"))
                .unwrap_or_else(|| "-".to_string()),
            entry
                .evaluation
                .parity_pct
                .map(|pct| format!("{pct}%"))
                .unwrap_or_else(|| "-".to_string()),
            decision_name(&entry.decision),
        );
    }
    Ok(())
}

fn summary_line(result: &EvaluationResult) -> String {
    format!(
        "pass={} score={} parity={} exit={}",
        result.pass,
        result
            .score
            .map(|score| format!("{score:.2}"))
            .unwrap_or_else(|| "-".to_string()),
        result
            .parity_pct
            .map(|pct| format!("{pct}%"))
            .unwrap_or_else(|| "-".to_string()),
        result.exit_code
    )
}

fn run_evaluator(command: &str, worktree_dir: &Path, format: &EvaluatorFormat) -> Result<EvaluationResult> {
    let started = Instant::now();
    let output = std::process::Command::new("sh")
        .args(["-lc", command])
        .current_dir(worktree_dir)
        .output()
        .with_context(|| format!("failed to execute evaluator `{command}` in {}", worktree_dir.display()))?;
    let duration_secs = started.elapsed().as_secs_f64();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code().unwrap_or(-1);

    match format {
        EvaluatorFormat::Json => {
            #[derive(Deserialize)]
            struct JsonEvaluation {
                pass: Option<bool>,
                score: Option<f64>,
                parity_pct: Option<u32>,
            }

            let parsed: JsonEvaluation = serde_json::from_str(stdout.trim())
                .with_context(|| format!("failed to parse evaluator JSON from `{command}`"))?;
            Ok(EvaluationResult {
                pass: parsed.pass.unwrap_or(output.status.success()),
                score: parsed.score,
                parity_pct: parsed.parity_pct.or_else(|| current_parity_pct(worktree_dir)),
                exit_code,
                stdout,
                stderr,
                duration_secs,
            })
        }
        EvaluatorFormat::ExitCode => Ok(EvaluationResult {
            pass: output.status.success(),
            score: None,
            parity_pct: current_parity_pct(worktree_dir),
            exit_code,
            stdout,
            stderr,
            duration_secs,
        }),
    }
}

fn current_parity_pct(project_root: &Path) -> Option<u32> {
    ParityReport::load(project_root)
        .ok()
        .map(|report| report.summary().overall_parity_pct as u32)
}

fn git_head(worktree_dir: &Path) -> Result<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(worktree_dir)
        .output()
        .with_context(|| format!("failed to read HEAD in {}", worktree_dir.display()))?;
    if !output.status.success() {
        bail!("failed to read HEAD in {}", worktree_dir.display());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_commit_all(worktree_dir: &Path, message: &str) -> Result<()> {
    let add = std::process::Command::new("git")
        .args(["add", "-A"])
        .current_dir(worktree_dir)
        .status()
        .with_context(|| format!("failed to stage worktree {}", worktree_dir.display()))?;
    if !add.success() {
        bail!("failed to stage worktree {}", worktree_dir.display());
    }

    let commit = std::process::Command::new("git")
        .args(["commit", "-m", message])
        .current_dir(worktree_dir)
        .status()
        .with_context(|| format!("failed to commit worktree {}", worktree_dir.display()))?;
    if !commit.success() {
        bail!("failed to commit worktree {}", worktree_dir.display());
    }
    Ok(())
}

fn git_reset_hard(worktree_dir: &Path, target: &str) -> Result<()> {
    let status = std::process::Command::new("git")
        .args(["reset", "--hard", target])
        .current_dir(worktree_dir)
        .status()
        .with_context(|| format!("failed to reset {}", worktree_dir.display()))?;
    if !status.success() {
        bail!("failed to reset {} to {}", worktree_dir.display(), target);
    }
    Ok(())
}

fn mission_dir(project_root: &Path, mission_id: &str) -> PathBuf {
    project_root.join(".batty").join(RESEARCH_DIR).join(mission_id)
}

fn ledger_path(project_root: &Path, mission_id: &str) -> PathBuf {
    mission_dir(project_root, mission_id).join(LEDGER_FILE)
}

fn mission_state_path(project_root: &Path, mission_id: &str) -> PathBuf {
    mission_dir(project_root, mission_id).join(MISSION_STATE_FILE)
}

fn current_mission_path(project_root: &Path) -> PathBuf {
    project_root
        .join(".batty")
        .join(RESEARCH_DIR)
        .join(CURRENT_MISSION_FILE)
}

fn save_mission(project_root: &Path, mission: &ResearchMission) -> Result<()> {
    let path = mission_state_path(project_root, &mission.id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let content = serde_json::to_vec_pretty(mission)
        .with_context(|| format!("failed to serialize {}", mission.id))?;
    fs::write(&path, content).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn load_current_mission(project_root: &Path) -> Result<Option<ResearchMission>> {
    let current = current_mission_path(project_root);
    if !current.exists() {
        return Ok(None);
    }
    let pointer: MissionPointer = serde_json::from_slice(
        &fs::read(&current).with_context(|| format!("failed to read {}", current.display()))?,
    )
    .with_context(|| format!("failed to parse {}", current.display()))?;
    let state_path = mission_state_path(project_root, &pointer.mission_id);
    let mission = serde_json::from_slice(
        &fs::read(&state_path).with_context(|| format!("failed to read {}", state_path.display()))?,
    )
    .with_context(|| format!("failed to parse {}", state_path.display()))?;
    Ok(Some(mission))
}

fn set_current_mission(project_root: &Path, mission_id: &str) -> Result<()> {
    let path = current_mission_path(project_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let content = serde_json::to_vec_pretty(&MissionPointer {
        mission_id: mission_id.to_string(),
    })?;
    fs::write(&path, content).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn mission_id(hypothesis: &str) -> String {
    let slug: String = hypothesis
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let compact = slug
        .split('-')
        .filter(|part| !part.is_empty())
        .take(6)
        .collect::<Vec<_>>()
        .join("-");
    format!("{}-{}", compact, chrono::Utc::now().timestamp())
}

fn shorten_commit(commit: &str) -> String {
    commit.chars().take(7).collect()
}

fn decision_name(decision: &ResearchDecision) -> &'static str {
    match decision {
        ResearchDecision::Baseline => "baseline",
        ResearchDecision::Keep => "keep",
        ResearchDecision::Discard => "discard",
        ResearchDecision::Error => "error",
    }
}

fn keep_policy_name(policy: &KeepPolicy) -> &'static str {
    match policy {
        KeepPolicy::PassOnly => "pass-only",
        KeepPolicy::ScoreImprovement => "score-improvement",
        KeepPolicy::ParityImprovement => "parity-improvement",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "git {:?} failed", args);
    }

    fn repo_with_file() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init"]);
        git(tmp.path(), &["config", "user.email", "test@example.com"]);
        git(tmp.path(), &["config", "user.name", "Test User"]);
        fs::write(tmp.path().join("note.txt"), "baseline\n").unwrap();
        git(tmp.path(), &["add", "note.txt"]);
        git(tmp.path(), &["commit", "-m", "baseline"]);
        tmp
    }

    fn baseline_result(score: Option<f64>, parity_pct: Option<u32>, pass: bool) -> EvaluationResult {
        EvaluationResult {
            pass,
            score,
            parity_pct,
            exit_code: if pass { 0 } else { 1 },
            stdout: String::new(),
            stderr: String::new(),
            duration_secs: 0.0,
        }
    }

    #[test]
    fn pass_only_policy_discards_failures() {
        let tmp = repo_with_file();
        let baseline_commit = git_head(tmp.path()).unwrap();
        fs::write(tmp.path().join("note.txt"), "candidate\n").unwrap();
        let mut mission = ResearchMission {
            id: "mission".to_string(),
            hypothesis: "pass only".to_string(),
            evaluator_command: "printf '{\"pass\":false}' && exit 1".to_string(),
            evaluator_format: EvaluatorFormat::Json,
            keep_policy: KeepPolicy::PassOnly,
            max_iterations: 3,
            worktree_dir: tmp.path().to_path_buf(),
            baseline: Some(baseline_result(None, None, true)),
        };
        let mut ledger = ResearchLedger {
            entries: vec![LedgerEntry {
                iteration: 0,
                decision: ResearchDecision::Baseline,
                evaluation: baseline_result(None, None, true),
                commit: baseline_commit.clone(),
                timestamp: chrono::Utc::now(),
            }],
            path: tmp.path().join("ledger.jsonl"),
        };

        let decision = run_research_iteration(&mut mission, &mut ledger).unwrap();
        assert_eq!(decision, ResearchDecision::Discard);
        assert_eq!(git_head(tmp.path()).unwrap(), baseline_commit);
    }

    #[test]
    fn start_research_records_baseline_entry() {
        let root = tempfile::tempdir().unwrap();
        let worktree = repo_with_file();

        let mission = start_research(
            root.path(),
            StartResearchOptions {
                hypothesis: "record baseline".to_string(),
                evaluator_command: "printf '{\"pass\":true,\"score\":1.0}'".to_string(),
                evaluator_format: EvaluatorFormat::Json,
                keep_policy: KeepPolicy::ScoreImprovement,
                max_iterations: 10,
                worktree_dir: worktree.path().to_path_buf(),
            },
        )
        .unwrap();

        let current = current_status(root.path()).unwrap().unwrap();
        assert_eq!(current.mission.id, mission.id);
        assert_eq!(
            current
                .latest_entry
                .as_ref()
                .map(|entry| entry.decision.clone()),
            Some(ResearchDecision::Baseline)
        );

        let ledger = read_current_ledger(root.path()).unwrap().unwrap();
        assert_eq!(ledger.entries.len(), 1);
        assert_eq!(ledger.entries[0].evaluation.score, Some(1.0));
    }

    #[test]
    fn score_improvement_keeps_and_commits() {
        let tmp = repo_with_file();
        fs::write(tmp.path().join("note.txt"), "candidate\n").unwrap();
        let mut mission = ResearchMission {
            id: "mission".to_string(),
            hypothesis: "improve score".to_string(),
            evaluator_command: "printf '{\"pass\":true,\"score\":2.0}'".to_string(),
            evaluator_format: EvaluatorFormat::Json,
            keep_policy: KeepPolicy::ScoreImprovement,
            max_iterations: 3,
            worktree_dir: tmp.path().to_path_buf(),
            baseline: Some(baseline_result(Some(1.0), None, true)),
        };
        let mut ledger = ResearchLedger {
            entries: vec![LedgerEntry {
                iteration: 0,
                decision: ResearchDecision::Baseline,
                evaluation: baseline_result(Some(1.0), None, true),
                commit: git_head(tmp.path()).unwrap(),
                timestamp: chrono::Utc::now(),
            }],
            path: tmp.path().join("ledger.jsonl"),
        };

        let decision = run_research_iteration(&mut mission, &mut ledger).unwrap();
        assert_eq!(decision, ResearchDecision::Keep);
        assert_eq!(mission.baseline.as_ref().and_then(|result| result.score), Some(2.0));
        assert_eq!(ledger.entries.len(), 2);
    }

    #[test]
    fn discard_resets_to_last_kept_commit() {
        let tmp = repo_with_file();
        let baseline_commit = git_head(tmp.path()).unwrap();
        fs::write(tmp.path().join("note.txt"), "bad candidate\n").unwrap();
        let mut mission = ResearchMission {
            id: "mission".to_string(),
            hypothesis: "avoid regression".to_string(),
            evaluator_command: "printf '{\"pass\":true,\"score\":1.0}'".to_string(),
            evaluator_format: EvaluatorFormat::Json,
            keep_policy: KeepPolicy::ScoreImprovement,
            max_iterations: 3,
            worktree_dir: tmp.path().to_path_buf(),
            baseline: Some(baseline_result(Some(2.0), None, true)),
        };
        let mut ledger = ResearchLedger {
            entries: vec![LedgerEntry {
                iteration: 0,
                decision: ResearchDecision::Baseline,
                evaluation: baseline_result(Some(2.0), None, true),
                commit: baseline_commit.clone(),
                timestamp: chrono::Utc::now(),
            }],
            path: tmp.path().join("ledger.jsonl"),
        };

        let decision = run_research_iteration(&mut mission, &mut ledger).unwrap();
        assert_eq!(decision, ResearchDecision::Discard);
        assert_eq!(fs::read_to_string(tmp.path().join("note.txt")).unwrap(), "baseline\n");
        assert_eq!(git_head(tmp.path()).unwrap(), baseline_commit);
    }

    #[test]
    fn parity_improvement_uses_parity_report() {
        let tmp = repo_with_file();
        fs::write(
            tmp.path().join("PARITY.md"),
            concat!(
                "---\n",
                "project: trivial\n",
                "target: trivial.z80\n",
                "source_platform: zx-spectrum-z80\n",
                "target_language: rust\n",
                "last_verified: 2026-04-06\n",
                "overall_parity: 50%\n",
                "---\n\n",
                "| Behavior | Spec | Test | Implementation | Verified | Notes |\n",
                "| --- | --- | --- | --- | --- | --- |\n",
                "| Startup | complete | complete | complete | PASS | ok |\n",
                "| Errors | complete | complete | draft | -- | pending |\n",
            ),
        )
        .unwrap();

        let result = run_evaluator("true", tmp.path(), &EvaluatorFormat::ExitCode).unwrap();
        assert_eq!(result.parity_pct, Some(50));
    }
}
