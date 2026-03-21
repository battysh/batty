use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;
use tracing::warn;

use super::config::{ModelPricing, TeamConfig};
use super::hierarchy::{self, MemberInstance};
use super::{daemon_state_path, status, team_config_path};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TokenUsage {
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_creation_5m_input_tokens: u64,
    cache_creation_1h_input_tokens: u64,
    cache_read_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
}

impl TokenUsage {
    fn total_tokens(&self) -> u64 {
        self.input_tokens
            + self.cached_input_tokens
            + self.cache_creation_input_tokens
            + self.cache_read_input_tokens
            + self.output_tokens
            + self.reasoning_output_tokens
    }

    fn display_cache_tokens(&self) -> u64 {
        self.cached_input_tokens + self.cache_creation_input_tokens + self.cache_read_input_tokens
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionUsage {
    model: Option<String>,
    usage: TokenUsage,
}

#[derive(Debug, Clone, PartialEq)]
struct CostEntry {
    member_name: String,
    agent: String,
    model: String,
    task: String,
    session_file: PathBuf,
    usage: TokenUsage,
    estimated_cost_usd: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
struct CostReport {
    team_name: String,
    entries: Vec<CostEntry>,
    total_estimated_cost_usd: f64,
    unpriced_models: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum SessionAgent {
    Codex,
    Claude,
}

#[derive(Debug, Clone)]
struct SessionRoots {
    codex_sessions_root: PathBuf,
    claude_projects_root: PathBuf,
}

#[derive(Debug, Deserialize, Default)]
struct LaunchIdentityRecord {
    #[serde(default)]
    session_id: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct PersistedDaemonStateCostView {
    #[serde(default)]
    active_tasks: HashMap<String, u32>,
}

pub fn show_cost(project_root: &Path) -> Result<()> {
    let config_path = team_config_path(project_root);
    if !config_path.exists() {
        bail!("no team config found at {}", config_path.display());
    }

    let team_config = TeamConfig::load(&config_path)?;
    let report = collect_cost_report(project_root, &team_config, &SessionRoots::default())?;

    println!("Run cost estimate for team {}", report.team_name);
    if report.entries.is_empty() {
        println!("No agent session files with token usage were found.");
        return Ok(());
    }

    println!();
    println!(
        "{:<20} {:<12} {:<20} {:<10} {:>10} {:>10} {:>10} {:>12}",
        "MEMBER", "AGENT", "MODEL", "TASK", "INPUT", "CACHE", "OUTPUT", "COST"
    );
    println!("{}", "-".repeat(112));
    for entry in &report.entries {
        println!(
            "{:<20} {:<12} {:<20} {:<10} {:>10} {:>10} {:>10} {:>12}",
            entry.member_name,
            entry.agent,
            truncate_model(&entry.model),
            entry.task,
            entry.usage.input_tokens,
            entry.usage.display_cache_tokens(),
            entry.usage.output_tokens + entry.usage.reasoning_output_tokens,
            entry
                .estimated_cost_usd
                .map(|cost| format!("${cost:.4}"))
                .unwrap_or_else(|| "n/a".to_string()),
        );
    }

    println!();
    println!("Estimated total: ${:.4}", report.total_estimated_cost_usd);
    if !report.unpriced_models.is_empty() {
        println!(
            "Unpriced models: {}",
            report
                .unpriced_models
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    Ok(())
}

impl Default for SessionRoots {
    fn default() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        Self {
            codex_sessions_root: home.join(".codex").join("sessions"),
            claude_projects_root: home.join(".claude").join("projects"),
        }
    }
}

fn collect_cost_report(
    project_root: &Path,
    team_config: &TeamConfig,
    session_roots: &SessionRoots,
) -> Result<CostReport> {
    let members = hierarchy::resolve_hierarchy(team_config)?;
    let launch_state = load_launch_state(project_root);
    let active_tasks = load_active_tasks(project_root);
    let owned_task_buckets = status::owned_task_buckets(project_root, &members);
    let mut session_target_counts = HashMap::<(SessionAgent, PathBuf), usize>::new();
    for member in &members {
        let Some((agent_kind, session_cwd, _)) = member_session_target(project_root, member) else {
            continue;
        };
        *session_target_counts
            .entry((agent_kind, session_cwd))
            .or_insert(0usize) += 1;
    }
    let mut entries = Vec::new();
    let mut total_estimated_cost_usd = 0.0;
    let mut unpriced_models = BTreeSet::new();

    for member in &members {
        let Some((agent_kind, session_cwd, agent_label)) =
            member_session_target(project_root, member)
        else {
            continue;
        };
        let session_id = launch_state
            .get(&member.name)
            .and_then(|identity| identity.session_id.as_deref());
        let allow_cwd_fallback = session_id.is_some()
            || session_target_counts
                .get(&(agent_kind, session_cwd.clone()))
                .copied()
                .unwrap_or(0)
                <= 1;
        let session_file = match agent_kind {
            SessionAgent::Codex => discover_codex_session_file(
                &session_roots.codex_sessions_root,
                &session_cwd,
                session_id,
                allow_cwd_fallback,
            )?,
            SessionAgent::Claude => discover_claude_session_file(
                &session_roots.claude_projects_root,
                &session_cwd,
                session_id,
                allow_cwd_fallback,
            )?,
        };
        let Some(session_file) = session_file else {
            continue;
        };

        let session_usage = match agent_kind {
            SessionAgent::Codex => parse_codex_session_usage(&session_file)?,
            SessionAgent::Claude => parse_claude_session_usage(&session_file)?,
        };
        let Some(session_usage) = session_usage else {
            continue;
        };
        if session_usage.usage.total_tokens() == 0 {
            continue;
        }

        let model = session_usage.model.unwrap_or_else(|| "unknown".to_string());
        let estimated_cost_usd = pricing_for_model(&team_config.cost.models, &model)
            .map(|pricing| estimate_cost_usd(&session_usage.usage, &pricing));
        if let Some(cost) = estimated_cost_usd {
            total_estimated_cost_usd += cost;
        } else {
            unpriced_models.insert(model.clone());
        }

        let task = active_tasks
            .get(&member.name)
            .map(|task_id| format!("#{task_id}"))
            .or_else(|| {
                owned_task_buckets
                    .get(&member.name)
                    .map(|buckets| status::format_owned_tasks_summary(&buckets.active))
            })
            .unwrap_or_else(|| "-".to_string());

        entries.push(CostEntry {
            member_name: member.name.clone(),
            agent: agent_label.to_string(),
            model,
            task,
            session_file,
            usage: session_usage.usage,
            estimated_cost_usd,
        });
    }

    entries.sort_by(|left, right| left.member_name.cmp(&right.member_name));

    Ok(CostReport {
        team_name: team_config.name.clone(),
        entries,
        total_estimated_cost_usd,
        unpriced_models,
    })
}

fn truncate_model(model: &str) -> String {
    const MAX_LEN: usize = 20;
    if model.chars().count() <= MAX_LEN {
        model.to_string()
    } else {
        let short = model.chars().take(MAX_LEN - 3).collect::<String>();
        format!("{short}...")
    }
}

fn member_session_target(
    project_root: &Path,
    member: &MemberInstance,
) -> Option<(SessionAgent, PathBuf, &'static str)> {
    if member.role_type == super::config::RoleType::User {
        return None;
    }

    let work_dir = if member.use_worktrees {
        project_root
            .join(".batty")
            .join("worktrees")
            .join(&member.name)
    } else {
        project_root.to_path_buf()
    };

    match member.agent.as_deref() {
        Some("codex") | Some("codex-cli") => Some((
            SessionAgent::Codex,
            work_dir
                .join(".batty")
                .join("codex-context")
                .join(&member.name),
            "codex",
        )),
        Some("claude") | Some("claude-code") | None => {
            Some((SessionAgent::Claude, work_dir, "claude"))
        }
        _ => None,
    }
}

fn load_launch_state(project_root: &Path) -> HashMap<String, LaunchIdentityRecord> {
    let path = project_root.join(".batty").join("launch-state.json");
    let Ok(content) = fs::read_to_string(&path) else {
        return HashMap::new();
    };
    match serde_json::from_str::<HashMap<String, LaunchIdentityRecord>>(&content) {
        Ok(state) => state,
        Err(error) => {
            warn!(path = %path.display(), error = %error, "failed to parse launch state for cost reporting");
            HashMap::new()
        }
    }
}

fn load_active_tasks(project_root: &Path) -> HashMap<String, u32> {
    let path = daemon_state_path(project_root);
    let Ok(content) = fs::read_to_string(&path) else {
        return HashMap::new();
    };
    match serde_json::from_str::<PersistedDaemonStateCostView>(&content) {
        Ok(state) => state.active_tasks,
        Err(error) => {
            warn!(path = %path.display(), error = %error, "failed to parse daemon state for cost reporting");
            HashMap::new()
        }
    }
}

fn pricing_for_model(
    overrides: &HashMap<String, ModelPricing>,
    model: &str,
) -> Option<ModelPricing> {
    let normalized = model.to_ascii_lowercase();
    if let Some(pricing) = overrides.get(&normalized) {
        return Some(pricing.clone());
    }
    if let Some(pricing) = overrides.get(model) {
        return Some(pricing.clone());
    }
    built_in_model_pricing(&normalized)
}

fn built_in_model_pricing(model: &str) -> Option<ModelPricing> {
    // Defaults keep the command useful out of the box. team.yaml can override any model entry.
    if model.starts_with("gpt-5.4") {
        return Some(ModelPricing {
            input_usd_per_mtok: 2.5,
            cached_input_usd_per_mtok: 0.25,
            cache_creation_input_usd_per_mtok: None,
            cache_creation_5m_input_usd_per_mtok: None,
            cache_creation_1h_input_usd_per_mtok: None,
            cache_read_input_usd_per_mtok: 0.0,
            output_usd_per_mtok: 15.0,
            reasoning_output_usd_per_mtok: Some(15.0),
        });
    }
    if model.starts_with("claude-opus-4") {
        return Some(ModelPricing {
            input_usd_per_mtok: 15.0,
            cached_input_usd_per_mtok: 0.0,
            cache_creation_input_usd_per_mtok: None,
            cache_creation_5m_input_usd_per_mtok: Some(18.75),
            cache_creation_1h_input_usd_per_mtok: Some(30.0),
            cache_read_input_usd_per_mtok: 1.5,
            output_usd_per_mtok: 75.0,
            reasoning_output_usd_per_mtok: None,
        });
    }
    if model.starts_with("claude-sonnet-4") {
        return Some(ModelPricing {
            input_usd_per_mtok: 3.0,
            cached_input_usd_per_mtok: 0.0,
            cache_creation_input_usd_per_mtok: None,
            cache_creation_5m_input_usd_per_mtok: Some(3.75),
            cache_creation_1h_input_usd_per_mtok: Some(6.0),
            cache_read_input_usd_per_mtok: 0.3,
            output_usd_per_mtok: 15.0,
            reasoning_output_usd_per_mtok: None,
        });
    }
    None
}

fn estimate_cost_usd(usage: &TokenUsage, pricing: &ModelPricing) -> f64 {
    let classified_cache_creation =
        usage.cache_creation_5m_input_tokens + usage.cache_creation_1h_input_tokens;
    let unclassified_cache_creation = usage
        .cache_creation_input_tokens
        .saturating_sub(classified_cache_creation);
    let reasoning_rate = pricing
        .reasoning_output_usd_per_mtok
        .unwrap_or(pricing.output_usd_per_mtok);
    let cache_creation_generic_rate = pricing
        .cache_creation_input_usd_per_mtok
        .or(pricing.cache_creation_5m_input_usd_per_mtok)
        .unwrap_or(pricing.input_usd_per_mtok);

    let total_usd = (usage.input_tokens as f64 * pricing.input_usd_per_mtok)
        + (usage.cached_input_tokens as f64 * pricing.cached_input_usd_per_mtok)
        + (usage.cache_creation_5m_input_tokens as f64
            * pricing
                .cache_creation_5m_input_usd_per_mtok
                .unwrap_or(cache_creation_generic_rate))
        + (usage.cache_creation_1h_input_tokens as f64
            * pricing
                .cache_creation_1h_input_usd_per_mtok
                .unwrap_or(cache_creation_generic_rate))
        + (unclassified_cache_creation as f64 * cache_creation_generic_rate)
        + (usage.cache_read_input_tokens as f64 * pricing.cache_read_input_usd_per_mtok)
        + (usage.output_tokens as f64 * pricing.output_usd_per_mtok)
        + (usage.reasoning_output_tokens as f64 * reasoning_rate);

    total_usd / 1_000_000.0
}

fn discover_codex_session_file(
    sessions_root: &Path,
    cwd: &Path,
    session_id: Option<&str>,
    allow_cwd_fallback: bool,
) -> Result<Option<PathBuf>> {
    if !sessions_root.exists() {
        return Ok(None);
    }

    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for year in read_dir_paths(sessions_root)? {
        for month in read_dir_paths(&year)? {
            for day in read_dir_paths(&month)? {
                for entry in read_dir_paths(&day)? {
                    if entry.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                        continue;
                    }
                    let Some(meta) = read_codex_session_meta(&entry)? else {
                        continue;
                    };
                    if meta.cwd.as_deref() != Some(cwd.as_os_str()) {
                        continue;
                    }
                    if let Some(wanted) = session_id
                        && meta.id.as_deref() == Some(wanted)
                    {
                        return Ok(Some(entry));
                    }
                    let modified = fs::metadata(&entry)
                        .and_then(|metadata| metadata.modified())
                        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                    match &newest {
                        Some((current, _)) if modified <= *current => {}
                        _ => newest = Some((modified, entry)),
                    }
                }
            }
        }
    }

    Ok(allow_cwd_fallback
        .then_some(newest)
        .flatten()
        .map(|(_, path)| path))
}

fn discover_claude_session_file(
    projects_root: &Path,
    cwd: &Path,
    session_id: Option<&str>,
    allow_cwd_fallback: bool,
) -> Result<Option<PathBuf>> {
    if !projects_root.exists() {
        return Ok(None);
    }

    let preferred_dir = projects_root.join(cwd.to_string_lossy().replace('/', "-"));
    if let Some(session_id) = session_id {
        let exact = preferred_dir.join(format!("{session_id}.jsonl"));
        if exact.is_file() {
            return Ok(Some(exact));
        }
    }

    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    if preferred_dir.is_dir() {
        for entry in read_dir_paths(&preferred_dir)? {
            if entry.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let modified = fs::metadata(&entry)
                .and_then(|metadata| metadata.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            match &newest {
                Some((current, _)) if modified <= *current => {}
                _ => newest = Some((modified, entry)),
            }
        }
    }

    Ok(allow_cwd_fallback
        .then_some(newest)
        .flatten()
        .map(|(_, path)| path))
}

#[derive(Debug)]
struct CodexSessionMeta {
    id: Option<String>,
    cwd: Option<std::ffi::OsString>,
}

fn read_codex_session_meta(path: &Path) -> Result<Option<CodexSessionMeta>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if entry.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let payload = entry.get("payload");
        return Ok(Some(CodexSessionMeta {
            id: payload
                .and_then(|payload| payload.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string),
            cwd: payload
                .and_then(|payload| payload.get("cwd"))
                .and_then(Value::as_str)
                .map(std::ffi::OsString::from),
        }));
    }
    Ok(None)
}

fn parse_codex_session_usage(path: &Path) -> Result<Option<SessionUsage>> {
    let file = File::open(path)
        .with_context(|| format!("failed to open codex session {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut usage = TokenUsage::default();
    let mut model = None;

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(&line) else {
            continue;
        };

        if entry.get("type").and_then(Value::as_str) == Some("turn_context")
            && let Some(value) = entry
                .get("payload")
                .and_then(|payload| payload.get("model"))
                .and_then(Value::as_str)
        {
            model = Some(value.to_string());
        }

        if entry.get("type").and_then(Value::as_str) != Some("event_msg")
            || entry
                .get("payload")
                .and_then(|payload| payload.get("type"))
                .and_then(Value::as_str)
                != Some("token_count")
        {
            continue;
        }

        let Some(last_usage) = entry
            .get("payload")
            .and_then(|payload| payload.get("info"))
            .and_then(|info| info.get("last_token_usage"))
        else {
            continue;
        };

        usage.input_tokens += json_u64(last_usage.get("input_tokens"));
        usage.cached_input_tokens += json_u64(last_usage.get("cached_input_tokens"));
        usage.output_tokens += json_u64(last_usage.get("output_tokens"));
        usage.reasoning_output_tokens += json_u64(last_usage.get("reasoning_output_tokens"));
    }

    if model.is_none() && usage.total_tokens() == 0 {
        return Ok(None);
    }

    Ok(Some(SessionUsage { model, usage }))
}

fn parse_claude_session_usage(path: &Path) -> Result<Option<SessionUsage>> {
    let file = File::open(path)
        .with_context(|| format!("failed to open claude session {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut usage = TokenUsage::default();
    let mut model = None;

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(&line) else {
            continue;
        };

        let Some(message) = entry.get("message") else {
            continue;
        };
        if let Some(value) = message.get("model").and_then(Value::as_str) {
            model = Some(value.to_string());
        }
        let Some(usage_value) = message.get("usage") else {
            continue;
        };

        usage.input_tokens += json_u64(usage_value.get("input_tokens"));
        usage.output_tokens += json_u64(usage_value.get("output_tokens"));
        usage.cache_creation_input_tokens +=
            json_u64(usage_value.get("cache_creation_input_tokens"));
        usage.cache_read_input_tokens += json_u64(usage_value.get("cache_read_input_tokens"));

        let cache_creation = usage_value.get("cache_creation");
        usage.cache_creation_5m_input_tokens +=
            json_u64(cache_creation.and_then(|value| value.get("ephemeral_5m_input_tokens")));
        usage.cache_creation_1h_input_tokens +=
            json_u64(cache_creation.and_then(|value| value.get("ephemeral_1h_input_tokens")));
    }

    if model.is_none() && usage.total_tokens() == 0 {
        return Ok(None);
    }

    Ok(Some(SessionUsage { model, usage }))
}

fn json_u64(value: Option<&Value>) -> u64 {
    value.and_then(Value::as_u64).unwrap_or(0)
}

fn read_dir_paths(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        paths.push(entry.path());
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::config::{CostConfig, RoleType, TeamConfig, WorkflowMode};

    fn test_team_config(models: HashMap<String, ModelPricing>) -> TeamConfig {
        TeamConfig {
            name: "batty".to_string(),
            workflow_mode: WorkflowMode::Legacy,
            board: Default::default(),
            standup: Default::default(),
            automation: Default::default(),
            automation_sender: None,
            orchestrator_pane: true,
            orchestrator_position: Default::default(),
            layout: None,
            workflow_policy: Default::default(),
            cost: CostConfig { models },
            roles: vec![
                crate::team::config::RoleDef {
                    name: "architect".to_string(),
                    role_type: RoleType::Architect,
                    agent: Some("claude".to_string()),
                    instances: 1,
                    prompt: None,
                    talks_to: Vec::new(),
                    channel: None,
                    channel_config: None,
                    nudge_interval_secs: None,
                    receives_standup: None,
                    standup_interval_secs: None,
                    owns: Vec::new(),
                    use_worktrees: false,
                },
                crate::team::config::RoleDef {
                    name: "engineer".to_string(),
                    role_type: RoleType::Engineer,
                    agent: Some("codex".to_string()),
                    instances: 1,
                    prompt: None,
                    talks_to: Vec::new(),
                    channel: None,
                    channel_config: None,
                    nudge_interval_secs: None,
                    receives_standup: None,
                    standup_interval_secs: None,
                    owns: Vec::new(),
                    use_worktrees: true,
                },
            ],
        }
    }

    #[test]
    fn parse_codex_session_usage_sums_last_token_usage() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("codex.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"abc\",\"cwd\":\"/tmp/repo\"}}\n",
                "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5.4\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":100,\"cached_input_tokens\":25,\"output_tokens\":10,\"reasoning_output_tokens\":5}}}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"last_token_usage\":{\"input_tokens\":50,\"cached_input_tokens\":5,\"output_tokens\":4,\"reasoning_output_tokens\":1}}}}\n"
            ),
        )
        .unwrap();

        let usage = parse_codex_session_usage(&path).unwrap().unwrap();
        assert_eq!(usage.model.as_deref(), Some("gpt-5.4"));
        assert_eq!(usage.usage.input_tokens, 150);
        assert_eq!(usage.usage.cached_input_tokens, 30);
        assert_eq!(usage.usage.output_tokens, 14);
        assert_eq!(usage.usage.reasoning_output_tokens, 6);
    }

    #[test]
    fn parse_claude_session_usage_sums_message_usage() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("claude.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"message\":{\"model\":\"claude-opus-4-6\",\"usage\":{\"input_tokens\":10,\"output_tokens\":2,\"cache_creation_input_tokens\":20,\"cache_read_input_tokens\":3,\"cache_creation\":{\"ephemeral_5m_input_tokens\":5,\"ephemeral_1h_input_tokens\":15}}}}\n",
                "{\"message\":{\"model\":\"claude-opus-4-6\",\"usage\":{\"input_tokens\":7,\"output_tokens\":4,\"cache_creation_input_tokens\":8,\"cache_read_input_tokens\":1}}}\n"
            ),
        )
        .unwrap();

        let usage = parse_claude_session_usage(&path).unwrap().unwrap();
        assert_eq!(usage.model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(usage.usage.input_tokens, 17);
        assert_eq!(usage.usage.output_tokens, 6);
        assert_eq!(usage.usage.cache_creation_input_tokens, 28);
        assert_eq!(usage.usage.cache_read_input_tokens, 4);
        assert_eq!(usage.usage.cache_creation_5m_input_tokens, 5);
        assert_eq!(usage.usage.cache_creation_1h_input_tokens, 15);
    }

    #[test]
    fn estimate_cost_uses_pricing_breakdown() {
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            cached_input_tokens: 2_000_000,
            cache_creation_input_tokens: 1_000_000,
            cache_creation_5m_input_tokens: 400_000,
            cache_creation_1h_input_tokens: 500_000,
            cache_read_input_tokens: 300_000,
            output_tokens: 100_000,
            reasoning_output_tokens: 50_000,
        };
        let pricing = ModelPricing {
            input_usd_per_mtok: 2.0,
            cached_input_usd_per_mtok: 0.5,
            cache_creation_input_usd_per_mtok: Some(3.0),
            cache_creation_5m_input_usd_per_mtok: Some(4.0),
            cache_creation_1h_input_usd_per_mtok: Some(5.0),
            cache_read_input_usd_per_mtok: 0.25,
            output_usd_per_mtok: 8.0,
            reasoning_output_usd_per_mtok: Some(10.0),
        };

        let estimated = estimate_cost_usd(&usage, &pricing);
        let expected = 2.0 + 1.0 + 1.6 + 2.5 + 0.3 + 0.075 + 0.8 + 0.5;
        assert!((estimated - expected).abs() < 1e-9);
    }

    #[test]
    fn built_in_pricing_supports_common_models() {
        assert!(pricing_for_model(&HashMap::new(), "gpt-5.4").is_some());
        assert!(pricing_for_model(&HashMap::new(), "claude-opus-4-6").is_some());
        assert!(pricing_for_model(&HashMap::new(), "claude-sonnet-4").is_some());
    }

    #[test]
    fn collect_cost_report_maps_members_to_current_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path();
        fs::create_dir_all(
            project_root
                .join(".batty")
                .join("worktrees")
                .join("engineer"),
        )
        .unwrap();
        fs::create_dir_all(
            project_root
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks"),
        )
        .unwrap();

        fs::write(
            project_root.join(".batty").join("launch-state.json"),
            r#"{
  "architect": {"session_id": "claude-session"},
  "engineer": {"session_id": "codex-session"}
}"#,
        )
        .unwrap();
        fs::write(
            daemon_state_path(project_root),
            r#"{"active_tasks":{"engineer":100}}"#,
        )
        .unwrap();
        fs::write(
            project_root
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks")
                .join("100-task.md"),
            concat!(
                "---\n",
                "id: 100\n",
                "title: Cost task\n",
                "status: in-progress\n",
                "claimed_by: engineer\n",
                "---\n"
            ),
        )
        .unwrap();

        let codex_root = project_root.join("codex-sessions");
        let codex_day = codex_root.join("2026").join("03").join("21");
        fs::create_dir_all(&codex_day).unwrap();
        fs::write(
            codex_day.join("rollout.jsonl"),
            format!(
                "{}\n{}\n{}\n",
                format!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"codex-session\",\"cwd\":\"{}\"}}}}",
                    project_root
                        .join(".batty")
                        .join("worktrees")
                        .join("engineer")
                        .join(".batty")
                        .join("codex-context")
                        .join("engineer")
                        .display()
                ),
                r#"{"type":"turn_context","payload":{"model":"gpt-5.4"}}"#,
                r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":1000,"cached_input_tokens":250,"output_tokens":100,"reasoning_output_tokens":10}}}}"#,
            ),
        )
        .unwrap();

        let claude_root = project_root.join("claude-projects");
        let claude_dir = claude_root.join(project_root.to_string_lossy().replace('/', "-"));
        fs::create_dir_all(&claude_dir).unwrap();
        fs::write(
            claude_dir.join("claude-session.jsonl"),
            "{\"message\":{\"model\":\"claude-opus-4-6\",\"usage\":{\"input_tokens\":10,\"output_tokens\":2,\"cache_creation_input_tokens\":20,\"cache_read_input_tokens\":3,\"cache_creation\":{\"ephemeral_5m_input_tokens\":5,\"ephemeral_1h_input_tokens\":15}}}}\n",
        )
        .unwrap();

        let report = collect_cost_report(
            project_root,
            &test_team_config(HashMap::new()),
            &SessionRoots {
                codex_sessions_root: codex_root,
                claude_projects_root: claude_root,
            },
        )
        .unwrap();

        assert_eq!(report.entries.len(), 2);
        let engineer = report
            .entries
            .iter()
            .find(|entry| entry.member_name == "engineer")
            .unwrap();
        assert_eq!(engineer.task, "#100");
        assert_eq!(engineer.model, "gpt-5.4");
        assert!(engineer.estimated_cost_usd.unwrap() > 0.0);
    }

    #[test]
    fn collect_cost_report_skips_user_roles_and_shared_cwd_fallbacks() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path();
        let mut config = test_team_config(HashMap::new());
        config.roles.insert(
            0,
            crate::team::config::RoleDef {
                name: "human".to_string(),
                role_type: RoleType::User,
                agent: None,
                instances: 1,
                prompt: None,
                talks_to: Vec::new(),
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                use_worktrees: false,
            },
        );
        config.roles.push(crate::team::config::RoleDef {
            name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            instances: 1,
            prompt: None,
            talks_to: Vec::new(),
            channel: None,
            channel_config: None,
            nudge_interval_secs: None,
            receives_standup: None,
            standup_interval_secs: None,
            owns: Vec::new(),
            use_worktrees: false,
        });

        let claude_root = project_root.join("claude-projects");
        let claude_dir = claude_root.join(project_root.to_string_lossy().replace('/', "-"));
        fs::create_dir_all(&claude_dir).unwrap();
        fs::write(
            claude_dir.join("shared.jsonl"),
            "{\"message\":{\"model\":\"claude-opus-4-6\",\"usage\":{\"input_tokens\":10,\"output_tokens\":2}}}\n",
        )
        .unwrap();

        let report = collect_cost_report(
            project_root,
            &config,
            &SessionRoots {
                codex_sessions_root: project_root.join("codex-sessions"),
                claude_projects_root: claude_root,
            },
        )
        .unwrap();

        assert!(report.entries.is_empty());
    }
}
