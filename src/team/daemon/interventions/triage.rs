//! Triage backlog intervention: nudges idle managers who have unreviewed
//! direct-report result packets waiting in their inbox.

use std::time::Instant;

use anyhow::Result;
use tracing::{info, warn};

use super::super::*;

impl TeamDaemon {
    pub(in super::super) fn maybe_intervene_triage_backlog(&mut self) -> Result<()> {
        if self
            .config
            .team_config
            .workflow_mode
            .suppresses_manager_relay()
        {
            return Ok(());
        }
        if !self.config.team_config.automation.triage_interventions {
            return Ok(());
        }
        if super::super::super::pause_marker_path(&self.config.project_root).exists() {
            return Ok(());
        }
        if super::super::super::nudge_disabled_marker_path(&self.config.project_root, "triage")
            .exists()
        {
            return Ok(());
        }

        let inbox_root = inbox::inboxes_root(&self.config.project_root);
        let direct_reports =
            super::super::super::status::direct_reports_by_member(&self.config.members);
        let member_names: Vec<String> = self.config.pane_map.keys().cloned().collect();

        for name in member_names {
            let Some(member) = self
                .config
                .members
                .iter()
                .find(|member| member.name == name)
                .cloned()
            else {
                continue;
            };
            if !self.is_member_idle(&name) {
                continue;
            }
            let actionable_pending = match inbox::pending_messages(&inbox_root, &name) {
                Ok(messages) => {
                    crate::team::delivery::actionable_supervisory_notice_count(&messages)
                }
                Err(error) => {
                    warn!(member = %name, error = %error, "failed to read pending inbox for triage gating");
                    continue;
                }
            };
            if actionable_pending > 0 {
                continue;
            }

            let Some(reports) = direct_reports.get(&name) else {
                continue;
            };

            let triage_state =
                match super::super::super::status::delivered_direct_report_triage_state(
                    &inbox_root,
                    &name,
                    reports,
                ) {
                    Ok(state) => state,
                    Err(error) => {
                        warn!(member = %name, error = %error, "failed to compute triage intervention state");
                        continue;
                    }
                };
            if triage_state.count == 0 {
                continue;
            }

            let idle_epoch = self.triage_idle_epochs.get(&name).copied().unwrap_or(0);
            if idle_epoch == 0 {
                continue;
            }

            let already_notified_for = self.triage_interventions.get(&name).copied().unwrap_or(0);
            if already_notified_for >= idle_epoch {
                continue;
            }

            let triage_cooldown_key = format!("triage::{name}");
            if self.intervention_on_cooldown(&triage_cooldown_key) {
                continue;
            }

            let text = self.build_triage_intervention_message(&member, reports, triage_state.count);
            info!(member = %name, triage_backlog = triage_state.count, "firing triage intervention");
            let delivered_live = match self.queue_daemon_message(&name, &text) {
                Ok(MessageDelivery::LivePane) => true,
                Ok(_) => false,
                Err(error) => {
                    warn!(member = %name, error = %error, "failed to deliver triage intervention");
                    continue;
                }
            };
            self.record_orchestrator_action(format!(
                "recovery: triage intervention for {} with {} pending direct-report result(s)",
                name, triage_state.count
            ));
            self.triage_interventions.insert(name.clone(), idle_epoch);
            self.intervention_cooldowns
                .insert(triage_cooldown_key, Instant::now());
            if delivered_live {
                self.mark_member_working(&name);
            }
        }

        Ok(())
    }

    fn build_triage_intervention_message(
        &self,
        member: &MemberInstance,
        direct_reports: &[String],
        triage_count: usize,
    ) -> String {
        let report_list = direct_reports.join(", ");
        let first_report = direct_reports.first().cloned().unwrap_or_default();
        let engineer_reports: Vec<&String> = direct_reports
            .iter()
            .filter(|name| {
                self.config
                    .members
                    .iter()
                    .find(|member| member.name == **name)
                    .is_some_and(|member| member.role_type == RoleType::Engineer)
            })
            .collect();
        let first_engineer = engineer_reports.first().map(|name| name.as_str());

        let mut message = format!(
            "Triage backlog detected: {triage_count} direct-report result packet(s) are waiting for review from {report_list}. Resolve the backlog now so those reports can move again.\n\
            Resolve it with Batty commands now:\n\
            1. `batty inbox {member_name}` to list the recent result packets.\n\
            2. `batty read {member_name} <ref>` for each packet you need to review in full.\n\
            3. `batty send {first_report} \"accepted / blocked / next step\"` to disposition each report and unblock the sender.",
            member_name = member.name,
        );

        if let Some(engineer) = first_engineer {
            message.push_str(&format!(
                "\n4. If more implementation is needed, issue it directly with `batty assign {engineer} \"<next task>\"`."
            ));
        }

        if let Some(parent) = &member.reports_to {
            message.push_str(&format!(
                "\n5. After triage, summarize upward with `batty send {parent} \"triage summary: accepted / blocked / reassigned / next load\"`."
            ));
        }

        message.push_str(
            "\nDo the triage now and drive the backlog to zero. Batty will remind you again the next time you become idle while triage backlog remains.",
        );
        self.prepend_member_nudge(member, message)
    }
}
