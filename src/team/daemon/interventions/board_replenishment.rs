//! Board replenishment intervention: nudges idle architects when the
//! unblocked todo queue falls below the configured threshold and idle
//! engineers have no assigned work.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{info, warn};

use super::super::*;
use super::{OwnedTaskInterventionState, task_needs_owned_intervention};

const BOARD_REPLENISHMENT_STREAK_REQUIRED: u64 = 2;
const BOARD_REPLENISHMENT_REQUEST_INTERVAL_SECS: u64 = 15 * 60;

struct BoardReplenishmentContext<'a> {
    idle_engineers: &'a [String],
    unblocked_todo_tasks: &'a [&'a crate::task::Task],
    todo_count: usize,
    in_progress_count: usize,
    done_count: usize,
    directive_context: Option<&'a str>,
}

impl TeamDaemon {
    pub(in super::super) fn maybe_intervene_board_replenishment(&mut self) -> Result<()> {
        if super::super::super::pause_marker_path(&self.config.project_root).exists() {
            return Ok(());
        }
        if !self.config.team_config.board.auto_replenish {
            return Ok(());
        }
        if super::super::super::nudge_disabled_marker_path(&self.config.project_root, "replenish")
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
        let dispatchable_task_ids: std::collections::HashSet<u32> =
            crate::team::resolver::engineer_dispatchable_tasks(&board_dir, &self.config.members)?
                .into_iter()
                .map(|task| task.id)
                .collect();
        let replenishment_targets: Vec<MemberInstance> = self
            .config
            .members
            .iter()
            .filter(|member| member.role_type == RoleType::Architect)
            .cloned()
            .collect();
        if replenishment_targets.is_empty() {
            return Ok(());
        }
        let replenishment_keys: Vec<String> = replenishment_targets
            .iter()
            .map(|member| board_replenishment_intervention_key(&member.name))
            .collect();

        let engineer_names: Vec<String> = self
            .config
            .members
            .iter()
            .filter(|member| member.role_type == RoleType::Engineer)
            .map(|member| member.name.clone())
            .collect();
        if engineer_names.is_empty() {
            return Ok(());
        }

        // Suppress when all engineers have active tasks — transient idle is not starvation.
        if super::all_engineers_have_active_tasks(&engineer_names, &tasks) {
            tracing::debug!("suppressing board replenishment: all engineers have active tasks");
            self.clear_board_replenishment_streaks(&replenishment_keys);
            return Ok(());
        }

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
        if idle_unassigned_engineers.is_empty() {
            self.clear_board_replenishment_streaks(&replenishment_keys);
            return Ok(());
        }

        let unblocked_todo_tasks: Vec<&crate::task::Task> = tasks
            .iter()
            .filter(|task| dispatchable_task_ids.contains(&task.id))
            .collect();

        if idle_unassigned_engineers.len() <= unblocked_todo_tasks.len() {
            self.clear_board_replenishment_streaks(&replenishment_keys);
            return Ok(());
        }

        let todo_count = tasks.iter().filter(|task| task.status == "todo").count();
        let in_progress_count = tasks
            .iter()
            .filter(|task| matches!(task.status.as_str(), "in-progress" | "in_progress"))
            .count();
        let done_count = tasks
            .iter()
            .filter(|task| matches!(task.status.as_str(), "done" | "archived"))
            .count();
        let context = replenishment_context(&self.config.project_root);

        for architect in &replenishment_targets {
            // Allow replenishment even if the architect appears "working", as long
            // as they've been in that state for more than 5 minutes (likely a false
            // positive from the shim state classifier sitting on a prompt).
            const WORKING_GRACE_SECS: u64 = 300;
            let is_idle = self.is_member_idle(&architect.name);
            let is_long_working = !is_idle
                && self
                    .shim_handles
                    .get(&architect.name)
                    .map(|h| h.secs_since_state_change() > WORKING_GRACE_SECS)
                    .unwrap_or(false);
            if !is_idle && !is_long_working {
                continue;
            }
            if is_idle && !self.ready_for_idle_automation(&inbox_root, &architect.name) {
                continue;
            }

            let replenishment_key = board_replenishment_intervention_key(&architect.name);
            let signature = board_replenishment_intervention_signature(
                &idle_unassigned_engineers,
                &unblocked_todo_tasks,
                todo_count,
                in_progress_count,
                done_count,
            );
            let prior_streak = self
                .owned_task_interventions
                .get(&replenishment_key)
                .filter(|state| state.signature == signature)
                .map(|state| state.idle_epoch)
                .unwrap_or(0);
            let streak = prior_streak.saturating_add(1);
            self.owned_task_interventions.insert(
                replenishment_key.clone(),
                OwnedTaskInterventionState {
                    idle_epoch: streak,
                    signature: signature.clone(),
                    detected_at: Instant::now(),
                    escalation_sent: false,
                },
            );
            if streak < BOARD_REPLENISHMENT_STREAK_REQUIRED {
                continue;
            }
            if self.board_replenishment_on_cooldown(&replenishment_key) {
                continue;
            }

            let text = self.build_board_replenishment_message(
                architect,
                BoardReplenishmentContext {
                    idle_engineers: &idle_unassigned_engineers,
                    unblocked_todo_tasks: &unblocked_todo_tasks,
                    todo_count,
                    in_progress_count,
                    done_count,
                    directive_context: context.as_deref(),
                },
            );
            info!(
                member = %architect.name,
                idle_engineers = idle_unassigned_engineers.len(),
                unblocked_todo = unblocked_todo_tasks.len(),
                "firing board replenishment intervention"
            );
            let delivered_live = match self.queue_daemon_message(&architect.name, &text) {
                Ok(MessageDelivery::LivePane) => true,
                Ok(_) => false,
                Err(error) => {
                    warn!(member = %architect.name, error = %error, "failed to deliver board replenishment intervention");
                    continue;
                }
            };
            self.record_orchestrator_action(format!(
                "recovery: board replenishment intervention for {} (idle engineers: {}, dispatchable todo: {}, board summary done/in-progress/todo: {}/{}/{})",
                architect.name,
                idle_unassigned_engineers.len(),
                unblocked_todo_tasks.len(),
                done_count,
                in_progress_count,
                todo_count,
            ));
            self.owned_task_interventions.insert(
                replenishment_key.clone(),
                OwnedTaskInterventionState {
                    idle_epoch: streak,
                    signature,
                    detected_at: Instant::now(),
                    escalation_sent: true,
                },
            );
            self.intervention_cooldowns
                .insert(replenishment_key, Instant::now());
            if delivered_live {
                self.mark_member_working(&architect.name);
            }
        }

        Ok(())
    }

    fn build_board_replenishment_message(
        &self,
        member: &MemberInstance,
        context: BoardReplenishmentContext<'_>,
    ) -> String {
        let BoardReplenishmentContext {
            idle_engineers,
            unblocked_todo_tasks,
            todo_count,
            in_progress_count,
            done_count,
            directive_context,
        } = context;
        let idle_engineer_summary = idle_engineers.join(", ");
        let todo_summary = if unblocked_todo_tasks.is_empty() {
            "none".to_string()
        } else {
            unblocked_todo_tasks
                .iter()
                .take(4)
                .map(|task| format!("#{} {}", task.id, task.title))
                .collect::<Vec<_>>()
                .join("; ")
        };

        let mut message = format!(
            "Board needs replenishment: {} idle engineers, {} todo tasks. Current board summary: done={done_count}, in-progress={in_progress_count}, todo={todo_count}. Idle engineers: {idle_engineer_summary}. Dispatchable todo tasks: {todo_summary}. Create tasks from planning/roadmap.md.",
            idle_engineers.len(),
            unblocked_todo_tasks.len()
        );

        if let Some(context) = directive_context {
            message.push_str("\n\nReplenishment context:\n");
            message.push_str(context);
        }

        if member.role_type == RoleType::Architect {
            message.push_str("\n\nACTION REQUIRED: Create concrete tasks from `planning/roadmap.md` and send the manager a structured directive now. This request is rate-limited to once every 15 minutes.");
        }

        if let Some(parent) = &member.reports_to {
            message.push_str(&format!(
                "\n\nAfter creating work, report with `batty send {parent} \"board replenished: <what was added>\"`."
            ));
        }

        message.push_str(
            "\nDo not leave idle engineers without executable work. Create the next concrete tasks or explain the exact blocker now.",
        );
        message
    }

    fn board_replenishment_on_cooldown(&self, key: &str) -> bool {
        let cooldown = Duration::from_secs(BOARD_REPLENISHMENT_REQUEST_INTERVAL_SECS);
        self.intervention_cooldowns
            .get(key)
            .is_some_and(|fired_at| fired_at.elapsed() < cooldown)
    }

    fn clear_board_replenishment_streaks(&mut self, keys: &[String]) {
        for key in keys {
            self.owned_task_interventions.remove(key);
        }
    }
}

pub(super) fn board_replenishment_intervention_key(member_name: &str) -> String {
    format!("replenishment::{member_name}")
}

pub(super) fn board_replenishment_intervention_signature(
    idle_engineers: &[String],
    unblocked_todo_tasks: &[&crate::task::Task],
    todo_count: usize,
    in_progress_count: usize,
    done_count: usize,
) -> String {
    let mut parts = vec![
        format!("counts:{todo_count}:{in_progress_count}:{done_count}"),
        format!("idle:{}", idle_engineers.len()),
    ];
    for engineer in idle_engineers {
        parts.push(format!("idle-free:{engineer}"));
    }
    for task in unblocked_todo_tasks {
        parts.push(format!("todo:{}:{}", task.id, task.title));
    }
    parts.sort();
    parts.join("|")
}

fn replenishment_context(project_root: &Path) -> Option<String> {
    let path = project_root
        .join(".batty")
        .join("team_config")
        .join("replenishment_context.md");
    std::fs::read_to_string(path)
        .ok()
        .map(|content| content.trim().to_string())
        .filter(|content| !content.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replenishment_key_uses_prefix() {
        assert_eq!(
            board_replenishment_intervention_key("architect"),
            "replenishment::architect"
        );
        assert_eq!(
            board_replenishment_intervention_key("arch-2"),
            "replenishment::arch-2"
        );
    }

    #[test]
    fn replenishment_signature_sorts_deterministically() {
        let sig1 = board_replenishment_intervention_signature(
            &["eng-2".to_string(), "eng-1".to_string()],
            &[],
            3,
            1,
            5,
        );
        let sig2 = board_replenishment_intervention_signature(
            &["eng-1".to_string(), "eng-2".to_string()],
            &[],
            3,
            1,
            5,
        );
        assert_eq!(sig1, sig2);
    }

    #[test]
    fn replenishment_signature_changes_with_different_counts() {
        let sig1 = board_replenishment_intervention_signature(&[], &[], 3, 1, 5);
        let sig2 = board_replenishment_intervention_signature(&[], &[], 4, 1, 5);
        assert_ne!(sig1, sig2);
    }

    #[test]
    fn replenishment_signature_includes_idle_engineers() {
        let sig = board_replenishment_intervention_signature(&["eng-1".to_string()], &[], 0, 0, 0);
        assert!(sig.contains("idle-free:eng-1"));
        assert!(sig.contains("idle:1"));
    }

    #[test]
    fn replenishment_context_returns_none_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(replenishment_context(tmp.path()), None);
    }

    #[test]
    fn replenishment_context_returns_none_when_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("replenishment_context.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "   \n  \n").unwrap();
        assert_eq!(replenishment_context(tmp.path()), None);
    }

    #[test]
    fn replenishment_context_returns_trimmed_content() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("replenishment_context.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "  Focus on launch tasks.  \n").unwrap();
        assert_eq!(
            replenishment_context(tmp.path()),
            Some("Focus on launch tasks.".to_string())
        );
    }
}
