use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::task::Task;
use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use super::config::{AllocationPolicy, RoleType, TeamConfig};
use super::hierarchy::resolve_hierarchy;
use super::standup::MemberState;
use super::{daemon_state_path, team_config_dir};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct EngineerProfile {
    pub name: String,
    pub completed_task_ids: Vec<u32>,
    pub active_file_paths: HashSet<String>,
    pub domain_tags: HashSet<String>,
    pub active_task_count: u32,
    pub total_completions: u32,
    pub recent_merge_conflicts: u32,
    pub performance: Option<EngineerPerformanceProfile>,
    pub telemetry_completed_tasks: u32,
    pub completion_rate: f64,
    pub avg_task_duration_secs: Option<f64>,
    pub first_pass_test_rate: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct EngineerPerformanceProfile {
    pub avg_task_completion_secs: Option<f64>,
    pub lines_per_hour: Option<f64>,
    pub first_pass_test_rate: Option<f64>,
    pub context_exhaustion_frequency: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct EngineerRoutingBreakdown {
    pub engineer: String,
    pub total_score: i32,
    pub tag_matches: usize,
    pub file_matches: usize,
    pub completion_rate: f64,
    pub avg_task_duration_secs: Option<f64>,
    pub first_pass_test_rate: Option<f64>,
    pub telemetry_completed_tasks: u32,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RoutingDecisionExplanation {
    pub chosen_engineer: Option<String>,
    pub fallback_to_round_robin: bool,
    pub fallback_reason: Option<String>,
    pub breakdowns: Vec<EngineerRoutingBreakdown>,
}

#[derive(Debug, Default, Deserialize)]
struct AllocationTaskFrontmatter {
    #[serde(default)]
    changed_paths: Vec<String>,
}

const PROFILE_RETENTION_DAYS: i64 = 7;
const ENGINEER_PROFILES_FILE: &str = "engineer_profiles.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedEngineerProfiles {
    #[serde(default = "engineer_profiles_format_version")]
    version: u32,
    #[serde(default)]
    updated_at: Option<String>,
    #[serde(default)]
    completions: Vec<PersistedTaskProfile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedTaskProfile {
    engineer: String,
    task_id: u32,
    completed_at: String,
    #[serde(default)]
    changed_paths: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    recent_merge_conflict: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct EngineerTelemetryStats {
    completed_tasks: u32,
    completion_rate: f64,
    avg_task_duration_secs: Option<f64>,
    first_pass_test_rate: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct PersistedDaemonStateView {
    #[serde(default)]
    states: HashMap<String, MemberState>,
}

fn engineer_profiles_format_version() -> u32 {
    1
}

pub fn build_engineer_profiles(
    engineers: &[String],
    tasks: &[Task],
) -> Result<HashMap<String, EngineerProfile>> {
    build_engineer_profiles_with_history(engineers, tasks, &[], &HashMap::new())
}

pub fn load_engineer_profiles(
    project_root: &Path,
    engineers: &[String],
    tasks: &[Task],
) -> Result<HashMap<String, EngineerProfile>> {
    let persisted = load_persisted_task_profiles(project_root)?;
    let telemetry = load_engineer_telemetry_stats(project_root)?;
    let mut profiles =
        build_engineer_profiles_with_history(engineers, tasks, &persisted, &telemetry)?;
    if let Ok(conn) = crate::team::telemetry_db::open(project_root)
        && let Ok(rows) = crate::team::telemetry_db::query_engineer_performance_profiles(&conn)
    {
        for row in rows {
            if let Some(profile) = profiles.get_mut(&row.role) {
                profile.performance = Some(EngineerPerformanceProfile {
                    avg_task_completion_secs: row.avg_task_completion_secs,
                    lines_per_hour: row.lines_per_hour,
                    first_pass_test_rate: row.first_pass_test_rate,
                    context_exhaustion_frequency: row.context_exhaustion_frequency,
                });
            }
        }
    }
    Ok(profiles)
}

pub(crate) fn predict_task_file_paths(project_root: &Path, task: &Task) -> Result<HashSet<String>> {
    let mut paths = task_profile_paths(task)?;
    for record in load_persisted_task_profiles(project_root)? {
        if persisted_profile_matches_task(task, &record) {
            paths.extend(record.changed_paths);
        }
    }
    Ok(paths)
}

pub fn persist_completed_task_profile(project_root: &Path, task: &Task) -> Result<()> {
    let Some(engineer) = task.claimed_by.as_deref() else {
        return Ok(());
    };

    let mut persisted = read_persisted_engineer_profiles(project_root)?;
    prune_persisted_profiles(&mut persisted);
    persisted
        .completions
        .retain(|entry| !(entry.engineer == engineer && entry.task_id == task.id));
    persisted.completions.push(PersistedTaskProfile {
        engineer: engineer.to_string(),
        task_id: task.id,
        completed_at: task
            .completed
            .clone()
            .unwrap_or_else(|| Utc::now().to_rfc3339()),
        changed_paths: load_changed_paths(task.source_path.as_path())?,
        tags: task.tags.clone(),
        recent_merge_conflict: task_has_conflict_signal(task),
    });
    persisted.updated_at = Some(Utc::now().to_rfc3339());
    write_persisted_engineer_profiles(project_root, &persisted)
}

fn build_engineer_profiles_with_history(
    engineers: &[String],
    tasks: &[Task],
    persisted: &[PersistedTaskProfile],
    telemetry: &HashMap<String, EngineerTelemetryStats>,
) -> Result<HashMap<String, EngineerProfile>> {
    let mut profiles: HashMap<String, EngineerProfile> = engineers
        .iter()
        .cloned()
        .map(|name| {
            let profile = EngineerProfile {
                name: name.clone(),
                ..EngineerProfile::default()
            };
            (name, profile)
        })
        .collect();

    let mut completed_task_keys: HashSet<(String, u32)> = HashSet::new();

    for record in persisted {
        let Some(profile) = profiles.get_mut(&record.engineer) else {
            continue;
        };
        apply_completed_profile(
            profile,
            record.task_id,
            record.tags.iter().cloned(),
            record.changed_paths.iter().cloned(),
            record.recent_merge_conflict,
        );
        completed_task_keys.insert((record.engineer.clone(), record.task_id));
    }

    for task in tasks {
        let Some(owner) = task.claimed_by.as_deref() else {
            continue;
        };
        let Some(profile) = profiles.get_mut(owner) else {
            continue;
        };

        if task.status == "done" {
            if completed_task_keys.insert((owner.to_string(), task.id)) {
                apply_completed_profile(
                    profile,
                    task.id,
                    task.tags.iter().cloned(),
                    task_profile_paths(task)?.into_iter(),
                    task_has_conflict_signal(task),
                );
            }
        } else if task_is_active_for_load(task) {
            profile.active_task_count += 1;
            profile.active_file_paths.extend(task_profile_paths(task)?);
            if task_has_conflict_signal(task) {
                profile.recent_merge_conflicts += 1;
            }
        }
    }

    for (engineer, stats) in telemetry {
        let Some(profile) = profiles.get_mut(engineer) else {
            continue;
        };
        profile.telemetry_completed_tasks = stats.completed_tasks;
        profile.completion_rate = stats.completion_rate;
        profile.avg_task_duration_secs = stats.avg_task_duration_secs;
        profile.first_pass_test_rate = stats.first_pass_test_rate;
    }

    Ok(profiles)
}

fn apply_completed_profile<I, J>(
    profile: &mut EngineerProfile,
    task_id: u32,
    tags: I,
    changed_paths: J,
    recent_merge_conflict: bool,
) where
    I: IntoIterator<Item = String>,
    J: IntoIterator<Item = String>,
{
    profile.completed_task_ids.push(task_id);
    profile.total_completions += 1;
    profile.domain_tags.extend(tags);
    profile.active_file_paths.extend(changed_paths);
    if recent_merge_conflict {
        profile.recent_merge_conflicts += 1;
    }
}

fn persisted_profile_matches_task(task: &Task, record: &PersistedTaskProfile) -> bool {
    if task
        .tags
        .iter()
        .any(|tag| record.tags.iter().any(|candidate| candidate == tag))
    {
        return true;
    }

    let hinted_dirs: HashSet<String> = task_hint_paths(task)
        .into_iter()
        .filter_map(|path| parent_dir(&path))
        .collect();
    let record_dirs: HashSet<String> = record
        .changed_paths
        .iter()
        .filter_map(|path| parent_dir(path))
        .collect();
    !hinted_dirs.is_empty() && !hinted_dirs.is_disjoint(&record_dirs)
}

pub fn score_engineer_for_task(
    engineer: &EngineerProfile,
    task: &Task,
    policy: &AllocationPolicy,
) -> i32 {
    let mut score = 0;

    let tag_overlap = task
        .tags
        .iter()
        .filter(|tag| engineer.domain_tags.contains(*tag))
        .count() as i32;
    score += tag_overlap * policy.tag_weight;

    let task_dirs = task_hint_directories(task);
    let engineer_dirs: HashSet<String> = engineer
        .active_file_paths
        .iter()
        .filter_map(|path| parent_dir(path))
        .collect();
    let dir_overlap = engineer_dirs.intersection(&task_dirs).count() as i32;
    score += dir_overlap * policy.file_overlap_weight;
    score += (engineer.completion_rate * 100.0).round() as i32;

    score -= (engineer.active_task_count as i32) * policy.load_penalty;
    score -= (engineer.recent_merge_conflicts as i32) * policy.conflict_penalty;
    if engineer.total_completions > 3 {
        score += policy.experience_bonus;
    }
    score += performance_score(engineer.performance.as_ref());

    score
}

fn performance_score(performance: Option<&EngineerPerformanceProfile>) -> i32 {
    let Some(performance) = performance else {
        return 0;
    };

    let mut score = 0;

    if let Some(first_pass_rate) = performance.first_pass_test_rate {
        if first_pass_rate >= 0.75 {
            score += 1;
        } else if first_pass_rate < 0.5 {
            score -= 1;
        }
    }

    if let Some(context_freq) = performance.context_exhaustion_frequency {
        if context_freq >= 0.5 {
            score -= 2;
        } else if context_freq > 0.0 {
            score -= 1;
        }
    }

    if let Some(avg_task_completion_secs) = performance.avg_task_completion_secs {
        if avg_task_completion_secs > 0.0 && avg_task_completion_secs <= 3_600.0 {
            score += 1;
        } else if avg_task_completion_secs >= 14_400.0 {
            score -= 1;
        }
    }

    if let Some(lines_per_hour) = performance.lines_per_hour
        && lines_per_hour >= 200.0
    {
        score += 1;
    }

    score
}

pub fn rank_engineers_for_task(
    engineers: &[String],
    profiles: &HashMap<String, EngineerProfile>,
    task: &Task,
    policy: &AllocationPolicy,
) -> Vec<String> {
    explain_routing_for_task(engineers, profiles, task, policy)
        .breakdowns
        .into_iter()
        .map(|breakdown| breakdown.engineer)
        .collect()
}

pub fn explain_routing_for_task(
    engineers: &[String],
    profiles: &HashMap<String, EngineerProfile>,
    task: &Task,
    policy: &AllocationPolicy,
) -> RoutingDecisionExplanation {
    let mut breakdowns: Vec<EngineerRoutingBreakdown> = engineers
        .iter()
        .map(|engineer| engineer_breakdown(engineer, profiles.get(engineer), task, policy))
        .collect();

    let has_any_telemetry = breakdowns
        .iter()
        .any(|breakdown| breakdown.telemetry_completed_tasks > 0);
    let telemetry_ready = has_any_telemetry
        && breakdowns
            .iter()
            .all(|breakdown| breakdown.telemetry_completed_tasks >= 5);
    if telemetry_ready || !has_any_telemetry {
        breakdowns.sort_by(compare_breakdowns);
        let chosen_engineer = breakdowns
            .first()
            .map(|breakdown| breakdown.engineer.clone());
        return RoutingDecisionExplanation {
            chosen_engineer,
            fallback_to_round_robin: false,
            fallback_reason: None,
            breakdowns,
        };
    }

    breakdowns.sort_by(|left, right| left.engineer.cmp(&right.engineer));
    let chosen_engineer = breakdowns
        .first()
        .map(|breakdown| breakdown.engineer.clone());
    RoutingDecisionExplanation {
        chosen_engineer,
        fallback_to_round_robin: true,
        fallback_reason: Some(
            "telemetry fallback: each eligible engineer needs at least 5 completed tasks"
                .to_string(),
        ),
        breakdowns,
    }
}

pub fn print_dispatch_explanation(project_root: &Path, task_id: Option<u32>) -> Result<()> {
    let board_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board");
    let tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks"))?;
    let task = select_dispatch_task(&tasks, task_id)
        .with_context(|| format!("no dispatchable task found for {:?}", task_id))?;

    let team_config = TeamConfig::load(&team_config_dir(project_root).join("team.yaml"))?;
    let members = resolve_hierarchy(&team_config)?;
    let mut engineers = load_idle_engineers(project_root, &members)?;
    if engineers.is_empty() {
        engineers = members
            .into_iter()
            .filter(|member| member.role_type == RoleType::Engineer)
            .map(|member| member.name)
            .collect();
    }
    let bench_state = crate::team::bench::load_bench_state(project_root)?;
    engineers.retain(|engineer| !bench_state.benched.contains_key(engineer));
    engineers.sort();
    let profiles = load_engineer_profiles(project_root, &engineers, &tasks)?;
    let explanation = explain_routing_for_task(
        &engineers,
        &profiles,
        task,
        &team_config.workflow_policy.allocation,
    );

    println!("Task #{}: {}", task.id, task.title);
    if let Some(chosen) = &explanation.chosen_engineer {
        println!("Chosen engineer: {chosen}");
    } else {
        println!("Chosen engineer: none");
    }
    if let Some(reason) = &explanation.fallback_reason {
        println!("Routing mode: {reason}");
    } else {
        println!("Routing mode: telemetry-scored");
    }
    println!();
    println!(
        "{:<20} {:>6} {:>5} {:>5} {:>10} {:>10} {:>11} {:>8}",
        "ENGINEER", "SCORE", "TAGS", "FILES", "COMPLETE%", "AVG SECS", "FIRST PASS%", "SAMPLES"
    );
    println!("{}", "-".repeat(88));
    for breakdown in explanation.breakdowns {
        println!(
            "{:<20} {:>6} {:>5} {:>5} {:>10.1} {:>10} {:>11.1} {:>8}",
            breakdown.engineer,
            breakdown.total_score,
            breakdown.tag_matches,
            breakdown.file_matches,
            breakdown.completion_rate * 100.0,
            breakdown
                .avg_task_duration_secs
                .map(|secs| format!("{secs:.0}"))
                .unwrap_or_else(|| "-".to_string()),
            breakdown.first_pass_test_rate.unwrap_or(0.0) * 100.0,
            breakdown.telemetry_completed_tasks,
        );
    }

    Ok(())
}

fn engineer_breakdown(
    engineer: &str,
    profile: Option<&EngineerProfile>,
    task: &Task,
    policy: &AllocationPolicy,
) -> EngineerRoutingBreakdown {
    let profile = profile.cloned().unwrap_or_else(|| EngineerProfile {
        name: engineer.to_string(),
        ..EngineerProfile::default()
    });
    let task_dirs = task_hint_directories(task);
    let engineer_dirs: HashSet<String> = profile
        .active_file_paths
        .iter()
        .filter_map(|path| parent_dir(path))
        .collect();
    let tag_matches = task
        .tags
        .iter()
        .filter(|tag| profile.domain_tags.contains(*tag))
        .count();
    let file_matches = engineer_dirs.intersection(&task_dirs).count();
    EngineerRoutingBreakdown {
        engineer: engineer.to_string(),
        total_score: score_engineer_for_task(&profile, task, policy),
        tag_matches,
        file_matches,
        completion_rate: profile.completion_rate,
        avg_task_duration_secs: profile.avg_task_duration_secs,
        first_pass_test_rate: profile.first_pass_test_rate,
        telemetry_completed_tasks: profile.telemetry_completed_tasks,
    }
}

fn compare_breakdowns(
    left: &EngineerRoutingBreakdown,
    right: &EngineerRoutingBreakdown,
) -> std::cmp::Ordering {
    right
        .total_score
        .cmp(&left.total_score)
        .then_with(|| {
            right
                .first_pass_test_rate
                .partial_cmp(&left.first_pass_test_rate)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .then_with(|| {
            left.avg_task_duration_secs
                .partial_cmp(&right.avg_task_duration_secs)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .then_with(|| left.engineer.cmp(&right.engineer))
}

fn select_dispatch_task(tasks: &[Task], task_id: Option<u32>) -> Option<&Task> {
    if let Some(task_id) = task_id {
        return tasks.iter().find(|task| task.id == task_id);
    }

    let task_status_by_id: HashMap<u32, String> = tasks
        .iter()
        .map(|task| (task.id, task.status.clone()))
        .collect();
    let mut dispatchable: Vec<&Task> = tasks
        .iter()
        .filter(|task| matches!(task.status.as_str(), "backlog" | "todo"))
        .filter(|task| task.claimed_by.is_none())
        .filter(|task| task.blocked.is_none())
        .filter(|task| task.blocked_on.is_none())
        .filter(|task| !task.is_schedule_blocked())
        .filter(|task| {
            task.depends_on.iter().all(|dep_id| {
                task_status_by_id
                    .get(dep_id)
                    .is_none_or(|status| status == "done")
            })
        })
        .collect();
    dispatchable.sort_by_key(|task| {
        (
            match task.priority.as_str() {
                "critical" => 0,
                "high" => 1,
                "medium" => 2,
                "low" => 3,
                _ => 4,
            },
            task.id,
        )
    });
    dispatchable.into_iter().next()
}

fn load_idle_engineers(
    project_root: &Path,
    members: &[super::hierarchy::MemberInstance],
) -> Result<Vec<String>> {
    let path = daemon_state_path(project_root);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let state: PersistedDaemonStateView = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(members
        .iter()
        .filter(|member| member.role_type == RoleType::Engineer)
        .filter(|member| state.states.get(&member.name) == Some(&MemberState::Idle))
        .map(|member| member.name.clone())
        .collect())
}

fn load_engineer_telemetry_stats(
    project_root: &Path,
) -> Result<HashMap<String, EngineerTelemetryStats>> {
    let conn = match super::telemetry_db::open(project_root) {
        Ok(conn) => conn,
        Err(_) => return Ok(HashMap::new()),
    };

    let mut stats = load_quality_metric_stats(&conn)?;
    for (engineer, completion_rate) in load_completion_rates(&conn)? {
        stats.entry(engineer).or_default().completion_rate = completion_rate;
    }
    Ok(stats)
}

fn load_completion_rates(conn: &Connection) -> Result<HashMap<String, f64>> {
    let mut stmt = conn.prepare(
        "SELECT role,
                SUM(CASE WHEN event_type = 'task_assigned' THEN 1 ELSE 0 END) AS assigned,
                SUM(CASE WHEN event_type = 'task_completed' THEN 1 ELSE 0 END) AS completed
         FROM events
         WHERE role IS NOT NULL AND event_type IN ('task_assigned', 'task_completed')
         GROUP BY role",
    )?;
    let rows = stmt.query_map([], |row| {
        let role: String = row.get(0)?;
        let assigned: i64 = row.get(1)?;
        let completed: i64 = row.get(2)?;
        let rate = if assigned > 0 {
            completed as f64 / assigned as f64
        } else {
            0.0
        };
        Ok((role, rate))
    })?;

    let mut rates = HashMap::new();
    for row in rows {
        let (role, rate) = row?;
        rates.insert(role, rate);
    }
    Ok(rates)
}

fn load_quality_metric_stats(conn: &Connection) -> Result<HashMap<String, EngineerTelemetryStats>> {
    let mut stmt = conn.prepare(
        "SELECT role,
                COUNT(*) AS samples,
                AVG(CAST(json_extract(payload, '$.time_to_completion_secs') AS REAL)) AS avg_secs,
                AVG(CAST(json_extract(payload, '$.first_pass_test_rate') AS REAL)) AS first_pass
         FROM events
         WHERE role IS NOT NULL AND event_type = 'quality_metrics_recorded'
         GROUP BY role",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            EngineerTelemetryStats {
                completed_tasks: row.get::<_, i64>(1)? as u32,
                completion_rate: 0.0,
                avg_task_duration_secs: row.get(2)?,
                first_pass_test_rate: row.get(3)?,
            },
        ))
    })?;

    let mut stats = HashMap::new();
    for row in rows {
        let (engineer, profile) = row?;
        stats.insert(engineer, profile);
    }
    Ok(stats)
}

fn task_is_active_for_load(task: &Task) -> bool {
    matches!(
        task.status.as_str(),
        "todo" | "backlog" | "in-progress" | "review" | "blocked"
    )
}

fn task_has_conflict_signal(task: &Task) -> bool {
    let blocked = task.blocked.as_deref().unwrap_or_default();
    let blocked_on = task.blocked_on.as_deref().unwrap_or_default();
    let description = task.description.to_ascii_lowercase();
    blocked.to_ascii_lowercase().contains("conflict")
        || blocked_on.to_ascii_lowercase().contains("conflict")
        || description.contains("merge conflict")
        || description.contains("rebase conflict")
}

fn task_profile_paths(task: &Task) -> Result<HashSet<String>> {
    let mut paths = task_hint_paths(task);
    for path in load_changed_paths(task.source_path.as_path())? {
        paths.insert(path);
    }
    Ok(paths)
}

fn task_hint_directories(task: &Task) -> HashSet<String> {
    task_hint_paths(task)
        .into_iter()
        .filter_map(|path| parent_dir(&path))
        .collect()
}

fn task_hint_paths(task: &Task) -> HashSet<String> {
    task.description
        .split_whitespace()
        .filter_map(clean_task_path_token)
        .collect()
}

fn clean_task_path_token(token: &str) -> Option<String> {
    let cleaned = token.trim_matches(|ch: char| {
        matches!(
            ch,
            '"' | '\'' | ',' | ':' | ';' | '(' | ')' | '[' | ']' | '`'
        )
    });
    parent_dir(cleaned).map(|_| cleaned.to_string())
}

fn load_changed_paths(path: &Path) -> Result<Vec<String>> {
    if path.as_os_str().is_empty() || !path.exists() {
        return Ok(Vec::new());
    }

    let content = std::fs::read_to_string(path)?;
    let Some(frontmatter) = extract_frontmatter(&content) else {
        return Ok(Vec::new());
    };
    let parsed: AllocationTaskFrontmatter = serde_yaml::from_str(frontmatter).unwrap_or_default();
    Ok(parsed.changed_paths)
}

fn load_persisted_task_profiles(project_root: &Path) -> Result<Vec<PersistedTaskProfile>> {
    let mut persisted = read_persisted_engineer_profiles(project_root)?;
    prune_persisted_profiles(&mut persisted);
    if persisted.updated_at.is_some() {
        write_persisted_engineer_profiles(project_root, &persisted)?;
    }
    Ok(persisted.completions)
}

fn read_persisted_engineer_profiles(project_root: &Path) -> Result<PersistedEngineerProfiles> {
    let path = engineer_profiles_path(project_root);
    if !path.exists() {
        return Ok(PersistedEngineerProfiles {
            version: engineer_profiles_format_version(),
            updated_at: None,
            completions: Vec::new(),
        });
    }

    let content = std::fs::read_to_string(&path)?;
    let persisted = serde_json::from_str(&content)?;
    Ok(persisted)
}

fn write_persisted_engineer_profiles(
    project_root: &Path,
    persisted: &PersistedEngineerProfiles,
) -> Result<()> {
    let path = engineer_profiles_path(project_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(persisted)?)?;
    Ok(())
}

fn prune_persisted_profiles(persisted: &mut PersistedEngineerProfiles) {
    let cutoff = Utc::now() - Duration::days(PROFILE_RETENTION_DAYS);
    persisted.completions.retain(|entry| {
        DateTime::parse_from_rfc3339(&entry.completed_at)
            .map(|completed| completed.with_timezone(&Utc) >= cutoff)
            .unwrap_or(false)
    });
}

fn engineer_profiles_path(project_root: &Path) -> PathBuf {
    team_config_dir(project_root).join(ENGINEER_PROFILES_FILE)
}

fn extract_frontmatter(content: &str) -> Option<&str> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }
    let after_open = trimmed[3..].strip_prefix('\n').unwrap_or(&trimmed[3..]);
    let close_pos = after_open.find("\n---")?;
    Some(&after_open[..close_pos])
}

fn parent_dir(path: &str) -> Option<String> {
    PathBuf::from(path)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(|parent| parent.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::config::AllocationStrategy;
    use crate::team::events::{QualityMetricsInfo, TeamEvent};
    use crate::team::telemetry_db;
    use std::fs;

    fn task(tags: &[&str], description: &str) -> Task {
        Task {
            id: 1,
            title: "task".to_string(),
            status: "todo".to_string(),
            priority: "high".to_string(),
            claimed_by: None,
            claimed_at: None,
            claim_ttl_secs: None,
            claim_expires_at: None,
            last_progress_at: None,
            claim_warning_sent_at: None,
            claim_extensions: None,
            last_output_bytes: None,
            blocked: None,
            tags: tags.iter().map(|tag| (*tag).to_string()).collect(),
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
            description: description.to_string(),
            batty_config: None,
            source_path: PathBuf::new(),
        }
    }

    fn policy() -> AllocationPolicy {
        AllocationPolicy {
            strategy: AllocationStrategy::Scored,
            ..AllocationPolicy::default()
        }
    }

    #[test]
    fn score_prefers_tag_overlap() {
        let profile = EngineerProfile {
            domain_tags: HashSet::from(["dispatch".to_string()]),
            ..EngineerProfile::default()
        };
        assert!(score_engineer_for_task(&profile, &task(&["dispatch"], ""), &policy()) > 0);
    }

    #[test]
    fn score_prefers_matching_directory_hints() {
        let profile = EngineerProfile {
            active_file_paths: HashSet::from(["src/team/dispatch/queue.rs".to_string()]),
            ..EngineerProfile::default()
        };
        assert!(
            score_engineer_for_task(
                &profile,
                &task(&[], "Touch src/team/dispatch/mod.rs next."),
                &policy(),
            ) > 0
        );
    }

    #[test]
    fn score_penalizes_active_load() {
        let light = EngineerProfile {
            active_task_count: 0,
            ..EngineerProfile::default()
        };
        let busy = EngineerProfile {
            active_task_count: 2,
            ..EngineerProfile::default()
        };
        assert!(
            score_engineer_for_task(&light, &task(&[], ""), &policy())
                > score_engineer_for_task(&busy, &task(&[], ""), &policy())
        );
    }

    #[test]
    fn rank_engineers_falls_back_to_name_order_on_tie() {
        let engineers = vec!["eng-2".to_string(), "eng-1".to_string()];
        let profiles = HashMap::from([
            ("eng-1".to_string(), EngineerProfile::default()),
            ("eng-2".to_string(), EngineerProfile::default()),
        ]);
        let ranked = rank_engineers_for_task(&engineers, &profiles, &task(&[], ""), &policy());
        assert_eq!(ranked, vec!["eng-1".to_string(), "eng-2".to_string()]);
    }

    #[test]
    fn score_prefers_more_reliable_performance_profile() {
        let reliable = EngineerProfile {
            performance: Some(EngineerPerformanceProfile {
                avg_task_completion_secs: Some(1800.0),
                lines_per_hour: Some(250.0),
                first_pass_test_rate: Some(1.0),
                context_exhaustion_frequency: Some(0.0),
            }),
            ..EngineerProfile::default()
        };
        let unstable = EngineerProfile {
            performance: Some(EngineerPerformanceProfile {
                avg_task_completion_secs: Some(18_000.0),
                lines_per_hour: Some(10.0),
                first_pass_test_rate: Some(0.0),
                context_exhaustion_frequency: Some(1.0),
            }),
            ..EngineerProfile::default()
        };

        assert!(
            score_engineer_for_task(&reliable, &task(&[], ""), &policy())
                > score_engineer_for_task(&unstable, &task(&[], ""), &policy())
        );
    }

    #[test]
    fn build_profiles_reads_changed_paths_and_tags_from_board_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let task_path = tmp.path().join("042-profile.md");
        fs::write(
            &task_path,
            "---\nid: 42\ntitle: profile\nstatus: done\npriority: high\nclaimed_by: eng-2\ntags:\n  - dispatch\nchanged_paths:\n  - src/team/dispatch/queue.rs\nclass: standard\n---\n\nTouch src/team/dispatch/mod.rs too.\n",
        )
        .unwrap();

        let task = Task::from_file(&task_path).unwrap();
        let profiles =
            build_engineer_profiles(&["eng-1".to_string(), "eng-2".to_string()], &[task]).unwrap();
        let profile = profiles.get("eng-2").unwrap();

        assert_eq!(profile.total_completions, 1);
        assert!(profile.domain_tags.contains("dispatch"));
        assert!(
            profile
                .active_file_paths
                .contains("src/team/dispatch/queue.rs")
        );
    }

    #[test]
    fn build_profiles_counts_active_load_and_conflict_signals() {
        let task = Task {
            id: 7,
            title: "conflicted".to_string(),
            status: "review".to_string(),
            priority: "high".to_string(),
            claimed_by: Some("eng-1".to_string()),
            claimed_at: None,
            claim_ttl_secs: None,
            claim_expires_at: None,
            last_progress_at: None,
            claim_warning_sent_at: None,
            claim_extensions: None,
            last_output_bytes: None,
            blocked: Some("merge conflict".to_string()),
            tags: vec!["daemon".to_string()],
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
            description: "Resolve rebase conflict in src/team/daemon/mod.rs".to_string(),
            batty_config: None,
            source_path: PathBuf::new(),
        };

        let profiles = build_engineer_profiles(&["eng-1".to_string()], &[task]).unwrap();
        let profile = profiles.get("eng-1").unwrap();
        assert_eq!(profile.active_task_count, 1);
        assert_eq!(profile.recent_merge_conflicts, 1);
    }

    #[test]
    fn load_engineer_profiles_merges_recent_persisted_history() {
        let tmp = tempfile::tempdir().unwrap();
        let persisted = PersistedEngineerProfiles {
            version: engineer_profiles_format_version(),
            updated_at: Some(Utc::now().to_rfc3339()),
            completions: vec![PersistedTaskProfile {
                engineer: "eng-2".to_string(),
                task_id: 9,
                completed_at: Utc::now().to_rfc3339(),
                changed_paths: vec!["src/team/dispatch/queue.rs".to_string()],
                tags: vec!["dispatch".to_string()],
                recent_merge_conflict: false,
            }],
        };
        write_persisted_engineer_profiles(tmp.path(), &persisted).unwrap();

        let profiles = load_engineer_profiles(tmp.path(), &["eng-2".to_string()], &[]).unwrap();
        let profile = profiles.get("eng-2").unwrap();

        assert_eq!(profile.total_completions, 1);
        assert!(profile.domain_tags.contains("dispatch"));
        assert!(
            profile
                .active_file_paths
                .contains("src/team/dispatch/queue.rs")
        );
    }

    #[test]
    fn persist_completed_task_profile_prunes_old_history() {
        let tmp = tempfile::tempdir().unwrap();
        let stale_completed_at =
            (Utc::now() - Duration::days(PROFILE_RETENTION_DAYS + 1)).to_rfc3339();
        let persisted = PersistedEngineerProfiles {
            version: engineer_profiles_format_version(),
            updated_at: Some(Utc::now().to_rfc3339()),
            completions: vec![PersistedTaskProfile {
                engineer: "eng-1".to_string(),
                task_id: 1,
                completed_at: stale_completed_at,
                changed_paths: vec!["src/old.rs".to_string()],
                tags: vec!["old".to_string()],
                recent_merge_conflict: false,
            }],
        };
        write_persisted_engineer_profiles(tmp.path(), &persisted).unwrap();

        let task_path = tmp.path().join("042-profile.md");
        fs::write(
            &task_path,
            "---\nid: 42\ntitle: profile\nstatus: done\npriority: high\nclaimed_by: eng-2\ntags:\n  - dispatch\nchanged_paths:\n  - src/team/dispatch/queue.rs\nclass: standard\ncompleted: 2026-04-06T03:00:00-04:00\n---\n\nTouch src/team/dispatch/mod.rs too.\n",
        )
        .unwrap();

        let task = Task::from_file(&task_path).unwrap();
        persist_completed_task_profile(tmp.path(), &task).unwrap();

        let loaded =
            load_engineer_profiles(tmp.path(), &["eng-1".to_string(), "eng-2".to_string()], &[])
                .unwrap();
        assert_eq!(loaded.get("eng-1").unwrap().total_completions, 0);
        assert_eq!(loaded.get("eng-2").unwrap().total_completions, 1);
    }

    #[test]
    fn load_engineer_profiles_reads_telemetry_reliability_metrics() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".batty")).unwrap();
        let conn = telemetry_db::open(tmp.path()).unwrap();
        for task_id in 1..=5 {
            telemetry_db::insert_event(
                &conn,
                &TeamEvent::task_assigned("eng-2", &task_id.to_string()),
            )
            .unwrap();
            telemetry_db::insert_event(
                &conn,
                &TeamEvent::quality_metrics_recorded(&QualityMetricsInfo {
                    backend: "codex",
                    role: "eng-2",
                    task: &task_id.to_string(),
                    narration_ratio: 0.1,
                    commit_frequency: 1.0,
                    first_pass_test_rate: 1.0,
                    retry_rate: 0.0,
                    time_to_completion_secs: 120,
                }),
            )
            .unwrap();
            telemetry_db::insert_event(
                &conn,
                &TeamEvent::task_completed("eng-2", Some(&task_id.to_string())),
            )
            .unwrap();
        }

        let profiles = load_engineer_profiles(tmp.path(), &["eng-2".to_string()], &[]).unwrap();
        let profile = profiles.get("eng-2").unwrap();
        assert_eq!(profile.telemetry_completed_tasks, 5);
        assert!((profile.completion_rate - 1.0).abs() < f64::EPSILON);
        assert_eq!(profile.avg_task_duration_secs, Some(120.0));
        assert_eq!(profile.first_pass_test_rate, Some(1.0));
    }

    #[test]
    fn rank_engineers_prefers_higher_completion_rate_when_telemetry_is_sufficient() {
        let engineers = vec!["eng-1".to_string(), "eng-2".to_string()];
        let profiles = HashMap::from([
            (
                "eng-1".to_string(),
                EngineerProfile {
                    telemetry_completed_tasks: 5,
                    completion_rate: 0.4,
                    ..EngineerProfile::default()
                },
            ),
            (
                "eng-2".to_string(),
                EngineerProfile {
                    telemetry_completed_tasks: 5,
                    completion_rate: 0.9,
                    ..EngineerProfile::default()
                },
            ),
        ]);

        let ranked = rank_engineers_for_task(&engineers, &profiles, &task(&[], ""), &policy());
        assert_eq!(ranked[0], "eng-2");
    }

    #[test]
    fn explain_routing_falls_back_when_any_engineer_lacks_enough_samples() {
        let engineers = vec!["eng-2".to_string(), "eng-1".to_string()];
        let profiles = HashMap::from([
            (
                "eng-1".to_string(),
                EngineerProfile {
                    telemetry_completed_tasks: 4,
                    completion_rate: 1.0,
                    ..EngineerProfile::default()
                },
            ),
            (
                "eng-2".to_string(),
                EngineerProfile {
                    telemetry_completed_tasks: 7,
                    completion_rate: 0.2,
                    ..EngineerProfile::default()
                },
            ),
        ]);

        let explanation =
            explain_routing_for_task(&engineers, &profiles, &task(&[], ""), &policy());
        assert!(explanation.fallback_to_round_robin);
        assert_eq!(explanation.chosen_engineer.as_deref(), Some("eng-1"));
    }

    #[test]
    fn explain_routing_keeps_scored_mode_when_no_telemetry_exists() {
        let engineers = vec!["eng-1".to_string(), "eng-2".to_string()];
        let profiles = HashMap::from([
            ("eng-1".to_string(), EngineerProfile::default()),
            (
                "eng-2".to_string(),
                EngineerProfile {
                    domain_tags: HashSet::from(["dispatch".to_string()]),
                    ..EngineerProfile::default()
                },
            ),
        ]);

        let explanation =
            explain_routing_for_task(&engineers, &profiles, &task(&["dispatch"], ""), &policy());
        assert!(!explanation.fallback_to_round_robin);
        assert_eq!(explanation.chosen_engineer.as_deref(), Some("eng-2"));
    }
}
