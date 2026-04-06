//! Idle nudge automation: fires a one-shot nudge message to members
//! who have been idle past their configured timeout.

use anyhow::Result;
use tracing::{debug, info, warn};

use super::super::*;

impl TeamDaemon {
    pub(in super::super) fn maybe_fire_nudges(&mut self) -> Result<()> {
        if !self.config.team_config.automation.timeout_nudges {
            return Ok(());
        }
        if super::super::super::pause_marker_path(&self.config.project_root).exists() {
            return Ok(());
        }
        let inbox_root = inbox::inboxes_root(&self.config.project_root);

        // Load board tasks once so we can check whether engineers actually
        // have in-progress work before nudging them.
        let board_tasks = {
            let tasks_dir = self.board_dir().join("tasks");
            crate::task::load_tasks_from_dir(&tasks_dir).unwrap_or_default()
        };

        let member_names: Vec<String> = self.nudges.keys().cloned().collect();

        for name in member_names {
            let fire = {
                let schedule = &self.nudges[&name];
                if schedule.fired_this_idle {
                    false
                } else if let Some(idle_since) = schedule.idle_since {
                    idle_since.elapsed()
                        >= schedule.interval.max(self.automation_idle_grace_duration())
                        && self.ready_for_idle_automation(&inbox_root, &name)
                } else {
                    false
                }
            };

            if fire {
                // Skip nudge for engineers with no actionable (in-progress/todo) task.
                // Nudging them to "re-open your task" when they have nothing to work
                // on just wastes their context and floods the message queue.
                let is_engineer = self
                    .config
                    .members
                    .iter()
                    .any(|m| m.name == name && m.role_type == RoleType::Engineer);
                if is_engineer {
                    let has_actionable_task = board_tasks.iter().any(|task| {
                        task.claimed_by.as_deref() == Some(name.as_str())
                            && matches!(task.status.as_str(), "in-progress" | "todo")
                    });
                    if !has_actionable_task {
                        debug!(
                            member = %name,
                            "skipping nudge — engineer has no in-progress or todo task"
                        );
                        if let Some(schedule) = self.nudges.get_mut(&name) {
                            schedule.fired_this_idle = true;
                        }
                        continue;
                    }
                }
                let prompt_text = self.nudges[&name].text.clone();
                let text = format!(
                    "{prompt_text}\n\nIdle nudge: you have been idle past your configured timeout. Move the current lane forward now or report the exact blocker."
                );
                info!(member = %name, "firing nudge (idle timeout)");
                let delivered_live = match self.queue_daemon_message(&name, &text) {
                    Ok(MessageDelivery::LivePane) => true,
                    Ok(_) => false,
                    Err(error) => {
                        warn!(member = %name, error = %error, "failed to deliver nudge");
                        continue;
                    }
                };
                if let Some(schedule) = self.nudges.get_mut(&name) {
                    schedule.fired_this_idle = true;
                }
                if delivered_live {
                    self.mark_member_working(&name);
                }
            }
        }

        Ok(())
    }
}
