use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::task::Task;
use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use super::config::AllocationPolicy;
use super::team_config_dir;

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
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct EngineerPerformanceProfile {
    pub avg_task_completion_secs: Option<f64>,
    pub lines_per_hour: Option<f64>,
    pub first_pass_test_rate: Option<f64>,
    pub context_exhaustion_frequency: Option<f64>,
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

fn engineer_profiles_format_version() -> u32 {
    1
}

pub fn build_engineer_profiles(
    engineers: &[String],
    tasks: &[Task],
) -> Result<HashMap<String, EngineerProfile>> {
    build_engineer_profiles_with_history(engineers, tasks, &[])
}

pub fn load_engineer_profiles(
    project_root: &Path,
    engineers: &[String],
    tasks: &[Task],
) -> Result<HashMap<String, EngineerProfile>> {
    let persisted = load_persisted_task_profiles(project_root)?;
    let mut profiles = build_engineer_profiles_with_history(engineers, tasks, &persisted)?;
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
    let mut ranked: Vec<(String, i32)> = engineers
        .iter()
        .map(|name| {
            let score = profiles
                .get(name)
                .map(|profile| score_engineer_for_task(profile, task, policy))
                .unwrap_or_default();
            (name.clone(), score)
        })
        .collect();

    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.into_iter().map(|(name, _)| name).collect()
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
}
