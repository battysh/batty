use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;

use crate::task::Task;

use super::config::RoleType;
use super::hierarchy::MemberInstance;
use super::inbox;
use super::review::ReviewQueueState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SupervisoryPressure {
    ReviewBacklog,
    TriageBacklog,
    IdleActiveRecovery,
    DispatchGap,
    ReviewNudge,
    IdleNudge,
    RecoveryUpdate,
    ResolvedUpdate,
    StatusUpdate,
}

impl SupervisoryPressure {
    pub(crate) fn priority(self) -> u8 {
        match self {
            Self::ReviewBacklog => 0,
            Self::TriageBacklog => 1,
            Self::IdleActiveRecovery => 2,
            Self::DispatchGap => 3,
            Self::ReviewNudge => 4,
            Self::IdleNudge => 5,
            Self::RecoveryUpdate => 6,
            Self::ResolvedUpdate => 7,
            Self::StatusUpdate => 8,
        }
    }

    pub(crate) fn actionable(self) -> bool {
        matches!(
            self,
            Self::ReviewBacklog
                | Self::TriageBacklog
                | Self::IdleActiveRecovery
                | Self::DispatchGap
        )
    }

    pub(crate) fn short_label(self) -> &'static str {
        match self {
            Self::ReviewBacklog => "review backlog",
            Self::TriageBacklog => "direct-report packets",
            Self::IdleActiveRecovery => "idle active lanes",
            Self::DispatchGap => "dispatch gap",
            Self::ReviewNudge => "review nudge",
            Self::IdleNudge => "idle nudge",
            Self::RecoveryUpdate => "recovery update",
            Self::ResolvedUpdate => "resolved notice",
            Self::StatusUpdate => "status update",
        }
    }

    pub(crate) fn stall_reason_suffix(self) -> &'static str {
        match self {
            Self::ReviewBacklog => "review_backlog",
            Self::TriageBacklog => "direct_report_packets",
            Self::IdleActiveRecovery => "idle_active_lanes",
            Self::DispatchGap => "dispatch_gap",
            Self::ReviewNudge => "review_nudge",
            Self::IdleNudge => "idle_nudge",
            Self::RecoveryUpdate => "recovery_update",
            Self::ResolvedUpdate => "resolved_notice",
            Self::StatusUpdate => "status_update",
        }
    }

    pub(crate) fn status_label(self, count: usize) -> String {
        match self {
            Self::ReviewBacklog => format!("review backlog ({count})"),
            Self::TriageBacklog => format!("direct-report packets ({count})"),
            Self::IdleActiveRecovery => format!("idle active lanes ({count})"),
            Self::DispatchGap => format!("dispatch gap ({count})"),
            Self::ReviewNudge => format!("review nudge ({count})"),
            Self::IdleNudge => format!("idle nudge ({count})"),
            Self::RecoveryUpdate => format!("recovery update ({count})"),
            Self::ResolvedUpdate => format!("resolved notice ({count})"),
            Self::StatusUpdate => format!("status update ({count})"),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SupervisoryMemberActivity {
    pub(crate) idle: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SupervisoryPressureSnapshot {
    counts: HashMap<SupervisoryPressure, usize>,
}

impl SupervisoryPressureSnapshot {
    pub(crate) fn add_pressure(&mut self, pressure: SupervisoryPressure, count: usize) {
        if count == 0 {
            return;
        }
        *self.counts.entry(pressure).or_default() += count;
    }

    pub(crate) fn add_notice_body(&mut self, body: &str) {
        if let Some(pressure) = classify_supervisory_pressure(body) {
            self.add_pressure(pressure, 1);
        }
    }

    pub(crate) fn actionable_count(&self) -> usize {
        self.counts
            .keys()
            .filter(|pressure| pressure.actionable())
            .count()
    }

    pub(crate) fn top_actionable(&self) -> Option<(SupervisoryPressure, usize)> {
        self.counts
            .iter()
            .filter(|(pressure, count)| pressure.actionable() && **count > 0)
            .map(|(pressure, count)| (*pressure, *count))
            .min_by_key(|(pressure, _)| pressure.priority())
    }

    pub(crate) fn status_summary(&self) -> Option<String> {
        self.top_actionable().map(|(pressure, count)| {
            format!(
                "pressure {}: {}",
                self.actionable_count(),
                pressure.status_label(count)
            )
        })
    }
}

pub(crate) fn normalized_body(body: &str) -> String {
    body.trim().to_ascii_lowercase()
}

pub(crate) fn classify_supervisory_pressure(body: &str) -> Option<SupervisoryPressure> {
    classify_supervisory_pressure_normalized(&normalized_body(body))
}

pub(crate) fn classify_supervisory_pressure_normalized(body: &str) -> Option<SupervisoryPressure> {
    if is_review_nudge_normalized(body) {
        Some(SupervisoryPressure::ReviewNudge)
    } else if body.starts_with("review backlog detected:") {
        Some(SupervisoryPressure::ReviewBacklog)
    } else if body.starts_with("triage backlog detected:") {
        Some(SupervisoryPressure::TriageBacklog)
    } else if body.starts_with("dispatch recovery needed:")
        || body.contains("utilization recovery")
        || body.starts_with("utilization gap detected:")
        || body.starts_with("architect utilization")
    {
        Some(classify_recovery_pressure(body))
    } else if body.contains("dispatch queue") || body.contains("dispatch fallback") {
        Some(SupervisoryPressure::DispatchGap)
    } else if is_idle_nudge_normalized(body) {
        Some(SupervisoryPressure::IdleNudge)
    } else if is_resolved_supervisory_update(body) {
        Some(SupervisoryPressure::ResolvedUpdate)
    } else if body.starts_with("recovery:")
        || body.contains("lane blocked")
        || body.contains("stuck-task escalation")
    {
        Some(SupervisoryPressure::RecoveryUpdate)
    } else if is_status_update_normalized(body) {
        Some(SupervisoryPressure::StatusUpdate)
    } else {
        None
    }
}

fn classify_recovery_pressure(body: &str) -> SupervisoryPressure {
    if body.contains("still holding active work")
        || body.contains("still have active work")
        || body.contains("parked on active work")
        || body.contains("idle active lane")
        || body.contains("idle active work")
    {
        SupervisoryPressure::IdleActiveRecovery
    } else {
        SupervisoryPressure::DispatchGap
    }
}

fn is_resolved_supervisory_update(body: &str) -> bool {
    body.contains("healthy and no action is required right now")
        || body.contains("healthy; no action is required")
        || body.contains("resolved and no action is required")
        || body.contains("already resolved")
}

pub(crate) fn is_idle_nudge(body: &str) -> bool {
    is_idle_nudge_normalized(&normalized_body(body))
}

pub(crate) fn is_idle_nudge_normalized(body: &str) -> bool {
    body.contains("idle nudge:")
        || body.contains("if you are idle, take action now")
        || body.contains("you have been idle past your configured timeout")
}

pub(crate) fn is_review_nudge(body: &str) -> bool {
    is_review_nudge_normalized(&normalized_body(body))
}

pub(crate) fn is_review_nudge_normalized(body: &str) -> bool {
    body.starts_with("review nudge:")
}

pub(crate) fn is_status_update_normalized(body: &str) -> bool {
    body.starts_with("rollup:") || body.contains("status update")
}

pub(crate) fn extract_task_id(body: &str) -> Option<String> {
    let lower = body.to_ascii_lowercase();

    if let Some(pos) = lower.find("task_id") {
        let after = &body[pos + 7..];
        let digits: String = after
            .chars()
            .skip_while(|ch| !ch.is_ascii_digit())
            .take_while(|ch| ch.is_ascii_digit())
            .collect();
        if !digits.is_empty() {
            return Some(digits);
        }
    }

    if let Some(pos) = body.find('#') {
        let digits: String = body[pos + 1..]
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect();
        if !digits.is_empty() {
            return Some(digits);
        }
    }

    None
}

pub(crate) fn actionable_supervisory_pressure_count_from_bodies<'a, I>(bodies: I) -> usize
where
    I: IntoIterator<Item = &'a str>,
{
    let mut snapshot = SupervisoryPressureSnapshot::default();
    for body in bodies {
        snapshot.add_notice_body(body);
    }
    snapshot.actionable_count()
}

pub(crate) fn supervisory_pending_pressure(
    inbox_root: &Path,
    member_name: &str,
) -> Result<SupervisoryPressureSnapshot> {
    let mut snapshot = SupervisoryPressureSnapshot::default();
    for message in crate::team::inbox_tiered::pending_messages_union(inbox_root, member_name)? {
        snapshot.add_notice_body(&message.body);
    }
    Ok(snapshot)
}

pub(crate) fn supervisory_pressure_snapshots(
    project_root: &Path,
    members: &[MemberInstance],
    activity: &HashMap<String, SupervisoryMemberActivity>,
) -> HashMap<String, SupervisoryPressureSnapshot> {
    let board_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board");
    let tasks_dir = board_dir.join("tasks");
    let tasks = crate::task::load_tasks_from_dir(&tasks_dir).unwrap_or_default();
    let inbox_root = inbox::inboxes_root(project_root);
    let direct_reports = crate::team::status::direct_reports_by_member(members);
    let dispatchable_task_ids: std::collections::HashSet<u32> =
        crate::team::resolver::dispatchable_tasks(&board_dir)
            .unwrap_or_default()
            .into_iter()
            .map(|task| task.id)
            .collect();
    let engineer_names: Vec<String> = members
        .iter()
        .filter(|member| member.role_type == RoleType::Engineer)
        .map(|member| member.name.clone())
        .collect();

    members
        .iter()
        .filter(|member| matches!(member.role_type, RoleType::Architect | RoleType::Manager))
        .map(|member| {
            let mut snapshot =
                supervisory_pending_pressure(&inbox_root, &member.name).unwrap_or_default();
            if let Some(reports) = direct_reports.get(&member.name)
                && let Ok(triage) = crate::team::status::delivered_direct_report_triage_state(
                    &inbox_root,
                    &member.name,
                    reports,
                )
            {
                snapshot.add_pressure(SupervisoryPressure::TriageBacklog, triage.count);
            }

            let live_review_count = tasks
                .iter()
                .filter(|task| {
                    review_backlog_owner_for_task(task, members).as_deref()
                        == Some(member.name.as_str())
                        && matches!(
                            crate::team::review::classify_review_task(project_root, task, &tasks),
                            ReviewQueueState::Current
                        )
                })
                .count();
            snapshot.add_pressure(SupervisoryPressure::ReviewBacklog, live_review_count);

            match member.role_type {
                RoleType::Manager => {
                    if let Some(reports) = direct_reports.get(&member.name) {
                        let report_snapshots: Vec<(bool, Vec<u32>)> = reports
                            .iter()
                            .map(|report| {
                                let idle = activity.get(report).copied().unwrap_or_default().idle;
                                let active_task_ids = tasks
                                    .iter()
                                    .filter(|task| {
                                        task.claimed_by.as_deref() == Some(report.as_str())
                                    })
                                    .filter(|task| task_needs_supervisory_recovery(task))
                                    .map(|task| task.id)
                                    .collect::<Vec<_>>();
                                (idle, active_task_ids)
                            })
                            .collect();
                        let all_reports_idle = !report_snapshots.is_empty()
                            && report_snapshots.iter().all(|(idle, _)| *idle);
                        if all_reports_idle {
                            let idle_active = report_snapshots
                                .iter()
                                .filter(|(_, task_ids)| !task_ids.is_empty())
                                .count();
                            snapshot
                                .add_pressure(SupervisoryPressure::IdleActiveRecovery, idle_active);

                            let idle_unassigned = report_snapshots
                                .iter()
                                .filter(|(_, task_ids)| task_ids.is_empty())
                                .count();
                            if idle_unassigned > 0 && !dispatchable_task_ids.is_empty() {
                                snapshot.add_pressure(
                                    SupervisoryPressure::DispatchGap,
                                    idle_unassigned.min(dispatchable_task_ids.len()),
                                );
                            }
                        }
                    }
                }
                RoleType::Architect
                    if !engineer_names.is_empty()
                        && !all_engineers_have_active_tasks(&engineer_names, &tasks) =>
                {
                    let working_engineers = engineer_names
                        .iter()
                        .filter(|name| !activity.get(*name).copied().unwrap_or_default().idle)
                        .count();
                    if working_engineers < engineer_names.len().div_ceil(2) {
                        let idle_active = engineer_names
                            .iter()
                            .filter(|name| activity.get(*name).copied().unwrap_or_default().idle)
                            .filter(|name| {
                                tasks.iter().any(|task| {
                                    task.claimed_by.as_deref() == Some(name.as_str())
                                        && task_needs_supervisory_recovery(task)
                                })
                            })
                            .count();
                        snapshot.add_pressure(SupervisoryPressure::IdleActiveRecovery, idle_active);

                        let idle_unassigned = engineer_names
                            .iter()
                            .filter(|name| activity.get(*name).copied().unwrap_or_default().idle)
                            .filter(|name| {
                                !tasks.iter().any(|task| {
                                    task.claimed_by.as_deref() == Some(name.as_str())
                                        && task_needs_supervisory_recovery(task)
                                })
                            })
                            .count();
                        if idle_unassigned > 0 && !dispatchable_task_ids.is_empty() {
                            snapshot.add_pressure(
                                SupervisoryPressure::DispatchGap,
                                idle_unassigned.min(dispatchable_task_ids.len()),
                            );
                        }
                    }
                }
                _ => {}
            }

            (member.name.clone(), snapshot)
        })
        .collect()
}

fn task_needs_supervisory_recovery(task: &Task) -> bool {
    !matches!(task.status.as_str(), "review" | "done" | "archived")
}

fn review_backlog_owner_for_task(task: &Task, members: &[MemberInstance]) -> Option<String> {
    if task.status != "review" {
        return None;
    }
    let claimed_by = task.claimed_by.as_deref()?;
    Some(
        members
            .iter()
            .find(|member| member.name == claimed_by)
            .and_then(|member| member.reports_to.clone())
            .unwrap_or_else(|| claimed_by.to_string()),
    )
}

fn all_engineers_have_active_tasks(engineer_names: &[String], tasks: &[Task]) -> bool {
    !engineer_names.is_empty()
        && engineer_names.iter().all(|name| {
            tasks.iter().any(|task| {
                task.claimed_by.as_deref() == Some(name.as_str())
                    && matches!(task.status.as_str(), "in-progress" | "in_progress")
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_review_backlog_pressure() {
        assert_eq!(
            classify_supervisory_pressure_normalized(&normalized_body(
                "Review backlog detected: direct-report work is waiting for your review."
            )),
            Some(SupervisoryPressure::ReviewBacklog)
        );
    }

    #[test]
    fn classify_triage_backlog_pressure() {
        assert_eq!(
            classify_supervisory_pressure_normalized(&normalized_body(
                "Triage backlog detected: 2 direct-report result packet(s) are waiting."
            )),
            Some(SupervisoryPressure::TriageBacklog)
        );
    }

    #[test]
    fn classify_idle_active_recovery_pressure() {
        assert_eq!(
            classify_supervisory_pressure_normalized(&normalized_body(
                "Dispatch recovery needed: idle reports still holding active work."
            )),
            Some(SupervisoryPressure::IdleActiveRecovery)
        );
    }

    #[test]
    fn classify_dispatch_gap_pressure() {
        assert_eq!(
            classify_supervisory_pressure_normalized(&normalized_body(
                "Utilization recovery needed: 2 idle engineer(s) have no active task and 2 dispatchable task(s) are available."
            )),
            Some(SupervisoryPressure::DispatchGap)
        );
    }

    #[test]
    fn classify_resolved_supervisory_update() {
        assert_eq!(
            classify_supervisory_pressure_normalized(&normalized_body(
                "Rollup: review backlog is healthy and no action is required right now."
            )),
            Some(SupervisoryPressure::ResolvedUpdate)
        );
    }

    #[test]
    fn snapshot_status_summary_uses_top_actionable_pressure() {
        let mut snapshot = SupervisoryPressureSnapshot::default();
        snapshot.add_pressure(SupervisoryPressure::DispatchGap, 2);
        snapshot.add_pressure(SupervisoryPressure::ReviewBacklog, 1);

        assert_eq!(
            snapshot.status_summary().as_deref(),
            Some("pressure 2: review backlog (1)")
        );
    }

    #[test]
    fn actionable_pressure_count_from_bodies_dedupes_repeated_stale_updates() {
        let bodies = [
            "Rollup: review backlog is healthy and no action is required right now.",
            "Rollup: review backlog is healthy and no action is required right now.",
            "Review backlog detected: direct-report work is waiting for your review.",
        ];

        assert_eq!(actionable_supervisory_pressure_count_from_bodies(bodies), 1);
    }

    #[test]
    fn classify_idle_nudge_pressure_from_instructional_text() {
        assert_eq!(
            classify_supervisory_pressure_normalized(&normalized_body(
                "If you are idle, take action NOW"
            )),
            Some(SupervisoryPressure::IdleNudge)
        );
    }

    #[test]
    fn classify_status_update_pressure() {
        assert_eq!(
            classify_supervisory_pressure_normalized(&normalized_body(
                "Status update: triage queue is unchanged."
            )),
            Some(SupervisoryPressure::StatusUpdate)
        );
    }

    #[test]
    fn extract_task_id_prefers_task_id_field() {
        assert_eq!(
            extract_task_id(r#"{"task_id": 99, "body": "Task #42"}"#),
            Some("99".to_string())
        );
    }

    #[test]
    fn extract_task_id_falls_back_to_hash_reference() {
        assert_eq!(extract_task_id("Task #42 is done"), Some("42".to_string()));
    }
}
