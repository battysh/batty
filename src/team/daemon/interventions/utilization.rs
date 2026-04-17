//! Architect utilization intervention: nudges idle architects when team
//! throughput is low — too many engineers are idle while executable work
//! exists on the board.

use std::time::Instant;

use anyhow::Result;
use tracing::{info, warn};

use super::super::*;
use super::{OwnedTaskInterventionState, task_needs_owned_intervention};
use crate::team::config::PlanningDirectiveFile;

impl TeamDaemon {
    pub(in super::super) fn maybe_intervene_architect_utilization(&mut self) -> Result<()> {
        if !self
            .config
            .team_config
            .automation
            .architect_utilization_interventions
        {
            return Ok(());
        }
        if super::super::super::pause_marker_path(&self.config.project_root).exists() {
            return Ok(());
        }
        if super::super::super::nudge_disabled_marker_path(&self.config.project_root, "utilization")
            .exists()
        {
            return Ok(());
        }

        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let inbox_root = inbox::inboxes_root(&self.config.project_root);
        let tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks"))?;
        let direct_reports =
            super::super::super::status::direct_reports_by_member(&self.config.members);
        let engineer_names: Vec<String> = self
            .config
            .members
            .iter()
            .filter(|member| member.role_type == RoleType::Engineer)
            .map(|member| member.name.clone())
            .collect();
        let total_engineers = engineer_names.len();
        if total_engineers == 0 {
            return Ok(());
        }

        // Suppress when all engineers have active tasks — transient idle is not starvation.
        if super::all_engineers_have_active_tasks(&engineer_names, &tasks) {
            tracing::debug!(
                "suppressing utilization intervention: all engineers have active tasks"
            );
            return Ok(());
        }

        let working_engineers: Vec<String> = engineer_names
            .iter()
            .filter(|name| !self.is_member_idle(name))
            .cloned()
            .collect();
        let idle_unassigned_engineers: Vec<String> = engineer_names
            .iter()
            .filter(|name| self.is_member_idle(name))
            .filter(|name| {
                !tasks.iter().any(|task| {
                    task.claimed_by.as_deref() == Some(name.as_str())
                        && task_needs_owned_intervention(task.status.as_str())
                })
            })
            .cloned()
            .collect();
        let idle_active_engineers: Vec<(String, Vec<u32>)> = engineer_names
            .iter()
            .filter(|name| self.is_member_idle(name))
            // #702: when the owned-task intervention has just nudged the engineer
            // directly and hasn't yet escalated to their manager, surfacing the
            // same engineer to the architect duplicates the nudge in the same
            // tick — burning a supervisory turn on information the engineer is
            // already being asked to act on. Once owned-task gives up and
            // escalates (`escalation_sent = true`), the architect signal becomes
            // load-bearing again, so we only suppress the pre-escalation window.
            .filter(|name| {
                !self
                    .owned_task_interventions
                    .get(name.as_str())
                    .is_some_and(|state| !state.escalation_sent)
            })
            .filter_map(|name| {
                let task_ids: Vec<u32> = tasks
                    .iter()
                    .filter(|task| task.claimed_by.as_deref() == Some(name.as_str()))
                    .filter(|task| task_needs_owned_intervention(task.status.as_str()))
                    .map(|task| task.id)
                    .collect();
                (!task_ids.is_empty()).then(|| (name.clone(), task_ids))
            })
            .collect();
        let dispatchable_task_ids: std::collections::HashSet<u32> =
            crate::team::resolver::dispatchable_tasks(&board_dir)?
                .into_iter()
                .map(|task| task.id)
                .collect();
        let unassigned_open_tasks: Vec<&crate::task::Task> = tasks
            .iter()
            .filter(|task| dispatchable_task_ids.contains(&task.id))
            .collect();

        let utilization_gap = !idle_active_engineers.is_empty()
            || (!idle_unassigned_engineers.is_empty() && !unassigned_open_tasks.is_empty());
        if !utilization_gap {
            return Ok(());
        }
        if working_engineers.len() >= total_engineers.div_ceil(2) {
            return Ok(());
        }

        let architect_members: Vec<MemberInstance> = self
            .config
            .members
            .iter()
            .filter(|member| {
                member.role_type == RoleType::Architect && direct_reports.contains_key(&member.name)
            })
            .cloned()
            .collect();

        for architect in architect_members {
            let stall_threshold = self.config.team_config.workflow_policy.stall_threshold_secs;
            let supervisory_stalled =
                self.is_supervisory_lane_stalled(&architect.name, stall_threshold);
            if !self.is_member_idle(&architect.name) && !supervisory_stalled {
                continue;
            }
            if supervisory_stalled {
                let reason = self.supervisory_progress_signal(&architect.name, stall_threshold);
                self.record_supervisory_stall_reason(&architect.name, stall_threshold, reason);
            }
            if !supervisory_stalled && !self.ready_for_idle_automation(&inbox_root, &architect.name)
            {
                continue;
            }

            let utilization_key = architect_utilization_intervention_key(&architect.name);
            let signature = architect_utilization_intervention_signature(
                &working_engineers,
                &idle_active_engineers,
                &idle_unassigned_engineers,
                &unassigned_open_tasks,
            );
            if self
                .owned_task_interventions
                .get(&utilization_key)
                .is_some_and(|state| state.signature == signature)
            {
                continue;
            }
            if self.utilization_intervention_on_cooldown(&utilization_key) {
                continue;
            }

            let text = self.build_architect_utilization_message(
                &architect,
                &working_engineers,
                &idle_active_engineers,
                &idle_unassigned_engineers,
                &unassigned_open_tasks,
            );
            info!(
                member = %architect.name,
                working_engineers = working_engineers.len(),
                idle_active_engineers = idle_active_engineers.len(),
                idle_unassigned_engineers = idle_unassigned_engineers.len(),
                unassigned_open_tasks = unassigned_open_tasks.len(),
                "firing architect utilization intervention"
            );
            let delivered_live = match self.queue_daemon_message(&architect.name, &text) {
                Ok(MessageDelivery::LivePane) => true,
                Ok(_) => false,
                Err(error) => {
                    warn!(member = %architect.name, error = %error, "failed to deliver architect utilization intervention");
                    continue;
                }
            };
            self.record_orchestrator_action(format!(
                "recovery: utilization intervention for {} (working engineers: {}, idle active: {}, idle unassigned: {}, open tasks: {})",
                architect.name,
                working_engineers.len(),
                idle_active_engineers.len(),
                idle_unassigned_engineers.len(),
                unassigned_open_tasks.len()
            ));
            let idle_epoch = self
                .triage_idle_epochs
                .get(&architect.name)
                .copied()
                .unwrap_or(0);
            self.owned_task_interventions.insert(
                utilization_key.clone(),
                OwnedTaskInterventionState {
                    idle_epoch,
                    signature,
                    detected_at: Instant::now(),
                    escalation_sent: false,
                },
            );
            self.intervention_cooldowns
                .insert(utilization_key, Instant::now());
            if delivered_live {
                self.mark_member_working(&architect.name);
            }
        }

        Ok(())
    }

    pub(super) fn build_architect_utilization_message(
        &self,
        member: &MemberInstance,
        working_engineers: &[String],
        idle_active_engineers: &[(String, Vec<u32>)],
        idle_unassigned_engineers: &[String],
        unassigned_open_tasks: &[&crate::task::Task],
    ) -> String {
        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let board_dir_str = board_dir.display();
        let working_summary = if working_engineers.is_empty() {
            "none".to_string()
        } else {
            working_engineers.join(", ")
        };
        let idle_active_summary = if idle_active_engineers.is_empty() {
            "none".to_string()
        } else {
            idle_active_engineers
                .iter()
                .map(|(engineer, task_ids)| {
                    let ids = task_ids
                        .iter()
                        .map(|id| format!("#{id}"))
                        .collect::<Vec<_>>()
                        .join(",");
                    format!("{engineer} on {ids}")
                })
                .collect::<Vec<_>>()
                .join("; ")
        };
        let idle_unassigned_summary = if idle_unassigned_engineers.is_empty() {
            "none".to_string()
        } else {
            idle_unassigned_engineers.join(", ")
        };
        let open_task_summary = if unassigned_open_tasks.is_empty() {
            "none".to_string()
        } else {
            unassigned_open_tasks
                .iter()
                .take(4)
                .map(|task| format!("#{} ({}) {}", task.id, task.status, task.title))
                .collect::<Vec<_>>()
                .join("; ")
        };
        let idle_active_count = idle_active_engineers.len();
        let idle_unassigned_count = idle_unassigned_engineers.len();
        let dispatchable_count = unassigned_open_tasks.len();

        let mut message = format!(
            "Utilization recovery needed: {idle_unassigned_count} idle engineer(s) have no active task, {idle_active_count} idle engineer(s) are still parked on active work, and {dispatchable_count} dispatchable task(s) are available. Top dispatchable items: {open_task_summary}. Working engineers: {working_summary}. Idle active lanes: {idle_active_summary}. Idle free engineers: {idle_unassigned_summary}.\n\
            Recover throughput now:\n\
            1. `batty status`\n\
            2. `kanban-md list --dir {board_dir_str} --status in-progress`\n\
            3. `kanban-md list --dir {board_dir_str} --status todo`\n\
            4. `kanban-md list --dir {board_dir_str} --status backlog`"
        );

        if let Some((engineer, task_ids)) = idle_active_engineers.first() {
            let task_id = task_ids[0];
            if let Some(lead) = self.manager_for_member_name(engineer) {
                message.push_str(&format!(
                    "\n5. For an idle active lane, force lead action now with `batty send {lead} \"Engineer {engineer} is idle on Task #{task_id}. Normalize the board state or unblock/reassign this lane now.\"`."
                ));
            }
        }

        if let (Some(engineer), Some(task)) = (
            idle_unassigned_engineers.first(),
            unassigned_open_tasks.first(),
        ) {
            if let Some(lead) = self.manager_for_member_name(engineer) {
                message.push_str(&format!(
                    "\n6. For unused capacity, dispatch through the lead now with `batty send {lead} \"Start Task #{task_id} on {engineer} now: {title}\"`.",
                    task_id = task.id,
                    title = task.title,
                ));
            }
        }

        message.push_str(
            "\n7. If the board has no executable work left, create the next concrete task or ask the human only for a real policy decision. Do not leave the team underloaded without an explicit next dispatch.",
        );
        if let Some(parent) = &member.reports_to {
            message.push_str(&format!(
                "\n8. Report the recovery decision upward with `batty send {parent} \"utilization recovery: <what was dispatched or why the board is blocked>\"`."
            ));
        }
        self.prepend_planning_directive(
            PlanningDirectiveFile::ReplenishmentContext,
            "Replenishment context:",
            message,
        )
    }
}

pub(super) fn architect_utilization_intervention_key(member_name: &str) -> String {
    format!("utilization::{member_name}")
}

pub(super) fn architect_utilization_intervention_signature(
    working_engineers: &[String],
    idle_active_engineers: &[(String, Vec<u32>)],
    idle_unassigned_engineers: &[String],
    unassigned_open_tasks: &[&crate::task::Task],
) -> String {
    let mut parts = Vec::new();
    for engineer in working_engineers {
        parts.push(format!("working:{engineer}"));
    }
    for (engineer, task_ids) in idle_active_engineers {
        let ids = task_ids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        parts.push(format!("idle-active:{engineer}:{ids}"));
    }
    for engineer in idle_unassigned_engineers {
        parts.push(format!("idle-free:{engineer}"));
    }
    for task in unassigned_open_tasks {
        parts.push(format!("open:{}:{}", task.id, task.status));
    }
    parts.sort();
    parts.join("|")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::test_support::{
        TestDaemonBuilder, architect_member, engineer_member, manager_member,
    };

    #[test]
    fn utilization_key_uses_utilization_prefix() {
        assert_eq!(
            architect_utilization_intervention_key("architect"),
            "utilization::architect"
        );
    }

    #[test]
    fn utilization_signature_empty_inputs() {
        assert_eq!(
            architect_utilization_intervention_signature(&[], &[], &[], &[]),
            ""
        );
    }

    #[test]
    fn utilization_signature_working_only() {
        let sig =
            architect_utilization_intervention_signature(&["eng-1".to_string()], &[], &[], &[]);
        assert_eq!(sig, "working:eng-1");
    }

    #[test]
    fn utilization_signature_idle_active_includes_task_ids() {
        let sig = architect_utilization_intervention_signature(
            &[],
            &[("eng-1".to_string(), vec![10, 20])],
            &[],
            &[],
        );
        assert_eq!(sig, "idle-active:eng-1:10,20");
    }

    #[test]
    fn utilization_signature_idle_free() {
        let sig =
            architect_utilization_intervention_signature(&[], &[], &["eng-3".to_string()], &[]);
        assert_eq!(sig, "idle-free:eng-3");
    }

    #[test]
    fn utilization_message_keeps_batchable_prefix_for_supervisors() {
        let tmp = tempfile::tempdir().unwrap();
        let architect = architect_member("architect");
        let daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                architect.clone(),
                manager_member("manager", Some("architect")),
                engineer_member("eng-1", Some("manager"), false),
            ])
            .build();

        let message = daemon.build_architect_utilization_message(
            &architect,
            &[],
            &[],
            &["eng-1".to_string()],
            &[&crate::task::Task {
                id: 42,
                title: "Inbox triage".to_string(),
                status: "todo".to_string(),
                priority: "high".to_string(),
                assignee: None,
                claimed_by: None,
                claimed_at: None,
                claim_ttl_secs: None,
                claim_expires_at: None,
                last_progress_at: None,
                claim_warning_sent_at: None,
                claim_extensions: None,
                last_output_bytes: None,
                blocked: None,
                tags: Vec::new(),
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
                description: String::new(),
                batty_config: None,
                source_path: std::path::PathBuf::new(),
            }],
        );

        assert!(message.starts_with("Utilization recovery needed:"));
        assert!(message.contains("Top dispatchable items: #42 (todo) Inbox triage."));
        assert!(message.contains("Recover throughput now:"));
    }
}
