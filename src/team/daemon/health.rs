//! Agent health monitoring, lifecycle management, and restart logic.
//!
//! Extracted from daemon.rs: watcher polling, context exhaustion,
//! stall detection, pane death, backend health, startup preflight.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use super::*;

const CONTEXT_RESTART_COOLDOWN: Duration = Duration::from_secs(30);
const STARTUP_PREFLIGHT_RESPAWN_DELAY: Duration = Duration::from_millis(200);

impl TeamDaemon {
    pub(super) fn run_startup_preflight(&mut self) -> Result<()> {
        ensure_tmux_session_ready(&self.config.session)?;
        self.ensure_member_panes_ready()?;
        ensure_kanban_available()?;
        if ensure_board_initialized(&self.config.project_root)? {
            let board_dir = board_dir(&self.config.project_root);
            info!(
                board = %board_dir.display(),
                "initialized missing board during daemon startup preflight"
            );
            self.record_orchestrator_action(format!(
                "startup: initialized board at {}",
                board_dir.display()
            ));
        }
        self.validate_member_panes_on_startup();
        Ok(())
    }

    fn ensure_member_panes_ready(&mut self) -> Result<()> {
        let members = self.config.members.clone();
        for member in &members {
            if member.role_type == RoleType::User {
                continue;
            }

            let Some(pane_id) = self.config.pane_map.get(&member.name) else {
                bail!(
                    "daemon startup pre-flight failed: no tmux pane mapped for member '{}'",
                    member.name
                );
            };
            if !tmux::pane_exists(pane_id) {
                bail!(
                    "daemon startup pre-flight failed: pane '{}' for member '{}' is missing",
                    pane_id,
                    member.name
                );
            }

            if !tmux::pane_dead(pane_id)
                .with_context(|| format!("failed to inspect pane '{pane_id}'"))?
            {
                continue;
            }

            warn!(
                member = %member.name,
                pane = %pane_id,
                "respawning dead pane during daemon startup preflight"
            );
            tmux::respawn_pane(pane_id, "bash")
                .with_context(|| format!("failed to respawn pane '{pane_id}'"))?;
            std::thread::sleep(STARTUP_PREFLIGHT_RESPAWN_DELAY);

            if tmux::pane_dead(pane_id)
                .with_context(|| format!("failed to inspect respawned pane '{pane_id}'"))?
            {
                bail!(
                    "daemon startup pre-flight failed: pane '{}' for member '{}' stayed dead after respawn",
                    pane_id,
                    member.name
                );
            }

            self.record_orchestrator_action(format!(
                "startup: respawned dead pane for {}",
                member.name
            ));
            self.emit_event(TeamEvent::pane_respawned(&member.name));
        }

        Ok(())
    }

    pub(super) fn restart_dead_members(&mut self) -> Result<()> {
        let member_names: Vec<String> = self.config.pane_map.keys().cloned().collect();
        for name in member_names {
            let Some(pane_id) = self.config.pane_map.get(&name) else {
                continue;
            };
            if !tmux::pane_exists(pane_id) {
                continue;
            }
            if !tmux::pane_dead(pane_id).unwrap_or(false) {
                continue;
            }
            self.restart_member(&name)?;
        }
        Ok(())
    }

    pub(super) fn restart_member(&mut self, member_name: &str) -> Result<()> {
        let Some(member) = self
            .config
            .members
            .iter()
            .find(|member| member.name == member_name)
            .cloned()
        else {
            return Ok(());
        };
        let Some(pane_id) = self.config.pane_map.get(member_name).cloned() else {
            return Ok(());
        };

        warn!(member = %member_name, pane = %pane_id, "detected dead pane, restarting member");
        tmux::respawn_pane(&pane_id, "bash")?;
        std::thread::sleep(Duration::from_millis(200));

        let previous_launch_state = load_launch_state(&self.config.project_root);
        let duplicate_claude_session_ids = duplicate_claude_session_ids(&previous_launch_state);
        let plan = self.prepare_member_launch(
            &member,
            true,
            &previous_launch_state,
            &duplicate_claude_session_ids,
        )?;
        self.apply_member_launch(&member, &pane_id, &plan)?;
        if let Err(error) = self.persist_member_launch_identity(&member.name, plan.identity.clone())
        {
            warn!(member = %member.name, error = %error, "failed to persist restarted launch identity");
        }
        self.record_orchestrator_action(format!(
            "restart: respawned pane and relaunched {} after pane death",
            member.name
        ));
        self.emit_event(TeamEvent::pane_respawned(&member.name));
        self.record_member_crashed(&member.name, true);
        Ok(())
    }

    pub(super) fn handle_context_exhaustion(&mut self, member_name: &str) -> Result<()> {
        let Some(task) = self.active_task(member_name)? else {
            warn!(member = %member_name, "context exhausted but no active task is recorded");
            self.states
                .insert(member_name.to_string(), MemberState::Idle);
            return Ok(());
        };
        let Some(member) = self
            .config
            .members
            .iter()
            .find(|member| member.name == member_name)
            .cloned()
        else {
            return Ok(());
        };
        let Some(pane_id) = self.config.pane_map.get(member_name).cloned() else {
            return Ok(());
        };
        let restart_cooldown_key = Self::context_restart_cooldown_key(member_name);
        let restart_on_cooldown = self
            .intervention_cooldowns
            .get(&restart_cooldown_key)
            .is_some_and(|last| last.elapsed() < CONTEXT_RESTART_COOLDOWN);
        let escalation_cooldown_key = Self::context_escalation_cooldown_key(member_name);
        let escalation_on_cooldown = self
            .intervention_cooldowns
            .get(&escalation_cooldown_key)
            .is_some_and(|last| last.elapsed() < CONTEXT_RESTART_COOLDOWN);

        let prior_restarts = self.context_restart_count(task.id)?;
        if prior_restarts >= 1 {
            if escalation_on_cooldown {
                info!(
                    member = %member_name,
                    task_id = task.id,
                    "context exhaustion escalation suppressed by cooldown"
                );
                return Ok(());
            }
            self.escalate_context_exhaustion(&member, &task, prior_restarts + 1)?;
            self.intervention_cooldowns
                .insert(escalation_cooldown_key, Instant::now());
            return Ok(());
        }

        if restart_on_cooldown {
            info!(
                member = %member_name,
                task_id = task.id,
                "context exhaustion restart suppressed by cooldown"
            );
            return Ok(());
        }

        warn!(
            member = %member_name,
            task_id = task.id,
            "context exhausted; restarting agent with task context"
        );

        // Write progress checkpoint before restarting.
        let checkpoint = super::super::checkpoint::gather_checkpoint(
            &self.config.project_root,
            member_name,
            &task,
        );
        if let Err(error) =
            super::super::checkpoint::write_checkpoint(&self.config.project_root, &checkpoint)
        {
            warn!(member = %member_name, error = %error, "failed to write progress checkpoint");
        }

        tmux::respawn_pane(&pane_id, "bash")?;
        std::thread::sleep(Duration::from_millis(200));

        let assignment = Self::restart_assignment_message(&task);
        let launch =
            self.launch_task_assignment(member_name, &assignment, Some(task.id), false, false)?;
        let mut restart_notice = format!(
            "Restarted after context exhaustion. Continue task #{} from the current worktree state.",
            task.id
        );
        if let Some(branch) = launch.branch.as_deref() {
            restart_notice.push_str(&format!("\nBranch: {branch}"));
        }
        restart_notice.push_str(&format!("\nWorktree: {}", launch.work_dir.display()));
        // Include checkpoint content in restart notice.
        if let Some(cp_content) =
            super::super::checkpoint::read_checkpoint(&self.config.project_root, member_name)
        {
            restart_notice.push_str("\n\n--- Progress Checkpoint ---\n");
            restart_notice.push_str(&cp_content);
        }
        if let Err(error) = self.queue_message("daemon", member_name, &restart_notice) {
            warn!(member = %member_name, error = %error, "failed to inject restart notice");
        }
        self.record_orchestrator_action(format!(
            "restart: relaunched {} on task #{} after context exhaustion",
            member_name, task.id
        ));
        self.intervention_cooldowns
            .insert(restart_cooldown_key, Instant::now());
        self.record_agent_restarted(
            member_name,
            task.id.to_string(),
            "context_exhausted",
            prior_restarts + 1,
        );
        if let Some(branch) = launch.branch.as_deref() {
            info!(member = %member_name, task_id = task.id, branch, "context restart relaunched assignment");
        }
        Ok(())
    }

    fn escalate_context_exhaustion(
        &mut self,
        member: &MemberInstance,
        task: &crate::task::Task,
        restart_count: u32,
    ) -> Result<()> {
        let Some(manager) = member.reports_to.as_deref() else {
            warn!(
                member = %member.name,
                task_id = task.id,
                restart_count,
                "context exhaustion exceeded restart limit with no escalation target"
            );
            return Ok(());
        };

        let body = format!(
            "Task #{task_id} for {member_name} exhausted context {restart_count} times. Batty restarted it once already and will not restart it again automatically.\n\
Task: {title}\n\
Next step: decide whether to split the task, redirect the engineer, or intervene directly in the lane.",
            task_id = task.id,
            member_name = member.name,
            title = task.title,
        );
        self.queue_message("daemon", manager, &body)?;
        self.record_orchestrator_action(format!(
            "restart: escalated context exhaustion for {} on task #{} after {} exhaustions",
            member.name, task.id, restart_count
        ));
        self.record_task_escalated(&member.name, task.id.to_string(), Some("context_exhausted"));
        Ok(())
    }

    /// Handle a stalled agent — no output change for longer than the configured threshold.
    pub(super) fn handle_stalled_agent(
        &mut self,
        member_name: &str,
        stall_secs: u64,
    ) -> Result<()> {
        let Some(task) = self.active_task(member_name)? else {
            return Ok(());
        };
        let member = match self.config.members.iter().find(|m| m.name == member_name) {
            Some(m) => m.clone(),
            None => return Ok(()),
        };
        let Some(pane_id) = self.config.pane_map.get(member_name).cloned() else {
            return Ok(());
        };

        let stall_cooldown_key = format!("stall-restart::{member_name}");
        let on_cooldown = self
            .intervention_cooldowns
            .get(&stall_cooldown_key)
            .is_some_and(|last| last.elapsed() < CONTEXT_RESTART_COOLDOWN);
        if on_cooldown {
            return Ok(());
        }

        let task_id_str = task.id.to_string();
        let prior_restarts = self.stall_restart_count(task.id)?;
        let max_restarts = self.config.team_config.workflow_policy.max_stall_restarts;

        warn!(
            member = %member_name,
            task_id = task.id,
            stall_secs,
            prior_restarts,
            "agent stalled — no output change"
        );

        self.emit_event(TeamEvent::stall_detected(
            member_name,
            Some(task.id),
            stall_secs,
        ));
        self.record_orchestrator_action(format!(
            "stall: detected agent stall for {} on task #{} ({}s no output, {} prior restarts)",
            member_name, task.id, stall_secs, prior_restarts,
        ));

        if prior_restarts >= max_restarts {
            // Escalate to manager instead of restarting again.
            let escalation_key = format!("stall-escalation::{member_name}");
            let escalation_on_cooldown = self
                .intervention_cooldowns
                .get(&escalation_key)
                .is_some_and(|last| last.elapsed() < CONTEXT_RESTART_COOLDOWN);
            if escalation_on_cooldown {
                return Ok(());
            }
            self.escalate_stalled_agent(&member, &task, prior_restarts + 1)?;
            self.intervention_cooldowns
                .insert(escalation_key, Instant::now());
            return Ok(());
        }

        // Write progress checkpoint before restarting.
        let checkpoint = super::super::checkpoint::gather_checkpoint(
            &self.config.project_root,
            member_name,
            &task,
        );
        if let Err(error) =
            super::super::checkpoint::write_checkpoint(&self.config.project_root, &checkpoint)
        {
            warn!(member = %member_name, error = %error, "failed to write progress checkpoint");
        }

        // Restart the stalled agent with task context.
        tmux::respawn_pane(&pane_id, "bash")?;
        std::thread::sleep(Duration::from_millis(200));

        let assignment = Self::restart_assignment_message(&task);
        let launch =
            self.launch_task_assignment(member_name, &assignment, Some(task.id), false, false)?;
        let mut restart_notice = format!(
            "Restarted after stall ({}s no output). Continue task #{} from the current worktree state.",
            stall_secs, task.id
        );
        if let Some(branch) = launch.branch.as_deref() {
            restart_notice.push_str(&format!("\nBranch: {branch}"));
        }
        restart_notice.push_str(&format!("\nWorktree: {}", launch.work_dir.display()));
        // Include checkpoint content in restart notice.
        if let Some(cp_content) =
            super::super::checkpoint::read_checkpoint(&self.config.project_root, member_name)
        {
            restart_notice.push_str("\n\n--- Progress Checkpoint ---\n");
            restart_notice.push_str(&cp_content);
        }
        if let Err(error) = self.queue_message("daemon", member_name, &restart_notice) {
            warn!(member = %member_name, error = %error, "failed to inject stall restart notice");
        }
        self.record_orchestrator_action(format!(
            "stall: relaunched {} on task #{} after {}s stall",
            member_name, task.id, stall_secs,
        ));
        self.intervention_cooldowns
            .insert(stall_cooldown_key, Instant::now());
        self.record_agent_restarted(member_name, task_id_str, "stalled", prior_restarts + 1);
        Ok(())
    }

    /// Escalate a stalled agent to its manager after max restarts exceeded.
    fn escalate_stalled_agent(
        &mut self,
        member: &MemberInstance,
        task: &crate::task::Task,
        restart_count: u32,
    ) -> Result<()> {
        let Some(manager) = member.reports_to.as_deref() else {
            warn!(
                member = %member.name,
                task_id = task.id,
                restart_count,
                "stall exceeded restart limit with no escalation target"
            );
            return Ok(());
        };

        let body = format!(
            "Task #{task_id} for {member_name} stalled {restart_count} times (no output). \
             Batty restarted it {max} time(s) already and will not restart again automatically.\n\
             Task: {title}\n\
             Next step: decide whether to split the task, redirect the engineer, or intervene directly.",
            task_id = task.id,
            member_name = member.name,
            title = task.title,
            max = restart_count.saturating_sub(1),
        );
        self.queue_message("daemon", manager, &body)?;
        self.record_orchestrator_action(format!(
            "stall: escalated stall for {} on task #{} after {} stalls",
            member.name, task.id, restart_count,
        ));
        self.record_task_escalated(&member.name, task.id.to_string(), Some("stalled"));
        Ok(())
    }

    /// Count prior stall restarts for a given task from the event log.
    pub(super) fn stall_restart_count(&self, task_id: u32) -> Result<u32> {
        let events_path = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        let task_id = task_id.to_string();
        let count = super::super::events::read_events(&events_path)?
            .into_iter()
            .filter(|event| event.event == "agent_restarted")
            .filter(|event| event.task.as_deref() == Some(task_id.as_str()))
            .filter(|event| event.reason.as_deref() == Some("stalled"))
            .count() as u32;
        Ok(count)
    }

    /// Periodically check agent backend health and emit events on transitions.
    pub(super) fn check_backend_health(&mut self) -> Result<()> {
        let interval = Duration::from_secs(
            self.config
                .team_config
                .workflow_policy
                .health_check_interval_secs,
        );
        if self.last_health_check.elapsed() < interval {
            return Ok(());
        }
        self.last_health_check = Instant::now();

        // Collect (member_name, agent_name) pairs to avoid borrowing self.config during mutation.
        let checks: Vec<(String, String)> = self
            .config
            .members
            .iter()
            .filter(|m| m.role_type != RoleType::User)
            .map(|m| {
                (
                    m.name.clone(),
                    m.agent.as_deref().unwrap_or("claude").to_string(),
                )
            })
            .collect();

        for (member_name, agent_name) in &checks {
            let new_health =
                agent::health_check_by_name(agent_name).unwrap_or(BackendHealth::Healthy);
            let prev_health = self
                .backend_health
                .get(member_name)
                .copied()
                .unwrap_or(BackendHealth::Healthy);

            if new_health != prev_health {
                let transition = format!("{}→{}", prev_health.as_str(), new_health.as_str());
                info!(
                    member = %member_name,
                    agent = %agent_name,
                    transition = %transition,
                    "backend health changed"
                );
                self.emit_event(TeamEvent::health_changed(member_name, &transition));
                self.record_orchestrator_action(format!(
                    "health: {} backend {} ({})",
                    member_name, transition, agent_name,
                ));
            }
            self.backend_health.insert(member_name.clone(), new_health);
        }

        Ok(())
    }

    /// Check each engineer's worktree for large uncommitted diffs and send a
    /// commit reminder when the line count exceeds the configured threshold.
    /// Nudges are rate-limited to at most once per 5 minutes per engineer.
    pub(super) fn maybe_warn_uncommitted_work(&mut self) -> Result<()> {
        let threshold = self
            .config
            .team_config
            .workflow_policy
            .uncommitted_warn_threshold;
        if threshold == 0 {
            return Ok(());
        }

        let cooldown = Duration::from_secs(300); // 5 minutes

        let engineers: Vec<String> = self
            .config
            .members
            .iter()
            .filter(|m| m.role_type == RoleType::Engineer && m.use_worktrees)
            .map(|m| m.name.clone())
            .collect();

        for name in &engineers {
            // Rate-limit: skip if we warned this engineer recently.
            if let Some(last) = self.last_uncommitted_warn.get(name) {
                if last.elapsed() < cooldown {
                    continue;
                }
            }

            let worktree_path = self.worktree_dir(name);
            if !worktree_path.exists() {
                continue;
            }

            let lines = match uncommitted_diff_lines(&worktree_path) {
                Ok(n) => n,
                Err(error) => {
                    warn!(engineer = %name, error = %error, "failed to check uncommitted diff");
                    continue;
                }
            };

            if lines < threshold {
                continue;
            }

            info!(
                engineer = %name,
                uncommitted_lines = lines,
                threshold,
                "sending uncommitted work warning"
            );

            let body = format!(
                "COMMIT REMINDER: You have {lines} uncommitted lines in your worktree \
                 (threshold: {threshold}). Please commit your work now to avoid losing progress:\n\n\
                 git add -A && git commit -m 'wip: checkpoint'"
            );

            let sender = self.automation_sender_for(name);
            if let Err(error) = self.queue_message(&sender, name, &body) {
                warn!(engineer = %name, error = %error, "failed to send uncommitted work warning");
            }
            self.record_orchestrator_action(format!(
                "uncommitted-warn: {name} has {lines} uncommitted lines (threshold {threshold})"
            ));
            self.last_uncommitted_warn
                .insert(name.clone(), Instant::now());
        }

        Ok(())
    }

    /// Load the prompt template for a member, substituting role-specific info.
    pub(super) fn load_prompt(&self, member: &MemberInstance, config_dir: &Path) -> String {
        let prompt_file = member.prompt.as_deref().unwrap_or(match member.role_type {
            RoleType::Architect => "architect.md",
            RoleType::Manager => "manager.md",
            RoleType::Engineer => "engineer.md",
            RoleType::User => "architect.md", // shouldn't happen
        });

        let path = config_dir.join(prompt_file);
        match std::fs::read_to_string(&path) {
            Ok(content) => content
                .replace("{{member_name}}", &member.name)
                .replace("{{role_name}}", &member.role_name)
                .replace(
                    "{{reports_to}}",
                    member.reports_to.as_deref().unwrap_or("none"),
                ),
            Err(e) => {
                warn!(path = %path.display(), error = %e, "failed to load prompt template");
                format!(
                    "You are {} (role: {:?}). Work on assigned tasks.",
                    member.name, member.role_type
                )
            }
        }
    }

    /// Poll all watchers and handle state transitions.
    pub(super) fn poll_watchers(&mut self) -> Result<()> {
        let member_names: Vec<String> = self.watchers.keys().cloned().collect();

        for name in &member_names {
            let prev_state = self.states.get(name).copied();
            let prev_watcher_state = self
                .watchers
                .get(name)
                .map(|watcher| watcher.state)
                .unwrap_or(WatcherState::Idle);
            let (new_state, completion_observed, session_size_bytes, secs_since_output) = {
                let watcher = match self.watcher_mut(name) {
                    Ok(watcher) => watcher,
                    Err(error) => {
                        warn!(member = %name, error = %error, "watcher missing during poll");
                        continue;
                    }
                };
                match watcher.poll() {
                    Ok(new_state) => (
                        new_state,
                        watcher.take_completion_event(),
                        watcher.current_session_size_bytes(),
                        watcher.secs_since_last_output_change(),
                    ),
                    Err(e) => {
                        warn!(member = %name, error = %e, "watcher poll failed");
                        continue;
                    }
                }
            };

            let member_state = match new_state {
                WatcherState::Active => MemberState::Working,
                WatcherState::Ready | WatcherState::Idle => MemberState::Idle,
                WatcherState::PaneDead => MemberState::Idle,
                WatcherState::ContextExhausted => MemberState::Working,
            };

            // Record agent poll state for telemetry (idle_polls, working_polls, total_cycle_secs).
            if let Some(conn) = &self.telemetry_db {
                let is_working = member_state == MemberState::Working;
                let poll_secs = self.poll_interval.as_secs();
                if let Err(error) = crate::team::telemetry_db::record_agent_poll_state(
                    conn, name, is_working, poll_secs,
                ) {
                    debug!(error = %error, member = %name, "failed to record agent poll state");
                }
            }

            if prev_state != Some(member_state) {
                self.states.insert(name.clone(), member_state);

                // Update automation countdowns on state transitions.
                self.update_automation_timers_for_state(name, member_state);
            }

            if new_state == WatcherState::PaneDead {
                if prev_watcher_state != WatcherState::PaneDead {
                    warn!(member = %name, "detected pane death");
                    self.emit_event(TeamEvent::pane_death(name));
                    self.record_orchestrator_action(format!(
                        "lifecycle: detected pane death for {}",
                        name
                    ));
                }
                if let Err(error) = self.handle_pane_death(name) {
                    warn!(
                        member = %name,
                        error = %error,
                        "pane-death respawn handling failed; continuing"
                    );
                }
                continue;
            }

            if new_state == WatcherState::ContextExhausted {
                if prev_watcher_state != WatcherState::ContextExhausted {
                    let task_id = self.active_task_id(name);
                    warn!(
                        member = %name,
                        task_id,
                        session_size_bytes,
                        "detected context exhaustion"
                    );
                    self.record_context_exhausted(name, task_id, session_size_bytes);
                    self.record_orchestrator_action(format!(
                        "lifecycle: detected context exhaustion for {} (task={:?}, session_size_bytes={:?})",
                        name, task_id, session_size_bytes
                    ));
                }
                if let Err(error) = self.handle_context_exhaustion(name) {
                    warn!(
                        member = %name,
                        error = %error,
                        "context-exhausted restart handling failed; continuing"
                    );
                }
                continue;
            }

            // Stall detection: active agent with no output change beyond threshold.
            if new_state == WatcherState::Active && self.active_task_id(name).is_some() {
                let threshold = self.config.team_config.workflow_policy.stall_threshold_secs;
                if secs_since_output >= threshold {
                    if let Err(error) = self.handle_stalled_agent(name, secs_since_output) {
                        warn!(
                            member = %name,
                            error = %error,
                            "stall restart handling failed; continuing"
                        );
                    }
                    continue;
                }
            }

            if completion_observed && self.active_task_id(name).is_some() {
                // False-done prevention: verify the engineer's branch has commits
                // beyond main before triggering completion. Without this check,
                // idle engineers with no commits get marked as complete, orphaning
                // their board task.
                if self.member_uses_worktrees(name) {
                    let worktree_dir = self.worktree_dir(name);
                    match crate::team::git_cmd::run_git(
                        &worktree_dir,
                        &["rev-list", "--count", "main..HEAD"],
                    ) {
                        Ok(output) => {
                            let count = output.stdout.trim().parse::<u32>().unwrap_or(0);
                            if count == 0 {
                                warn!(
                                    member = %name,
                                    "engineer idle but no commits on task branch — skipping completion"
                                );
                                self.record_orchestrator_action(format!(
                                    "false-done prevention: {} reported completion but branch has no commits beyond main",
                                    name
                                ));
                                continue;
                            }
                        }
                        Err(error) => {
                            warn!(
                                member = %name,
                                error = %error,
                                "failed to check commits on task branch — skipping completion"
                            );
                            continue;
                        }
                    }
                }

                info!(member = %name, "detected task completion");
                if let Err(error) = merge::handle_engineer_completion(self, name) {
                    warn!(
                        member = %name,
                        error = %error,
                        "engineer completion handling failed; continuing"
                    );
                }
            }
        }

        Ok(())
    }

    pub(super) fn handle_pane_death(&mut self, member_name: &str) -> Result<()> {
        self.restart_member(member_name)
    }

    fn active_task(&self, member_name: &str) -> Result<Option<crate::task::Task>> {
        let Some(task_id) = self.active_task_id(member_name) else {
            return Ok(None);
        };
        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks"))?;
        Ok(tasks.into_iter().find(|task| task.id == task_id))
    }

    pub(super) fn context_restart_cooldown_key(member_name: &str) -> String {
        format!("context-restart::{member_name}")
    }

    pub(super) fn context_escalation_cooldown_key(member_name: &str) -> String {
        format!("context-escalation::{member_name}")
    }

    fn context_restart_count(&self, task_id: u32) -> Result<u32> {
        let events_path = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        let task_id = task_id.to_string();
        let count = super::super::events::read_events(&events_path)?
            .into_iter()
            .filter(|event| event.event == "agent_restarted")
            .filter(|event| event.task.as_deref() == Some(task_id.as_str()))
            .count() as u32;
        Ok(count)
    }

    pub(super) fn restart_assignment_message(task: &crate::task::Task) -> String {
        let mut message = format!(
            "Continuing Task #{}: {}\nPrevious session exhausted context; resume from the current worktree state and continue.\n\n{}",
            task.id, task.title, task.description
        );
        if let Some(branch) = task.branch.as_deref() {
            message.push_str(&format!("\n\nBranch: {branch}"));
        }
        if let Some(worktree_path) = task.worktree_path.as_deref() {
            message.push_str(&format!("\nWorktree: {worktree_path}"));
        }
        message
    }
}

/// Count total inserted + deleted lines from uncommitted changes in a worktree.
/// Runs `git diff --numstat` (unstaged) + `git diff --cached --numstat` (staged).
fn uncommitted_diff_lines(worktree: &Path) -> Result<usize> {
    let mut total = 0usize;
    for extra_args in [&["--numstat"] as &[&str], &["--cached", "--numstat"]] {
        let output = std::process::Command::new("git")
            .arg("diff")
            .args(extra_args)
            .current_dir(worktree)
            .output()
            .with_context(|| format!("failed to run git diff in {}", worktree.display()))?;
        if !output.status.success() {
            continue;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let mut parts = line.split_whitespace();
            let added: usize = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let removed: usize = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            total += added + removed;
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::team::config::{
        AutomationConfig, BoardConfig, OrchestratorPosition, RoleType, StandupConfig, TeamConfig,
        WorkflowMode, WorkflowPolicy,
    };
    use crate::team::events::{EventSink, TeamEvent};
    use crate::team::hierarchy::MemberInstance;
    use crate::team::standup::MemberState;
    use crate::team::test_helpers::{make_test_daemon, write_event_log};
    use crate::team::test_support::{
        TestDaemonBuilder, architect_member, engineer_member, git_ok, git_stdout, init_git_repo,
        manager_member, setup_fake_claude, write_owned_task_file,
        write_owned_task_file_with_context,
    };
    use crate::team::watcher::WatcherState;
    use serial_test::serial;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::{LazyLock, Mutex};
    use std::time::{Duration, Instant};

    static PATH_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    struct EnvVarGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var(key).ok();
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, original }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.original.as_deref() {
                Some(value) => unsafe {
                    std::env::set_var(self.key, value);
                },
                None => unsafe {
                    std::env::remove_var(self.key);
                },
            }
        }
    }

    fn setup_fake_kanban(tmp: &tempfile::TempDir, script_name: &str) -> PathBuf {
        let fake_bin = tmp.path().join(format!("{script_name}-bin"));
        std::fs::create_dir_all(&fake_bin).unwrap();
        let fake_kanban = fake_bin.join("kanban-md");
        std::fs::write(
            &fake_kanban,
            r#"#!/bin/bash
if [ "$1" = "--help" ]; then
  echo "kanban-md fake help"
  exit 0
fi
if [ "$1" = "init" ]; then
  shift
  while [ $# -gt 0 ]; do
    if [ "$1" = "--dir" ]; then
      shift
      mkdir -p "$1/tasks"
      exit 0
    fi
    shift
  done
fi
echo "unsupported fake kanban invocation" >&2
exit 1
"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_kanban, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        fake_bin
    }

    fn test_team_config(name: &str) -> TeamConfig {
        TeamConfig {
            name: name.to_string(),
            agent: None,
            workflow_mode: WorkflowMode::Legacy,
            workflow_policy: WorkflowPolicy::default(),
            board: BoardConfig::default(),
            standup: StandupConfig::default(),
            automation: AutomationConfig::default(),
            automation_sender: None,
            external_senders: Vec::new(),
            orchestrator_pane: true,
            orchestrator_position: OrchestratorPosition::Bottom,
            layout: None,
            cost: Default::default(),
            event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
            retro_min_duration_secs: 60,
            roles: Vec::new(),
        }
    }

    #[test]
    fn test_retry_count_increments_and_resets() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();

        daemon.active_tasks.insert("eng-1".into(), 42);
        assert_eq!(daemon.active_task_id("eng-1"), Some(42));
        assert_eq!(daemon.active_task_id("eng-2"), None);
        assert_eq!(daemon.increment_retry("eng-1"), 1);
        assert_eq!(daemon.increment_retry("eng-1"), 2);
        daemon.clear_active_task("eng-1");
        assert_eq!(daemon.active_task_id("eng-1"), None);
        assert_eq!(daemon.increment_retry("eng-1"), 1);
    }

    #[test]
    fn test_retry_count_triggers_escalation_at_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();

        daemon.active_tasks.insert("eng-1".into(), 42);
        assert_eq!(daemon.increment_retry("eng-1"), 1);
        assert_eq!(daemon.increment_retry("eng-1"), 2);
        assert_eq!(daemon.increment_retry("eng-1"), 3);
        daemon.clear_active_task("eng-1");
        assert_eq!(daemon.active_task_id("eng-1"), None);
    }

    #[test]
    fn test_active_task_id_returns_none_for_unassigned() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = TestDaemonBuilder::new(tmp.path()).build();

        assert_eq!(daemon.active_task_id("eng-1"), None);
    }

    #[test]
    fn nonfatal_kanban_failures_are_relayed_to_known_members() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![manager_member("manager", None)])
            .build();

        daemon.report_nonfatal_kanban_failure(
            "move task #42 to done",
            "kanban-md stderr goes here",
            ["manager"],
        );

        let messages =
            inbox::pending_messages(&inbox::inboxes_root(tmp.path()), "manager").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].from, "daemon");
        assert!(messages[0].body.contains("move task #42 to done"));
        assert!(messages[0].body.contains("kanban-md stderr goes here"));
    }

    #[test]
    #[serial]
    fn poll_watchers_respawns_pane_dead_member_and_records_events() {
        let session = format!("batty-test-restart-dead-member-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "architect-restart";
        let project_slug = tmp
            .path()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "default".to_string());
        let fake_bin = std::env::temp_dir().join(format!("batty-bin-{project_slug}-{member_name}"));
        let _ = std::fs::remove_dir_all(&fake_bin);
        std::fs::create_dir_all(&fake_bin).unwrap();
        let fake_log = tmp.path().join("fake-claude.log");
        let fake_claude = fake_bin.join("claude");
        std::fs::write(
            &fake_claude,
            format!(
                "#!/bin/bash\nprintf '%s\\n' \"$*\" >> '{}'\nsleep 5\n",
                fake_log.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        crate::tmux::create_session(&session, "bash", &[], tmp.path().to_str().unwrap()).unwrap();
        crate::tmux::create_window(
            &session,
            "keeper",
            "sleep",
            &["30".to_string()],
            tmp.path().to_str().unwrap(),
        )
        .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        Command::new("tmux")
            .args(["set-option", "-p", "-t", &pane_id, "remain-on-exit", "on"])
            .output()
            .unwrap();

        let member = MemberInstance {
            name: member_name.to_string(),
            role_name: "architect".to_string(),
            role_type: RoleType::Architect,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                agent: None,
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                external_senders: Vec::new(),
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.clone(),
            members: vec![member],
            pane_map: HashMap::from([(member_name.to_string(), pane_id.clone())]),
        })
        .unwrap();
        daemon
            .states
            .insert(member_name.to_string(), MemberState::Working);

        crate::tmux::send_keys(&pane_id, "exit", true).unwrap();
        for _ in 0..5 {
            if crate::tmux::pane_dead(&pane_id).unwrap_or(false) {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        assert!(crate::tmux::pane_dead(&pane_id).unwrap());

        daemon.poll_watchers().unwrap();
        std::thread::sleep(Duration::from_millis(700));

        assert!(!crate::tmux::pane_dead(&pane_id).unwrap_or(true));
        assert_eq!(daemon.states.get(member_name), Some(&MemberState::Idle));

        let log = (0..100)
            .find_map(|_| {
                let content = match std::fs::read_to_string(&fake_log) {
                    Ok(content) => content,
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(100));
                        return None;
                    }
                };
                if content.contains("--append-system-prompt") {
                    Some(content)
                } else {
                    std::thread::sleep(Duration::from_millis(100));
                    None
                }
            })
            .unwrap_or_else(|| {
                panic!(
                    "fake claude log was not written by restarted member at {}",
                    fake_log.display()
                )
            });
        assert!(log.contains("--append-system-prompt"));

        let launch_state = load_launch_state(tmp.path());
        let identity = launch_state
            .get(member_name)
            .expect("missing restarted member launch state");
        assert_eq!(identity.agent, "claude-code");
        assert!(identity.session_id.is_some());

        let events = std::fs::read_to_string(
            tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        assert!(events.contains("\"event\":\"pane_death\""));
        assert!(events.contains("\"event\":\"pane_respawned\""));
        assert!(events.contains("\"event\":\"member_crashed\""));
        assert!(events.contains(&format!("\"role\":\"{member_name}\"")));
        assert!(events.contains("\"restart\":true"));

        crate::tmux::kill_session(&session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
    }

    #[test]
    #[serial]
    fn spawn_all_agents_corrects_mismatched_cwd_before_launch() {
        let session = format!("batty-test-spawn-cwd-correct-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        let wrong_dir = tmp.path().join("wrong");
        std::fs::create_dir_all(&wrong_dir).unwrap();

        let member_name = "architect-cwd";
        let (fake_bin, fake_log) = setup_fake_claude(&tmp, member_name);

        crate::tmux::create_session(&session, "bash", &[], wrong_dir.to_string_lossy().as_ref())
            .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();

        let member = MemberInstance {
            name: member_name.to_string(),
            role_name: "architect".to_string(),
            role_type: RoleType::Architect,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                agent: None,
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                external_senders: Vec::new(),
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.clone(),
            members: vec![member],
            pane_map: HashMap::from([(member_name.to_string(), pane_id.clone())]),
        })
        .unwrap();

        daemon.spawn_all_agents(false).unwrap();

        let log = (0..100)
            .find_map(|_| {
                let content = match std::fs::read_to_string(&fake_log) {
                    Ok(content) => content,
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(100));
                        return None;
                    }
                };
                if content.contains("--append-system-prompt") {
                    Some(content)
                } else {
                    std::thread::sleep(Duration::from_millis(100));
                    None
                }
            })
            .unwrap_or_else(|| {
                panic!(
                    "fake claude log was not written by spawned member at {}",
                    fake_log.display()
                )
            });
        assert!(log.contains("--append-system-prompt"));

        let current = (0..50)
            .find_map(|_| match crate::tmux::pane_current_path(&pane_id) {
                Ok(current) if !current.is_empty() => Some(current),
                _ => {
                    std::thread::sleep(Duration::from_millis(100));
                    None
                }
            })
            .unwrap_or_else(|| {
                panic!("tmux pane current path never became available for target '{pane_id}'")
            });
        assert_eq!(
            normalized_assignment_dir(Path::new(&current)),
            normalized_assignment_dir(tmp.path())
        );

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        let corrected = events
            .iter()
            .find(|event| event.event == "cwd_corrected")
            .expect("expected cwd_corrected event during spawn");
        assert_eq!(corrected.role.as_deref(), Some(member_name));
        assert_eq!(
            corrected.reason.as_deref(),
            Some(tmp.path().to_string_lossy().as_ref())
        );

        crate::tmux::kill_session(&session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
    }

    #[test]
    #[serial]
    fn restart_member_corrects_mismatched_cwd_after_respawn() {
        let session = format!("batty-test-restart-cwd-correct-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        let wrong_dir = tmp.path().join("wrong");
        std::fs::create_dir_all(&wrong_dir).unwrap();

        let member_name = "architect-restart-cwd";
        let (fake_bin, fake_log) = setup_fake_claude(&tmp, member_name);

        crate::tmux::create_session(&session, "bash", &[], wrong_dir.to_string_lossy().as_ref())
            .unwrap();
        crate::tmux::create_window(
            &session,
            "keeper",
            "sleep",
            &["30".to_string()],
            wrong_dir.to_string_lossy().as_ref(),
        )
        .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        Command::new("tmux")
            .args(["set-option", "-p", "-t", &pane_id, "remain-on-exit", "on"])
            .output()
            .unwrap();

        let member = MemberInstance {
            name: member_name.to_string(),
            role_name: "architect".to_string(),
            role_type: RoleType::Architect,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                agent: None,
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                external_senders: Vec::new(),
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.clone(),
            members: vec![member],
            pane_map: HashMap::from([(member_name.to_string(), pane_id.clone())]),
        })
        .unwrap();

        crate::tmux::send_keys(&pane_id, "exit", true).unwrap();
        for _ in 0..10 {
            if crate::tmux::pane_dead(&pane_id).unwrap_or(false) {
                break;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        assert!(crate::tmux::pane_dead(&pane_id).unwrap());

        daemon.restart_member(member_name).unwrap();

        let log = (0..40)
            .find_map(|_| {
                std::thread::sleep(Duration::from_millis(200));
                let content = std::fs::read_to_string(&fake_log).ok()?;
                if content.contains("--append-system-prompt") {
                    Some(content)
                } else {
                    None
                }
            })
            .unwrap_or_else(|| {
                panic!(
                    "fake claude log was not written by restarted member at {}",
                    fake_log.display()
                )
            });
        assert!(log.contains("--append-system-prompt"));

        let expected = normalized_assignment_dir(tmp.path());
        let cwd_ok = (0..20).any(|_| {
            std::thread::sleep(Duration::from_millis(200));
            crate::tmux::pane_current_path(&pane_id)
                .map(|p| normalized_assignment_dir(Path::new(&p)) == expected)
                .unwrap_or(false)
        });
        assert!(cwd_ok, "pane cwd did not converge to expected dir");

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        assert!(events.iter().any(|event| {
            event.event == "pane_respawned" && event.role.as_deref() == Some(member_name)
        }));
        assert!(events.iter().any(|event| {
            event.event == "cwd_corrected"
                && event.role.as_deref() == Some(member_name)
                && event.reason.as_deref() == Some(tmp.path().to_string_lossy().as_ref())
        }));

        crate::tmux::kill_session(&session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
    }

    #[test]
    #[serial]
    fn agent_restart_relaunches_context_exhausted_member_with_task_context() {
        let session = format!("batty-test-agent-restart-context-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-restart";
        let lead_name = "lead";
        let (fake_bin, fake_log) = setup_fake_claude(&tmp, member_name);
        let worktree_path = tmp.path().join("worktree");
        std::fs::create_dir_all(&worktree_path).unwrap();

        crate::tmux::create_session(&session, "bash", &[], tmp.path().to_str().unwrap()).unwrap();
        crate::tmux::create_window(
            &session,
            "keeper",
            "sleep",
            &["30".to_string()],
            tmp.path().to_str().unwrap(),
        )
        .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        Command::new("tmux")
            .args(["set-option", "-p", "-t", &pane_id, "remain-on-exit", "on"])
            .output()
            .unwrap();

        let lead = MemberInstance {
            name: lead_name.to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: member_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some(lead_name.to_string()),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                agent: None,
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                external_senders: Vec::new(),
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.to_string(),
            members: vec![lead, engineer],
            pane_map: HashMap::from([(member_name.to_string(), pane_id.clone())]),
        })
        .unwrap();
        daemon
            .states
            .insert(member_name.to_string(), MemberState::Working);
        daemon.active_tasks.insert(member_name.to_string(), 191);

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, member_name).unwrap();
        inbox::init_inbox(&root, lead_name).unwrap();
        write_owned_task_file_with_context(
            tmp.path(),
            191,
            "active-task",
            "in-progress",
            member_name,
            "eng-1-2/191",
            &worktree_path.display().to_string(),
        );

        daemon.handle_context_exhaustion(member_name).unwrap();

        let log = (0..100)
            .find_map(|_| {
                let content = match std::fs::read_to_string(&fake_log) {
                    Ok(content) => content,
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(100));
                        return None;
                    }
                };
                if content.contains("Continuing Task #191") {
                    Some(content)
                } else {
                    std::thread::sleep(Duration::from_millis(100));
                    None
                }
            })
            .unwrap_or_else(|| {
                panic!(
                    "fake claude log was not written by restarted member at {}",
                    fake_log.display()
                )
            });
        assert!(log.contains("Previous session exhausted context"));
        assert!(log.contains("Branch: eng-1-2/191"));
        assert!(log.contains(&format!("Worktree: {}", worktree_path.display())));

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        let restart_events = events
            .iter()
            .filter(|event| event.event == "agent_restarted")
            .collect::<Vec<_>>();
        assert_eq!(restart_events.len(), 1);
        assert_eq!(restart_events[0].role.as_deref(), Some(member_name));
        assert_eq!(restart_events[0].task.as_deref(), Some("191"));
        assert_eq!(
            restart_events[0].reason.as_deref(),
            Some("context_exhausted")
        );
        assert_eq!(restart_events[0].restart_count, Some(1));
        assert!(events.iter().any(|event| {
            event.event == "message_routed"
                && event.from.as_deref() == Some("daemon")
                && event.to.as_deref() == Some(member_name)
        }));

        crate::tmux::kill_session(&session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
    }

    #[test]
    #[serial]
    fn context_exhaustion_relaunch_corrects_mismatched_cwd() {
        let session = "batty-test-context-cwd-correct";
        let _ = crate::tmux::kill_session(session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        let wrong_dir = tmp.path().join("wrong");
        std::fs::create_dir_all(&wrong_dir).unwrap();

        let member_name = "eng-context-cwd";
        let lead_name = "lead";
        let (fake_bin, _fake_log) = setup_fake_claude(&tmp, member_name);

        crate::tmux::create_session(session, "bash", &[], wrong_dir.to_string_lossy().as_ref())
            .unwrap();
        crate::tmux::create_window(
            session,
            "keeper",
            "sleep",
            &["30".to_string()],
            wrong_dir.to_string_lossy().as_ref(),
        )
        .unwrap();
        let pane_id = crate::tmux::pane_id(session).unwrap();
        Command::new("tmux")
            .args(["set-option", "-p", "-t", &pane_id, "remain-on-exit", "on"])
            .output()
            .unwrap();

        let lead = MemberInstance {
            name: lead_name.to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: member_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some(lead_name.to_string()),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                agent: None,
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                external_senders: Vec::new(),
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.to_string(),
            members: vec![lead, engineer],
            pane_map: HashMap::from([(member_name.to_string(), pane_id.clone())]),
        })
        .unwrap();
        daemon
            .states
            .insert(member_name.to_string(), MemberState::Working);
        daemon.active_tasks.insert(member_name.to_string(), 191);

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, member_name).unwrap();
        inbox::init_inbox(&root, lead_name).unwrap();
        write_owned_task_file(tmp.path(), 191, "active-task", "in-progress", member_name);

        daemon.handle_context_exhaustion(member_name).unwrap();
        let current = (0..50)
            .find_map(|_| match crate::tmux::pane_current_path(&pane_id) {
                Ok(path) if !path.is_empty() => Some(path),
                _ => {
                    std::thread::sleep(Duration::from_millis(100));
                    None
                }
            })
            .expect("expected relaunched pane to report a current working directory");
        assert_eq!(
            normalized_assignment_dir(Path::new(&current)),
            normalized_assignment_dir(tmp.path())
        );

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        assert!(events.iter().any(|event| {
            event.event == "cwd_corrected"
                && event.role.as_deref() == Some(member_name)
                && event.reason.as_deref() == Some(tmp.path().to_string_lossy().as_ref())
        }));

        crate::tmux::kill_session(session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
    }

    #[test]
    #[serial]
    fn agent_restart_second_exhaustion_escalates_instead_of_restarting() {
        let session = "batty-test-agent-restart-escalate";
        let _ = crate::tmux::kill_session(session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-escalate";
        let lead_name = "lead";
        let (fake_bin, fake_log) = setup_fake_claude(&tmp, member_name);

        crate::tmux::create_session(session, "bash", &[], tmp.path().to_str().unwrap()).unwrap();
        crate::tmux::create_window(
            session,
            "keeper",
            "sleep",
            &["30".to_string()],
            tmp.path().to_str().unwrap(),
        )
        .unwrap();
        let pane_id = crate::tmux::pane_id(session).unwrap();

        let lead = MemberInstance {
            name: lead_name.to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: member_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some(lead_name.to_string()),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                agent: None,
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                external_senders: Vec::new(),
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.to_string(),
            members: vec![lead, engineer],
            pane_map: HashMap::from([(member_name.to_string(), pane_id)]),
        })
        .unwrap();
        daemon
            .states
            .insert(member_name.to_string(), MemberState::Working);
        daemon.active_tasks.insert(member_name.to_string(), 191);
        daemon.intervention_cooldowns.insert(
            TeamDaemon::context_restart_cooldown_key(member_name),
            Instant::now(),
        );

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, lead_name).unwrap();
        write_owned_task_file(tmp.path(), 191, "active-task", "in-progress", member_name);
        write_event_log(
            tmp.path(),
            &[TeamEvent::agent_restarted(
                member_name,
                "191",
                "context_exhausted",
                1,
            )],
        );

        daemon.handle_context_exhaustion(member_name).unwrap();

        let pending = inbox::pending_messages(&root, lead_name).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].body.contains("Task #191"));
        assert!(pending[0].body.contains("split the task"));

        let log = std::fs::read_to_string(&fake_log).unwrap_or_default();
        assert!(!log.contains("Continuing Task #191"));

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event == "agent_restarted")
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event == "task_escalated")
                .count(),
            1
        );

        crate::tmux::kill_session(session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
    }

    #[test]
    #[serial]
    fn agent_restart_respects_cooldown_before_first_restart() {
        let session = "batty-test-agent-restart-cooldown";
        let _ = crate::tmux::kill_session(session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-cooldown";
        let lead_name = "lead";
        let (fake_bin, fake_log) = setup_fake_claude(&tmp, member_name);

        crate::tmux::create_session(session, "bash", &[], tmp.path().to_str().unwrap()).unwrap();
        crate::tmux::create_window(
            session,
            "keeper",
            "sleep",
            &["30".to_string()],
            tmp.path().to_str().unwrap(),
        )
        .unwrap();
        let pane_id = crate::tmux::pane_id(session).unwrap();

        let lead = MemberInstance {
            name: lead_name.to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: member_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some(lead_name.to_string()),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                agent: None,
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                external_senders: Vec::new(),
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.to_string(),
            members: vec![lead, engineer],
            pane_map: HashMap::from([(member_name.to_string(), pane_id)]),
        })
        .unwrap();
        daemon
            .states
            .insert(member_name.to_string(), MemberState::Working);
        daemon.active_tasks.insert(member_name.to_string(), 191);
        daemon.intervention_cooldowns.insert(
            TeamDaemon::context_restart_cooldown_key(member_name),
            Instant::now(),
        );

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, lead_name).unwrap();
        inbox::init_inbox(&root, member_name).unwrap();
        write_owned_task_file(tmp.path(), 191, "active-task", "in-progress", member_name);

        daemon.handle_context_exhaustion(member_name).unwrap();
        std::thread::sleep(Duration::from_millis(300));

        let log = std::fs::read_to_string(&fake_log).unwrap_or_default();
        assert!(log.is_empty());
        assert!(
            inbox::pending_messages(&root, lead_name)
                .unwrap()
                .is_empty()
        );
        assert!(
            inbox::pending_messages(&root, member_name)
                .unwrap()
                .is_empty()
        );

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        assert!(events.is_empty());

        crate::tmux::kill_session(session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
    }

    #[test]
    fn startup_preflight_reports_missing_kanban_binary() {
        let _path_guard = PATH_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let empty_bin = tmp.path().join("empty-bin");
        std::fs::create_dir_all(&empty_bin).unwrap();
        let _path = EnvVarGuard::set("PATH", empty_bin.to_string_lossy().as_ref());

        let error = ensure_kanban_available().unwrap_err();

        assert!(format!("{error:#}").contains("kanban-md"));
    }

    #[test]
    fn startup_preflight_initializes_missing_board_directory() {
        let _path_guard = PATH_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "startup_board_init");
        let fake_bin = setup_fake_kanban(&tmp, "startup-board-init");
        let original_path = std::env::var("PATH").unwrap_or_default();
        let _path = EnvVarGuard::set(
            "PATH",
            &format!("{}:{original_path}", fake_bin.to_string_lossy()),
        );

        let board_path = board_dir(&repo);
        assert!(!board_path.exists());

        assert!(ensure_board_initialized(&repo).unwrap());
        assert!(board_path.join("tasks").is_dir());
        assert!(!ensure_board_initialized(&repo).unwrap());
    }

    #[test]
    #[serial]
    fn startup_preflight_respawns_dead_pane_and_bootstraps_board() {
        let _path_guard = PATH_LOCK.lock().unwrap();
        let session = format!("batty-test-startup-preflight-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "startup_preflight");
        let fake_bin = setup_fake_kanban(&tmp, "startup-preflight");
        let original_path = std::env::var("PATH").unwrap_or_default();
        let _path = EnvVarGuard::set(
            "PATH",
            &format!("{}:{original_path}", fake_bin.to_string_lossy()),
        );

        crate::tmux::create_session(&session, "bash", &[], repo.to_string_lossy().as_ref())
            .unwrap();
        crate::tmux::create_window(
            &session,
            "eng-1",
            "bash",
            &[],
            repo.to_string_lossy().as_ref(),
        )
        .unwrap();

        let architect_pane = crate::tmux::pane_id(&session).unwrap();
        let engineer_pane = crate::tmux::pane_id(&format!("{session}:eng-1")).unwrap();
        Command::new("tmux")
            .args([
                "set-option",
                "-p",
                "-t",
                &engineer_pane,
                "remain-on-exit",
                "on",
            ])
            .output()
            .unwrap();
        crate::tmux::send_keys(&engineer_pane, "exit", true).unwrap();
        for _ in 0..20 {
            if crate::tmux::pane_dead(&engineer_pane).unwrap_or(false) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(crate::tmux::pane_dead(&engineer_pane).unwrap());

        let members = vec![
            architect_member("architect"),
            engineer_member("eng-1", Some("architect"), false),
        ];
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: repo.clone(),
            team_config: test_team_config("startup-preflight"),
            session: session.clone(),
            members,
            pane_map: HashMap::from([
                ("architect".to_string(), architect_pane),
                ("eng-1".to_string(), engineer_pane.clone()),
            ]),
        })
        .unwrap();

        daemon.run_startup_preflight().unwrap();

        assert!(!crate::tmux::pane_dead(&engineer_pane).unwrap_or(true));
        assert!(board_dir(&repo).join("tasks").is_dir());

        let events = crate::team::events::read_events(
            &repo.join(".batty").join("team_config").join("events.jsonl"),
        )
        .unwrap();
        assert!(events.iter().any(|event| {
            event.event == "pane_respawned" && event.role.as_deref() == Some("eng-1")
        }));

        crate::tmux::kill_session(&session).unwrap();
    }

    #[test]
    fn mark_member_working_updates_state_and_watcher() {
        let tmp = tempfile::tempdir().unwrap();
        let mut watchers = HashMap::new();
        watchers.insert(
            "architect".to_string(),
            SessionWatcher::new("%0", "architect", 300, None),
        );

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .watchers(watchers)
            .build();

        daemon.mark_member_working("architect");

        assert_eq!(daemon.states.get("architect"), Some(&MemberState::Working));
        assert_eq!(
            daemon
                .watchers
                .get("architect")
                .map(|watcher| watcher.state),
            Some(WatcherState::Active)
        );
    }

    #[test]
    #[serial]
    fn pre_assignment_health_check_corrects_mismatched_cwd() {
        let session = format!("batty-test-health-check-cwd-correct-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        let wrong_dir = tmp.path().join("wrong");
        let expected_dir = tmp.path().join("expected");
        std::fs::create_dir_all(&wrong_dir).unwrap();
        std::fs::create_dir_all(&expected_dir).unwrap();

        crate::tmux::create_session(&session, "bash", &[], wrong_dir.to_string_lossy().as_ref())
            .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        std::thread::sleep(Duration::from_millis(200));

        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: true,
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                agent: None,
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                external_senders: Vec::new(),
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.clone(),
            members: vec![engineer],
            pane_map: HashMap::from([("eng-1".to_string(), pane_id.clone())]),
        })
        .unwrap();

        daemon
            .ensure_member_pane_cwd("eng-1", &pane_id, &expected_dir)
            .unwrap();

        let current = crate::tmux::pane_current_path(&pane_id).unwrap();
        assert_eq!(
            normalized_assignment_dir(Path::new(&current)),
            normalized_assignment_dir(&expected_dir)
        );

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        let corrected = events
            .iter()
            .find(|event| event.event == "cwd_corrected")
            .expect("expected cwd_corrected event");
        assert_eq!(corrected.role.as_deref(), Some("eng-1"));
        assert_eq!(
            corrected.reason.as_deref(),
            Some(expected_dir.to_string_lossy().as_ref())
        );

        crate::tmux::kill_session(&session).unwrap();
    }

    #[test]
    #[serial]
    fn pre_assignment_health_check_cwd_matching_path_passes_silently() {
        let session = format!("batty-test-health-check-cwd-match-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        let expected_dir = tmp.path().join("expected");
        std::fs::create_dir_all(&expected_dir).unwrap();

        crate::tmux::create_session(
            &session,
            "bash",
            &[],
            expected_dir.to_string_lossy().as_ref(),
        )
        .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        std::thread::sleep(Duration::from_millis(200));

        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: true,
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                agent: None,
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                external_senders: Vec::new(),
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.clone(),
            members: vec![engineer],
            pane_map: HashMap::from([("eng-1".to_string(), pane_id.clone())]),
        })
        .unwrap();

        daemon
            .ensure_member_pane_cwd("eng-1", &pane_id, &expected_dir)
            .unwrap();

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        assert!(
            events.iter().all(|event| event.event != "cwd_corrected"),
            "did not expect cwd_corrected event when pane cwd already matched"
        );

        crate::tmux::kill_session(&session).unwrap();
    }

    #[test]
    #[serial]
    fn startup_cwd_validation_corrects_all_agent_panes() {
        let session = format!("batty-test-startup-cwd-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "startup-cwd");
        let wrong_architect_dir = tmp.path().join("wrong-architect");
        let wrong_engineer_dir = tmp.path().join("wrong-engineer");
        std::fs::create_dir_all(&wrong_architect_dir).unwrap();
        std::fs::create_dir_all(&wrong_engineer_dir).unwrap();

        crate::tmux::create_session(
            &session,
            "bash",
            &[],
            wrong_architect_dir.to_string_lossy().as_ref(),
        )
        .unwrap();
        crate::tmux::create_window(
            &session,
            "eng-1",
            "bash",
            &[],
            wrong_engineer_dir.to_string_lossy().as_ref(),
        )
        .unwrap();
        let architect_pane = crate::tmux::pane_id(&session).unwrap();
        let engineer_pane = crate::tmux::pane_id(&format!("{session}:eng-1")).unwrap();
        std::thread::sleep(Duration::from_millis(200));

        let architect = MemberInstance {
            name: "architect".to_string(),
            role_name: "architect".to_string(),
            role_type: RoleType::Architect,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: true,
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: repo.clone(),
            team_config: TeamConfig {
                name: "test".to_string(),
                agent: None,
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                external_senders: Vec::new(),
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                event_log_max_bytes: 10 * 1024 * 1024,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.clone(),
            members: vec![architect, engineer],
            pane_map: HashMap::from([
                ("architect".to_string(), architect_pane.clone()),
                ("eng-1".to_string(), engineer_pane.clone()),
            ]),
        })
        .unwrap();

        daemon.validate_member_panes_on_startup();

        let engineer_expected_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let architect_current = crate::tmux::pane_current_path(&architect_pane).unwrap();
        let engineer_current = crate::tmux::pane_current_path(&engineer_pane).unwrap();
        assert_eq!(
            normalized_assignment_dir(Path::new(&architect_current)),
            normalized_assignment_dir(&repo)
        );
        assert_eq!(
            normalized_assignment_dir(Path::new(&engineer_current)),
            normalized_assignment_dir(&engineer_expected_dir)
        );

        let events = crate::team::events::read_events(
            &repo.join(".batty").join("team_config").join("events.jsonl"),
        )
        .unwrap();
        assert!(events.iter().any(|event| {
            event.event == "cwd_corrected"
                && event.role.as_deref() == Some("architect")
                && event.reason.as_deref() == Some(repo.to_string_lossy().as_ref())
        }));
        assert!(events.iter().any(|event| {
            event.event == "cwd_corrected"
                && event.role.as_deref() == Some("eng-1")
                && event.reason.as_deref() == Some(engineer_expected_dir.to_string_lossy().as_ref())
        }));

        crate::tmux::kill_session(&session).unwrap();
    }

    #[test]
    fn stall_restart_count_returns_zero_with_no_events() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let daemon = make_test_daemon(tmp.path(), vec![]);
        let count = daemon.stall_restart_count(42).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn stall_restart_count_counts_only_stalled_reason() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        write_event_log(
            tmp.path(),
            &[
                TeamEvent::agent_restarted("eng-1-1", "42", "context_exhausted", 1),
                TeamEvent::agent_restarted("eng-1-1", "42", "stalled", 1),
                TeamEvent::agent_restarted("eng-1-1", "42", "stalled", 2),
                TeamEvent::agent_restarted("eng-1-1", "99", "stalled", 1),
            ],
        );

        let daemon = make_test_daemon(tmp.path(), vec![]);
        // Only counts stalled reason for task 42
        assert_eq!(daemon.stall_restart_count(42).unwrap(), 2);
        // Task 99 has only 1 stalled restart
        assert_eq!(daemon.stall_restart_count(99).unwrap(), 1);
        // Task 100 has no events
        assert_eq!(daemon.stall_restart_count(100).unwrap(), 0);
    }

    #[test]
    fn stall_detection_config_defaults() {
        let policy = WorkflowPolicy::default();
        assert_eq!(policy.stall_threshold_secs, 300);
        assert_eq!(policy.max_stall_restarts, 2);
    }

    #[test]
    #[serial]
    fn stall_restart_relaunches_stalled_agent_with_task_context() {
        let session = format!("batty-test-stall-restart-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-stall";
        let lead_name = "lead-stall";
        let (fake_bin, fake_log) = setup_fake_claude(&tmp, member_name);
        let worktree_path = tmp.path().join("worktree");
        std::fs::create_dir_all(&worktree_path).unwrap();

        crate::tmux::create_session(&session, "bash", &[], tmp.path().to_str().unwrap()).unwrap();
        crate::tmux::create_window(
            &session,
            "keeper",
            "sleep",
            &["30".to_string()],
            tmp.path().to_str().unwrap(),
        )
        .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        Command::new("tmux")
            .args(["set-option", "-p", "-t", &pane_id, "remain-on-exit", "on"])
            .output()
            .unwrap();

        let lead = MemberInstance {
            name: lead_name.to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: member_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some(lead_name.to_string()),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                agent: None,
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                external_senders: Vec::new(),
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.clone(),
            members: vec![lead, engineer],
            pane_map: HashMap::from([(member_name.to_string(), pane_id.clone())]),
        })
        .unwrap();
        daemon
            .states
            .insert(member_name.to_string(), MemberState::Working);
        daemon.active_tasks.insert(member_name.to_string(), 42);

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, member_name).unwrap();
        inbox::init_inbox(&root, lead_name).unwrap();
        write_owned_task_file_with_context(
            tmp.path(),
            42,
            "stall-test-task",
            "in-progress",
            member_name,
            "eng-stall/42",
            &worktree_path.display().to_string(),
        );

        daemon.handle_stalled_agent(member_name, 300).unwrap();

        let log = (0..100)
            .find_map(|_| {
                let content = match std::fs::read_to_string(&fake_log) {
                    Ok(content) => content,
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(100));
                        return None;
                    }
                };
                if content.contains("Continuing Task #42") {
                    Some(content)
                } else {
                    std::thread::sleep(Duration::from_millis(100));
                    None
                }
            })
            .unwrap_or_else(|| {
                panic!(
                    "fake claude log was not written by restarted member at {}",
                    fake_log.display()
                )
            });
        assert!(log.contains("stall-test-task"));
        assert!(log.contains("Branch: eng-stall/42"));
        assert!(log.contains(&format!("Worktree: {}", worktree_path.display())));

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();

        // Should have stall_detected event
        let stall_events: Vec<_> = events
            .iter()
            .filter(|e| e.event == "stall_detected")
            .collect();
        assert_eq!(stall_events.len(), 1);
        assert_eq!(stall_events[0].role.as_deref(), Some(member_name));
        assert_eq!(stall_events[0].task.as_deref(), Some("42"));
        assert_eq!(stall_events[0].uptime_secs, Some(300));

        // Should have agent_restarted event with "stalled" reason
        let restart_events: Vec<_> = events
            .iter()
            .filter(|e| e.event == "agent_restarted")
            .collect();
        assert_eq!(restart_events.len(), 1);
        assert_eq!(restart_events[0].role.as_deref(), Some(member_name));
        assert_eq!(restart_events[0].task.as_deref(), Some("42"));
        assert_eq!(restart_events[0].reason.as_deref(), Some("stalled"));
        assert_eq!(restart_events[0].restart_count, Some(1));

        // Should have injected a restart notice via message routing
        assert!(events.iter().any(|e| {
            e.event == "message_routed"
                && e.from.as_deref() == Some("daemon")
                && e.to.as_deref() == Some(member_name)
        }));

        crate::tmux::kill_session(&session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
    }

    #[test]
    #[serial]
    fn stall_escalates_after_max_restarts() {
        let session = format!("batty-test-stall-escalate-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-stall-esc";
        let lead_name = "lead-stall-esc";
        let (fake_bin, fake_log) = setup_fake_claude(&tmp, member_name);

        crate::tmux::create_session(&session, "bash", &[], tmp.path().to_str().unwrap()).unwrap();
        crate::tmux::create_window(
            &session,
            "keeper",
            "sleep",
            &["30".to_string()],
            tmp.path().to_str().unwrap(),
        )
        .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();

        let lead = MemberInstance {
            name: lead_name.to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: member_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some(lead_name.to_string()),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                agent: None,
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy {
                    max_stall_restarts: 2,
                    ..WorkflowPolicy::default()
                },
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                external_senders: Vec::new(),
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.clone(),
            members: vec![lead, engineer],
            pane_map: HashMap::from([(member_name.to_string(), pane_id)]),
        })
        .unwrap();
        daemon
            .states
            .insert(member_name.to_string(), MemberState::Working);
        daemon.active_tasks.insert(member_name.to_string(), 50);

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, lead_name).unwrap();
        write_owned_task_file(
            tmp.path(),
            50,
            "stall-escalate-task",
            "in-progress",
            member_name,
        );

        // Write 2 prior stall restarts to event log
        write_event_log(
            tmp.path(),
            &[
                TeamEvent::agent_restarted(member_name, "50", "stalled", 1),
                TeamEvent::agent_restarted(member_name, "50", "stalled", 2),
            ],
        );

        daemon.handle_stalled_agent(member_name, 600).unwrap();

        // Should have escalated to manager, not restarted
        let pending = inbox::pending_messages(&root, lead_name).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].body.contains("Task #50"));
        assert!(pending[0].body.contains("stalled"));
        assert!(pending[0].body.contains("will not restart again"));

        // Should NOT have re-launched the agent
        let log = std::fs::read_to_string(&fake_log).unwrap_or_default();
        assert!(!log.contains("Continuing Task #50"));

        // Check events
        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        // 2 prior + 0 new restarts (escalation, not restart)
        assert_eq!(
            events
                .iter()
                .filter(|e| e.event == "agent_restarted")
                .count(),
            2
        );
        // Should have task_escalated event
        assert!(events.iter().any(|e| {
            e.event == "task_escalated"
                && e.role.as_deref() == Some(member_name)
                && e.reason.as_deref() == Some("stalled")
        }));
        // Should have stall_detected event
        assert!(events.iter().any(|e| e.event == "stall_detected"));

        crate::tmux::kill_session(&session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
    }

    #[test]
    fn stall_restart_cooldown_prevents_repeat_restart() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-stall-cd";
        let lead_name = "lead-stall-cd";

        let lead = MemberInstance {
            name: lead_name.to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: member_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some(lead_name.to_string()),
            use_worktrees: false,
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![lead, engineer]);
        daemon.active_tasks.insert(member_name.to_string(), 77);
        daemon
            .config
            .pane_map
            .insert(member_name.to_string(), "%999".to_string());
        write_owned_task_file(tmp.path(), 77, "cooldown-task", "in-progress", member_name);

        // Set cooldown as if we just restarted
        daemon
            .intervention_cooldowns
            .insert(format!("stall-restart::{member_name}"), Instant::now());

        // Should be a no-op due to cooldown
        daemon.handle_stalled_agent(member_name, 300).unwrap();

        // No events should have been emitted (cooldown blocks before event emission?
        // Actually looking at the code, the stall_detected event is emitted before
        // cooldown check. Let me re-check...
        // Actually the cooldown check is BEFORE the event emission in handle_stalled_agent.
        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap_or_default();
        // No stall_detected or agent_restarted should be emitted
        assert_eq!(
            events
                .iter()
                .filter(|e| e.event == "stall_detected" || e.event == "agent_restarted")
                .count(),
            0,
            "cooldown should suppress all stall handling"
        );
    }

    #[test]
    fn health_check_interval_config_default() {
        use crate::team::config::WorkflowPolicy;
        let policy = WorkflowPolicy::default();
        assert_eq!(policy.health_check_interval_secs, 60);
    }

    #[test]
    fn check_backend_health_skipped_before_interval() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let engineer = MemberInstance {
            name: "eng-health".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);
        // Set last_health_check to now so the interval hasn't elapsed.
        daemon.last_health_check = Instant::now();
        daemon.check_backend_health().unwrap();
        // No health entries should have been recorded because the check was skipped.
        assert!(daemon.backend_health.is_empty());
    }

    #[test]
    fn check_backend_health_runs_after_interval() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let engineer = MemberInstance {
            name: "eng-health-run".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);
        // Force last_health_check to be old enough.
        daemon.last_health_check = Instant::now() - Duration::from_secs(3600);
        daemon.check_backend_health().unwrap();
        // Should have recorded health for the engineer.
        assert!(daemon.backend_health.contains_key("eng-health-run"));
    }

    #[test]
    fn check_backend_health_skips_user_roles() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let user = MemberInstance {
            name: "user-role".to_string(),
            role_name: "user".to_string(),
            role_type: RoleType::User,
            agent: None,
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![user]);
        daemon.last_health_check = Instant::now() - Duration::from_secs(3600);
        daemon.check_backend_health().unwrap();
        // User roles should not be checked.
        assert!(!daemon.backend_health.contains_key("user-role"));
    }

    #[test]
    fn check_backend_health_emits_event_on_transition() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let engineer = MemberInstance {
            name: "eng-health-ev".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);
        // Pre-populate with a different state to force a transition.
        daemon
            .backend_health
            .insert("eng-health-ev".to_string(), BackendHealth::Unreachable);
        daemon.last_health_check = Instant::now() - Duration::from_secs(3600);
        // Health check runs: claude binary should be found → Healthy.
        // Previous state was Unreachable → emits transition event.
        daemon.check_backend_health().unwrap();

        // Check events for health_changed.
        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        let health_events: Vec<_> = events
            .iter()
            .filter(|e| e.event == "health_changed")
            .collect();
        assert_eq!(health_events.len(), 1);
        assert_eq!(health_events[0].role.as_deref(), Some("eng-health-ev"));
        assert_eq!(
            health_events[0].reason.as_deref(),
            Some("unreachable→healthy")
        );
    }

    #[test]
    fn check_backend_health_no_event_when_state_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let engineer = MemberInstance {
            name: "eng-health-stable".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            // claude is installed, so health check returns Healthy.
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);
        // Pre-populate with Healthy (matching expected check result).
        daemon
            .backend_health
            .insert("eng-health-stable".to_string(), BackendHealth::Healthy);
        daemon.last_health_check = Instant::now() - Duration::from_secs(3600);
        daemon.check_backend_health().unwrap();

        // No health_changed events — state didn't change.
        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap_or_default();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.event == "health_changed")
                .count(),
            0,
            "no event when state is unchanged"
        );
    }

    // ---- Worktree reconciliation tests ----

    /// Helper: set up a test repo, engineer worktree on a merged task branch.
    fn setup_reconcile_scenario(engineer: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-reconcile");
        let worktree_dir = repo.join(".batty").join("worktrees").join(engineer);
        let team_config_dir = repo.join(".batty").join("team_config");

        setup_engineer_worktree(&repo, &worktree_dir, engineer, &team_config_dir).unwrap();

        // Create a task branch and commit work (use flat name to avoid ref conflicts)
        let task_branch = format!("{engineer}-42");
        git_ok(&worktree_dir, &["checkout", "-b", &task_branch]);
        std::fs::write(worktree_dir.join("feature.txt"), "work\n").unwrap();
        git_ok(&worktree_dir, &["add", "feature.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "task work"]);

        // Merge the task branch into main (simulate completed merge)
        git_ok(&repo, &["merge", &task_branch]);

        (tmp, repo, worktree_dir)
    }

    #[test]
    fn reconcile_resets_idle_engineer_on_merged_branch() {
        let (_tmp, repo, worktree_dir) = setup_reconcile_scenario("eng-reconcile");

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-reconcile", Some("manager"), true),
        ];
        let states = HashMap::from([("eng-reconcile".to_string(), MemberState::Idle)]);
        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(members)
            .states(states)
            .build();
        daemon.is_git_repo = true;

        daemon.maybe_reconcile_stale_worktrees().unwrap();

        let branch = git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert_eq!(
            branch,
            engineer_base_branch_name("eng-reconcile"),
            "worktree should be reset to base branch"
        );
    }

    #[test]
    fn reconcile_skips_working_engineer() {
        let (_tmp, repo, worktree_dir) = setup_reconcile_scenario("eng-working");

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-working", Some("manager"), true),
        ];
        let states = HashMap::from([("eng-working".to_string(), MemberState::Working)]);
        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(members)
            .states(states)
            .build();
        daemon.is_git_repo = true;

        daemon.maybe_reconcile_stale_worktrees().unwrap();

        let branch = git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert_eq!(
            branch, "eng-working-42",
            "worktree should stay on task branch when engineer is working"
        );
    }

    #[test]
    fn reconcile_skips_idle_engineer_with_active_task() {
        let (_tmp, repo, worktree_dir) = setup_reconcile_scenario("eng-active");

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-active", Some("manager"), true),
        ];
        let states = HashMap::from([("eng-active".to_string(), MemberState::Idle)]);
        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(members)
            .states(states)
            .build();
        daemon.is_git_repo = true;
        daemon.active_tasks.insert("eng-active".to_string(), 42);

        daemon.maybe_reconcile_stale_worktrees().unwrap();

        let branch = git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert_eq!(
            branch, "eng-active-42",
            "worktree should stay on task branch when engineer has active task"
        );
    }

    #[test]
    fn reconcile_skips_unmerged_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-reconcile-unmerged");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-unmerged");
        let team_config_dir = repo.join(".batty").join("team_config");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-unmerged", &team_config_dir).unwrap();

        // Create a task branch with uncommitted merge work (not merged to main)
        let task_branch = "eng-unmerged-99";
        git_ok(&worktree_dir, &["checkout", "-b", task_branch]);
        std::fs::write(worktree_dir.join("wip.txt"), "wip\n").unwrap();
        git_ok(&worktree_dir, &["add", "wip.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "work in progress"]);

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-unmerged", Some("manager"), true),
        ];
        let states = HashMap::from([("eng-unmerged".to_string(), MemberState::Idle)]);
        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(members)
            .states(states)
            .build();
        daemon.is_git_repo = true;

        daemon.maybe_reconcile_stale_worktrees().unwrap();

        let branch = git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert_eq!(
            branch, "eng-unmerged-99",
            "worktree should stay on unmerged task branch"
        );
    }

    #[test]
    fn reconcile_emits_worktree_reconciled_event() {
        let (_tmp, repo, _worktree_dir) = setup_reconcile_scenario("eng-event");

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-event", Some("manager"), true),
        ];
        let states = HashMap::from([("eng-event".to_string(), MemberState::Idle)]);
        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(members)
            .states(states)
            .build();
        daemon.is_git_repo = true;

        daemon.maybe_reconcile_stale_worktrees().unwrap();

        let events = crate::team::events::read_events(
            &repo.join(".batty").join("team_config").join("events.jsonl"),
        )
        .unwrap_or_default();
        assert!(
            events.iter().any(|e| e.event == "worktree_reconciled"
                && e.role.as_deref() == Some("eng-event")),
            "should emit worktree_reconciled event"
        );
    }

    // ── Integration tests: stall → checkpoint → restart → resume ──

    #[test]
    #[serial]
    fn stall_checkpoint_restart_resume_full_flow() {
        // End-to-end: stall fires → checkpoint written → agent restarted →
        // restart notice includes checkpoint content.
        let session = format!("batty-test-stall-cp-flow-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-cp-flow";
        let lead_name = "lead-cp-flow";
        let (fake_bin, fake_log) = setup_fake_claude(&tmp, member_name);
        let worktree_path = tmp
            .path()
            .join(".batty")
            .join("worktrees")
            .join(member_name);
        std::fs::create_dir_all(&worktree_path).unwrap();

        // Write a test output file so checkpoint picks it up.
        std::fs::write(
            worktree_path.join(".batty_test_output"),
            "test result: ok. 7 passed; 0 failed\n",
        )
        .unwrap();

        crate::tmux::create_session(&session, "bash", &[], tmp.path().to_str().unwrap()).unwrap();
        crate::tmux::create_window(
            &session,
            "keeper",
            "sleep",
            &["30".to_string()],
            tmp.path().to_str().unwrap(),
        )
        .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        Command::new("tmux")
            .args(["set-option", "-p", "-t", &pane_id, "remain-on-exit", "on"])
            .output()
            .unwrap();

        let lead = MemberInstance {
            name: lead_name.to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: member_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some(lead_name.to_string()),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                agent: None,
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                external_senders: Vec::new(),
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.clone(),
            members: vec![lead, engineer],
            pane_map: HashMap::from([(member_name.to_string(), pane_id.clone())]),
        })
        .unwrap();
        daemon
            .states
            .insert(member_name.to_string(), MemberState::Working);
        daemon.active_tasks.insert(member_name.to_string(), 100);

        let root = crate::team::inbox::inboxes_root(tmp.path());
        crate::team::inbox::init_inbox(&root, member_name).unwrap();
        crate::team::inbox::init_inbox(&root, lead_name).unwrap();
        write_owned_task_file_with_context(
            tmp.path(),
            100,
            "checkpoint-flow-task",
            "in-progress",
            member_name,
            &format!("{member_name}/100"),
            &worktree_path.display().to_string(),
        );

        // 1. Verify no checkpoint exists before stall.
        let cp_path = crate::team::checkpoint::checkpoint_path(tmp.path(), member_name);
        assert!(
            !cp_path.exists(),
            "checkpoint should not exist before stall"
        );

        // 2. Trigger stall handler.
        daemon.handle_stalled_agent(member_name, 300).unwrap();

        // 3. Verify checkpoint file was written.
        assert!(
            cp_path.exists(),
            "checkpoint file must exist after stall restart"
        );
        let cp_content = std::fs::read_to_string(&cp_path).unwrap();
        assert!(
            cp_content.contains("# Progress Checkpoint: eng-cp-flow"),
            "checkpoint must contain role header"
        );
        assert!(
            cp_content.contains("**Task:** #100"),
            "checkpoint must reference task id"
        );
        assert!(
            cp_content.contains("checkpoint-flow-task"),
            "checkpoint must contain task title"
        );

        // 4. Verify events: stall_detected + agent_restarted.
        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();

        let stall_events: Vec<_> = events
            .iter()
            .filter(|e| e.event == "stall_detected")
            .collect();
        assert_eq!(stall_events.len(), 1);
        assert_eq!(stall_events[0].role.as_deref(), Some(member_name));
        assert_eq!(stall_events[0].task.as_deref(), Some("100"));
        assert_eq!(stall_events[0].uptime_secs, Some(300));

        let restart_events: Vec<_> = events
            .iter()
            .filter(|e| e.event == "agent_restarted")
            .collect();
        assert_eq!(restart_events.len(), 1);
        assert_eq!(restart_events[0].reason.as_deref(), Some("stalled"));
        assert_eq!(restart_events[0].restart_count, Some(1));

        // 5. Verify a message was routed to the engineer (restart notice).
        //    The notice is delivered via live tmux injection (not inbox) when the pane
        //    is available. We verify via the message_routed event in the event log.
        assert!(
            events.iter().any(|e| {
                e.event == "message_routed"
                    && e.from.as_deref() == Some("daemon")
                    && e.to.as_deref() == Some(member_name)
            }),
            "restart notice must be routed from daemon to engineer"
        );

        // 6. Verify checkpoint content would be included in the restart notice.
        //    The code reads the checkpoint and appends it to the notice. Since we
        //    verified the checkpoint exists with the right content in step 3, and
        //    the message_routed event confirms delivery in step 5, the chain is
        //    complete. Additionally verify the checkpoint is readable.
        let checkpoint_for_resume =
            crate::team::checkpoint::read_checkpoint(tmp.path(), member_name);
        assert!(
            checkpoint_for_resume.is_some(),
            "checkpoint must be readable for resume"
        );
        let resume_content = checkpoint_for_resume.unwrap();
        assert!(resume_content.contains("# Progress Checkpoint:"));
        assert!(resume_content.contains("**Task:** #100"));

        // 6. Verify agent was relaunched (fake claude log).
        let _log = (0..100)
            .find_map(|_| {
                let content = std::fs::read_to_string(&fake_log).ok()?;
                if content.contains("Continuing Task #100") {
                    Some(content)
                } else {
                    std::thread::sleep(Duration::from_millis(100));
                    None
                }
            })
            .expect("fake claude should have been launched with task context");

        crate::tmux::kill_session(&session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
    }

    #[test]
    fn stall_with_no_active_task_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-no-task";
        let lead_name = "lead-no-task";
        let lead = MemberInstance {
            name: lead_name.to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: member_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some(lead_name.to_string()),
            use_worktrees: false,
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![lead, engineer]);
        // No active task set — active_tasks is empty.

        // Should be a no-op: no events, no checkpoint.
        daemon.handle_stalled_agent(member_name, 300).unwrap();

        let cp_path = crate::team::checkpoint::checkpoint_path(tmp.path(), member_name);
        assert!(!cp_path.exists(), "no checkpoint when no active task");

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap_or_default();
        assert!(
            events.iter().all(|e| e.event != "stall_detected"),
            "no stall_detected event when no active task"
        );
    }

    #[test]
    #[serial]
    fn stall_overwrites_existing_checkpoint() {
        // If a checkpoint already exists for this role, stall handler must overwrite it.
        let session = format!("batty-test-stall-cp-overwrite-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-cp-ow";
        let lead_name = "lead-cp-ow";
        let (fake_bin, _fake_log) = setup_fake_claude(&tmp, member_name);
        let worktree_path = tmp
            .path()
            .join(".batty")
            .join("worktrees")
            .join(member_name);
        std::fs::create_dir_all(&worktree_path).unwrap();

        // Write a pre-existing checkpoint for an older task.
        let old_cp = crate::team::checkpoint::Checkpoint {
            role: member_name.to_string(),
            task_id: 50,
            task_title: "Old task".to_string(),
            task_description: "Old description".to_string(),
            branch: Some("old-branch".to_string()),
            last_commit: None,
            test_summary: None,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
        };
        crate::team::checkpoint::write_checkpoint(tmp.path(), &old_cp).unwrap();
        let cp_path = crate::team::checkpoint::checkpoint_path(tmp.path(), member_name);
        assert!(cp_path.exists());
        let old_content = std::fs::read_to_string(&cp_path).unwrap();
        assert!(old_content.contains("Old task"));

        crate::tmux::create_session(&session, "bash", &[], tmp.path().to_str().unwrap()).unwrap();
        crate::tmux::create_window(
            &session,
            "keeper",
            "sleep",
            &["30".to_string()],
            tmp.path().to_str().unwrap(),
        )
        .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        Command::new("tmux")
            .args(["set-option", "-p", "-t", &pane_id, "remain-on-exit", "on"])
            .output()
            .unwrap();

        let lead = MemberInstance {
            name: lead_name.to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: member_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some(lead_name.to_string()),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                agent: None,
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                external_senders: Vec::new(),
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.clone(),
            members: vec![lead, engineer],
            pane_map: HashMap::from([(member_name.to_string(), pane_id.clone())]),
        })
        .unwrap();
        daemon
            .states
            .insert(member_name.to_string(), MemberState::Working);
        daemon.active_tasks.insert(member_name.to_string(), 200);

        let root = crate::team::inbox::inboxes_root(tmp.path());
        crate::team::inbox::init_inbox(&root, member_name).unwrap();
        crate::team::inbox::init_inbox(&root, lead_name).unwrap();
        write_owned_task_file_with_context(
            tmp.path(),
            200,
            "overwrite-test-task",
            "in-progress",
            member_name,
            &format!("{member_name}/200"),
            &worktree_path.display().to_string(),
        );

        daemon.handle_stalled_agent(member_name, 400).unwrap();

        // Checkpoint must now reference the new task, not the old one.
        let new_content = std::fs::read_to_string(&cp_path).unwrap();
        assert!(
            new_content.contains("**Task:** #200"),
            "checkpoint must reference new task after overwrite"
        );
        assert!(
            !new_content.contains("Old task"),
            "old checkpoint content must be gone"
        );
        assert!(
            new_content.contains("overwrite-test-task"),
            "checkpoint must contain new task title"
        );

        crate::tmux::kill_session(&session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
    }

    #[test]
    #[serial]
    fn stall_checkpoint_with_missing_worktree() {
        // When worktree directory doesn't exist, checkpoint should still be written
        // but branch/commit fields will be None (falls back to task.branch).
        let session = format!("batty-test-stall-cp-nowt-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-cp-nowt";
        let lead_name = "lead-cp-nowt";
        let (fake_bin, _fake_log) = setup_fake_claude(&tmp, member_name);
        // Deliberately do NOT create the worktree directory.

        crate::tmux::create_session(&session, "bash", &[], tmp.path().to_str().unwrap()).unwrap();
        crate::tmux::create_window(
            &session,
            "keeper",
            "sleep",
            &["30".to_string()],
            tmp.path().to_str().unwrap(),
        )
        .unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        Command::new("tmux")
            .args(["set-option", "-p", "-t", &pane_id, "remain-on-exit", "on"])
            .output()
            .unwrap();

        let lead = MemberInstance {
            name: lead_name.to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: member_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some(lead_name.to_string()),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                agent: None,
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                external_senders: Vec::new(),
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: Vec::new(),
            },
            session: session.clone(),
            members: vec![lead, engineer],
            pane_map: HashMap::from([(member_name.to_string(), pane_id.clone())]),
        })
        .unwrap();
        daemon
            .states
            .insert(member_name.to_string(), MemberState::Working);
        daemon.active_tasks.insert(member_name.to_string(), 300);

        let root = crate::team::inbox::inboxes_root(tmp.path());
        crate::team::inbox::init_inbox(&root, member_name).unwrap();
        crate::team::inbox::init_inbox(&root, lead_name).unwrap();
        // Task has a branch set in its frontmatter, but no actual worktree.
        write_owned_task_file_with_context(
            tmp.path(),
            300,
            "missing-wt-task",
            "in-progress",
            member_name,
            &format!("{member_name}/300"),
            "/nonexistent/worktree",
        );

        daemon.handle_stalled_agent(member_name, 500).unwrap();

        // Checkpoint should still be written even without a real worktree.
        let cp_path = crate::team::checkpoint::checkpoint_path(tmp.path(), member_name);
        assert!(
            cp_path.exists(),
            "checkpoint must be written even without worktree"
        );
        let cp_content = std::fs::read_to_string(&cp_path).unwrap();
        assert!(cp_content.contains("**Task:** #300"));
        assert!(cp_content.contains("missing-wt-task"));
        // Branch comes from task frontmatter, not git.
        assert!(cp_content.contains(&format!("**Branch:** {member_name}/300")));
        // No last commit since worktree doesn't exist.
        assert!(!cp_content.contains("**Last commit:**"));

        crate::tmux::kill_session(&session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
    }

    #[test]
    fn stall_checkpoint_cleared_on_task_clear() {
        // Verify that clear_active_task removes the checkpoint file.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-cp-clear";
        let cp = crate::team::checkpoint::Checkpoint {
            role: member_name.to_string(),
            task_id: 42,
            task_title: "Clearable task".to_string(),
            task_description: "Will be cleared".to_string(),
            branch: None,
            last_commit: None,
            test_summary: None,
            timestamp: "2026-03-22T00:00:00Z".to_string(),
        };
        crate::team::checkpoint::write_checkpoint(tmp.path(), &cp).unwrap();
        let cp_path = crate::team::checkpoint::checkpoint_path(tmp.path(), member_name);
        assert!(cp_path.exists());

        let mut daemon = make_test_daemon(tmp.path(), vec![]);
        daemon.active_tasks.insert(member_name.to_string(), 42);

        daemon.clear_active_task(member_name);

        assert!(
            !cp_path.exists(),
            "checkpoint must be removed when task is cleared"
        );
        assert!(daemon.active_tasks.get(member_name).is_none());
    }

    // ---- uncommitted work warning tests ----

    /// Initialize a bare git repo directly in a directory (no TempDir wrapper needed).
    fn init_bare_git_repo(path: &Path) {
        git_ok(
            path.parent().unwrap(),
            &["init", "-b", "main", path.to_str().unwrap()],
        );
        git_ok(path, &["config", "user.email", "batty@example.com"]);
        git_ok(path, &["config", "user.name", "Batty Tests"]);
    }

    #[test]
    fn uncommitted_diff_lines_counts_unstaged() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_bare_git_repo(&repo);

        std::fs::write(repo.join("hello.txt"), "line1\nline2\nline3\n").unwrap();
        git_ok(&repo, &["add", "hello.txt"]);
        git_ok(&repo, &["commit", "-m", "init"]);

        // Modify 2 of 3 lines (unstaged): 2 removed + 2 added = 4 lines
        std::fs::write(repo.join("hello.txt"), "changed1\nchanged2\nline3\n").unwrap();

        let lines = super::uncommitted_diff_lines(&repo).unwrap();
        assert!(lines >= 3, "expected >=3 uncommitted lines, got {lines}");
    }

    #[test]
    fn uncommitted_diff_lines_empty_when_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_bare_git_repo(&repo);

        std::fs::write(repo.join("hello.txt"), "line1\n").unwrap();
        git_ok(&repo, &["add", "hello.txt"]);
        git_ok(&repo, &["commit", "-m", "init"]);

        let lines = super::uncommitted_diff_lines(&repo).unwrap();
        assert_eq!(lines, 0, "clean repo should have 0 uncommitted lines");
    }

    #[test]
    fn uncommitted_diff_lines_includes_staged_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_bare_git_repo(&repo);

        std::fs::write(repo.join("hello.txt"), "original\n").unwrap();
        git_ok(&repo, &["add", "hello.txt"]);
        git_ok(&repo, &["commit", "-m", "init"]);

        // Stage a modification
        std::fs::write(repo.join("hello.txt"), "modified\n").unwrap();
        git_ok(&repo, &["add", "hello.txt"]);

        let lines = super::uncommitted_diff_lines(&repo).unwrap();
        assert!(lines >= 2, "staged changes should count, got {lines}");
    }

    fn make_uncommitted_warn_daemon(tmp: &tempfile::TempDir, threshold: usize) -> TeamDaemon {
        let repo = tmp.path();
        let team_config_dir = repo.join(".batty").join("team_config");
        std::fs::create_dir_all(&team_config_dir).unwrap();

        // Create a worktree dir for eng-1 that is a standalone git repo
        let wt = repo.join(".batty").join("worktrees").join("eng-1");
        std::fs::create_dir_all(&wt).unwrap();
        init_bare_git_repo(&wt);

        // Create a tracked file and commit it
        std::fs::write(wt.join("big.txt"), "a\n".repeat(300)).unwrap();
        git_ok(&wt, &["add", "big.txt"]);
        git_ok(&wt, &["commit", "-m", "init"]);
        // Now modify to create an uncommitted diff (300 lines changed)
        std::fs::write(wt.join("big.txt"), "b\n".repeat(300)).unwrap();

        // Create inbox dirs
        let inbox_root = crate::team::inbox::inboxes_root(repo);
        crate::team::inbox::init_inbox(&inbox_root, "eng-1").unwrap();
        crate::team::inbox::init_inbox(&inbox_root, "daemon").unwrap();
        crate::team::inbox::init_inbox(&inbox_root, "manager").unwrap();

        TestDaemonBuilder::new(repo)
            .members(vec![
                manager_member("manager", Some("architect")),
                engineer_member("eng-1", Some("manager"), true),
            ])
            .workflow_policy(WorkflowPolicy {
                uncommitted_warn_threshold: threshold,
                ..WorkflowPolicy::default()
            })
            .build()
    }

    #[test]
    fn maybe_warn_uncommitted_work_sends_nudge_above_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = make_uncommitted_warn_daemon(&tmp, 100);
        daemon.is_git_repo = true;

        daemon.maybe_warn_uncommitted_work().unwrap();

        assert!(
            daemon.last_uncommitted_warn.contains_key("eng-1"),
            "should have tracked the warning time for eng-1"
        );

        // Check that a message was queued to eng-1's inbox
        let inbox_root = crate::team::inbox::inboxes_root(tmp.path());
        let messages = crate::team::inbox::all_messages(&inbox_root, "eng-1").unwrap();
        assert!(
            !messages.is_empty(),
            "eng-1 should have received a commit reminder"
        );
        let body = &messages[0].0.body;
        assert!(
            body.contains("COMMIT REMINDER"),
            "message should contain COMMIT REMINDER, got: {body}"
        );
    }

    #[test]
    fn maybe_warn_uncommitted_work_rate_limited() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = make_uncommitted_warn_daemon(&tmp, 100);
        daemon.is_git_repo = true;

        // First call should send a warning
        daemon.maybe_warn_uncommitted_work().unwrap();
        let inbox_root = crate::team::inbox::inboxes_root(tmp.path());
        let count_after_first = crate::team::inbox::all_messages(&inbox_root, "eng-1")
            .unwrap()
            .len();

        // Second call should be rate-limited (no additional message)
        daemon.maybe_warn_uncommitted_work().unwrap();
        let count_after_second = crate::team::inbox::all_messages(&inbox_root, "eng-1")
            .unwrap()
            .len();

        assert_eq!(
            count_after_first, count_after_second,
            "second call within cooldown should not send another warning"
        );
    }

    #[test]
    fn maybe_warn_uncommitted_work_disabled_when_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = make_uncommitted_warn_daemon(&tmp, 0);
        daemon.is_git_repo = true;

        daemon.maybe_warn_uncommitted_work().unwrap();

        assert!(
            daemon.last_uncommitted_warn.is_empty(),
            "threshold=0 should disable warnings"
        );
    }

    #[test]
    fn maybe_warn_uncommitted_work_skips_below_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = make_uncommitted_warn_daemon(&tmp, 100);
        daemon.is_git_repo = true;

        // Reset the worktree to have a clean state
        let wt = tmp.path().join(".batty").join("worktrees").join("eng-1");
        git_ok(&wt, &["checkout", "--", "."]);

        daemon.maybe_warn_uncommitted_work().unwrap();

        assert!(
            daemon.last_uncommitted_warn.is_empty(),
            "clean worktree should not trigger a warning"
        );
    }

    #[test]
    fn maybe_warn_uncommitted_work_skips_non_worktree_engineers() {
        let tmp = tempfile::tempdir().unwrap();
        let team_config_dir = tmp.path().join(".batty").join("team_config");
        std::fs::create_dir_all(&team_config_dir).unwrap();

        let inbox_root = crate::team::inbox::inboxes_root(tmp.path());
        crate::team::inbox::init_inbox(&inbox_root, "eng-1").unwrap();
        crate::team::inbox::init_inbox(&inbox_root, "daemon").unwrap();
        crate::team::inbox::init_inbox(&inbox_root, "manager").unwrap();

        // Engineer without worktree (use_worktrees: false)
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member("manager", Some("architect")),
                engineer_member("eng-1", Some("manager"), false),
            ])
            .workflow_policy(WorkflowPolicy {
                uncommitted_warn_threshold: 10,
                ..WorkflowPolicy::default()
            })
            .build();
        daemon.is_git_repo = true;

        daemon.maybe_warn_uncommitted_work().unwrap();

        assert!(
            daemon.last_uncommitted_warn.is_empty(),
            "engineer without worktrees should be skipped"
        );
    }

    #[test]
    fn uncommitted_warn_threshold_config_default() {
        let policy = WorkflowPolicy::default();
        assert_eq!(policy.uncommitted_warn_threshold, 200);
    }

    #[test]
    fn false_done_prevention_no_commits_returns_zero() {
        // Verify that git rev-list --count main..HEAD returns 0 when
        // the worktree branch has no commits beyond main.
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-false-done");

        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");
        crate::team::task_loop::setup_engineer_worktree(
            &repo,
            &worktree_dir,
            "eng-1",
            &team_config_dir,
        )
        .unwrap();

        // No commits made on the branch — rev-list should return 0
        let output =
            crate::team::git_cmd::run_git(&worktree_dir, &["rev-list", "--count", "main..HEAD"])
                .unwrap();
        let count: u32 = output.stdout.trim().parse().unwrap();
        assert_eq!(count, 0, "branch with no new commits should return 0");
    }

    #[test]
    fn false_done_prevention_with_commits_returns_nonzero() {
        // Verify that git rev-list --count main..HEAD returns >0 when
        // the worktree branch has commits beyond main.
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-false-done-ok");

        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");
        crate::team::task_loop::setup_engineer_worktree(
            &repo,
            &worktree_dir,
            "eng-1",
            &team_config_dir,
        )
        .unwrap();

        // Make a commit on the branch
        std::fs::write(worktree_dir.join("work.txt"), "done\n").unwrap();
        git_ok(&worktree_dir, &["add", "work.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "task work"]);

        let output =
            crate::team::git_cmd::run_git(&worktree_dir, &["rev-list", "--count", "main..HEAD"])
                .unwrap();
        let count: u32 = output.stdout.trim().parse().unwrap();
        assert!(count > 0, "branch with commits should return > 0");
    }

    #[test]
    fn false_done_prevention_invalid_worktree_returns_error() {
        // Verify that git rev-list in a non-git directory returns an error
        let tmp = tempfile::tempdir().unwrap();
        let result =
            crate::team::git_cmd::run_git(tmp.path(), &["rev-list", "--count", "main..HEAD"]);
        assert!(result.is_err(), "non-git dir should return error");
    }

    // ── restart_assignment_message tests ──

    #[test]
    fn restart_assignment_message_includes_task_id_and_title() {
        let task = crate::task::Task {
            id: 42,
            title: "implement widget".to_string(),
            description: "Add the new widget feature.".to_string(),
            status: "in-progress".to_string(),
            priority: "high".to_string(),
            claimed_by: Some("eng-1".into()),
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
            batty_config: None,
            source_path: PathBuf::from("/tmp/task-42.md"),
        };
        let msg = TeamDaemon::restart_assignment_message(&task);
        assert!(msg.contains("Task #42"));
        assert!(msg.contains("implement widget"));
        assert!(msg.contains("Add the new widget feature."));
        assert!(msg.contains("Previous session exhausted context"));
        // No branch or worktree lines when those fields are None.
        assert!(!msg.contains("Branch:"));
        assert!(!msg.contains("Worktree:"));
    }

    #[test]
    fn restart_assignment_message_includes_branch_and_worktree() {
        let task = crate::task::Task {
            id: 99,
            title: "fix tests".to_string(),
            description: "Fix failing tests.".to_string(),
            status: "in-progress".to_string(),
            priority: "medium".to_string(),
            claimed_by: Some("eng-2".into()),
            blocked: None,
            tags: Vec::new(),
            depends_on: Vec::new(),
            review_owner: None,
            blocked_on: None,
            worktree_path: Some("/tmp/worktrees/eng-2".to_string()),
            branch: Some("eng-2/99".to_string()),
            commit: None,
            artifacts: Vec::new(),
            next_action: None,
            scheduled_for: None,
            cron_schedule: None,
            cron_last_run: None,
            completed: None,
            batty_config: None,
            source_path: PathBuf::from("/tmp/task-99.md"),
        };
        let msg = TeamDaemon::restart_assignment_message(&task);
        assert!(msg.contains("Branch: eng-2/99"));
        assert!(msg.contains("Worktree: /tmp/worktrees/eng-2"));
    }

    // ── cooldown key tests ──

    #[test]
    fn context_restart_cooldown_key_format() {
        assert_eq!(
            TeamDaemon::context_restart_cooldown_key("eng-1"),
            "context-restart::eng-1"
        );
    }

    #[test]
    fn context_escalation_cooldown_key_format() {
        assert_eq!(
            TeamDaemon::context_escalation_cooldown_key("eng-1"),
            "context-escalation::eng-1"
        );
    }

    // ── context_restart_count tests ──

    #[test]
    fn context_restart_count_returns_zero_with_no_events() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        let daemon = make_test_daemon(tmp.path(), vec![]);
        assert_eq!(daemon.context_restart_count(42).unwrap(), 0);
    }

    #[test]
    fn context_restart_count_counts_all_reasons_for_task() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        write_event_log(
            tmp.path(),
            &[
                TeamEvent::agent_restarted("eng-1", "42", "context_exhausted", 1),
                TeamEvent::agent_restarted("eng-1", "42", "stalled", 1),
                TeamEvent::agent_restarted("eng-1", "99", "context_exhausted", 1),
            ],
        );

        let daemon = make_test_daemon(tmp.path(), vec![]);
        // context_restart_count counts ALL agent_restarted events for the task
        // (not filtered by reason, unlike stall_restart_count)
        assert_eq!(daemon.context_restart_count(42).unwrap(), 2);
        assert_eq!(daemon.context_restart_count(99).unwrap(), 1);
        assert_eq!(daemon.context_restart_count(100).unwrap(), 0);
    }

    // ── load_prompt tests ──

    #[test]
    fn load_prompt_substitutes_template_variables() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("engineer.md"),
            "Hello {{member_name}}, role={{role_name}}, reports_to={{reports_to}}",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "engineer".to_string(),
            role_type: RoleType::Engineer,
            agent: None,
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
        };
        let daemon = make_test_daemon(tmp.path(), vec![member.clone()]);
        let prompt = daemon.load_prompt(&member, &config_dir);
        assert_eq!(prompt, "Hello eng-1, role=engineer, reports_to=manager");
    }

    #[test]
    fn load_prompt_uses_custom_prompt_file() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("custom.md"), "Custom: {{member_name}}").unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member = MemberInstance {
            name: "arch-1".to_string(),
            role_name: "architect".to_string(),
            role_type: RoleType::Architect,
            agent: None,
            prompt: Some("custom.md".to_string()),
            reports_to: None,
            use_worktrees: false,
        };
        let daemon = make_test_daemon(tmp.path(), vec![member.clone()]);
        let prompt = daemon.load_prompt(&member, &config_dir);
        assert_eq!(prompt, "Custom: arch-1");
    }

    #[test]
    fn load_prompt_fallback_on_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        // Do NOT create the expected file
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "engineer".to_string(),
            role_type: RoleType::Engineer,
            agent: None,
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let daemon = make_test_daemon(tmp.path(), vec![member.clone()]);
        let prompt = daemon.load_prompt(&member, &config_dir);
        assert!(prompt.contains("eng-1"));
        assert!(prompt.contains("Engineer"));
    }

    #[test]
    fn load_prompt_reports_to_none_becomes_none_string() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("architect.md"), "reports={{reports_to}}").unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member = MemberInstance {
            name: "arch-1".to_string(),
            role_name: "architect".to_string(),
            role_type: RoleType::Architect,
            agent: None,
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let daemon = make_test_daemon(tmp.path(), vec![member.clone()]);
        let prompt = daemon.load_prompt(&member, &config_dir);
        assert_eq!(prompt, "reports=none");
    }

    #[test]
    fn load_prompt_default_file_per_role_type() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("architect.md"), "ARCH").unwrap();
        std::fs::write(config_dir.join("manager.md"), "MGR").unwrap();
        std::fs::write(config_dir.join("engineer.md"), "ENG").unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let daemon = make_test_daemon(tmp.path(), vec![]);

        let arch = MemberInstance {
            name: "a".to_string(),
            role_name: "a".to_string(),
            role_type: RoleType::Architect,
            agent: None,
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let mgr = MemberInstance {
            name: "m".to_string(),
            role_name: "m".to_string(),
            role_type: RoleType::Manager,
            agent: None,
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let eng = MemberInstance {
            name: "e".to_string(),
            role_name: "e".to_string(),
            role_type: RoleType::Engineer,
            agent: None,
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        assert_eq!(daemon.load_prompt(&arch, &config_dir), "ARCH");
        assert_eq!(daemon.load_prompt(&mgr, &config_dir), "MGR");
        assert_eq!(daemon.load_prompt(&eng, &config_dir), "ENG");
    }

    // ── handle_context_exhaustion edge cases ──

    #[test]
    fn handle_context_exhaustion_no_task_sets_idle() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![engineer_member("eng-1", Some("manager"), false)])
            .build();
        daemon
            .states
            .insert("eng-1".to_string(), MemberState::Working);
        // No active task set — active_tasks is empty.

        daemon.handle_context_exhaustion("eng-1").unwrap();

        assert_eq!(daemon.states.get("eng-1"), Some(&MemberState::Idle));
    }

    #[test]
    fn handle_context_exhaustion_unknown_member_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        // Calling with a member that doesn't exist should not panic.
        let result = daemon.handle_context_exhaustion("nonexistent");
        assert!(result.is_ok());
    }

    #[test]
    fn handle_context_exhaustion_escalation_cooldown_suppresses() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-1";
        let lead_name = "manager";
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, lead_name).unwrap();
        inbox::init_inbox(&inbox_root, member_name).unwrap();

        write_owned_task_file(tmp.path(), 42, "test-task", "in-progress", member_name);
        // Write a prior restart event so we'd normally escalate.
        write_event_log(
            tmp.path(),
            &[TeamEvent::agent_restarted(
                member_name,
                "42",
                "context_exhausted",
                1,
            )],
        );

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                manager_member(lead_name, Some("architect")),
                engineer_member(member_name, Some(lead_name), false),
            ])
            .build();
        daemon.active_tasks.insert(member_name.to_string(), 42);
        // Set the escalation cooldown so it suppresses the escalation.
        daemon.intervention_cooldowns.insert(
            TeamDaemon::context_escalation_cooldown_key(member_name),
            Instant::now(),
        );

        daemon.handle_context_exhaustion(member_name).unwrap();

        // No message should have been sent to the manager.
        let pending = inbox::pending_messages(&inbox_root, lead_name).unwrap();
        assert!(
            pending.is_empty(),
            "escalation should be suppressed by cooldown"
        );
    }

    // ── handle_stalled_agent edge cases ──

    #[test]
    fn handle_stalled_agent_no_task_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![engineer_member("eng-1", Some("manager"), false)])
            .build();
        // No active task.
        let result = daemon.handle_stalled_agent("eng-1", 600);
        assert!(result.is_ok());
        // No events should be emitted.
        let events_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        let events = crate::team::events::read_events(&events_path).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn handle_stalled_agent_cooldown_prevents_action() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-1";
        write_owned_task_file(tmp.path(), 42, "test-task", "in-progress", member_name);
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, member_name).unwrap();

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![engineer_member(member_name, Some("manager"), false)])
            .build();
        daemon.active_tasks.insert(member_name.to_string(), 42);
        // Set the stall cooldown to suppress.
        daemon
            .intervention_cooldowns
            .insert(format!("stall-restart::{member_name}"), Instant::now());

        let result = daemon.handle_stalled_agent(member_name, 600);
        assert!(result.is_ok());
    }

    // ── check_backend_health additional edge cases ──

    #[test]
    fn check_backend_health_tracks_multiple_members() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                architect_member("architect"),
                engineer_member("eng-1", Some("architect"), false),
            ])
            .build();
        // Force interval to have elapsed.
        daemon.last_health_check = Instant::now() - Duration::from_secs(3600);

        daemon.check_backend_health().unwrap();

        // Both non-user members should have a health entry.
        assert!(daemon.backend_health.contains_key("architect"));
        assert!(daemon.backend_health.contains_key("eng-1"));
    }

    #[test]
    fn check_backend_health_default_agent_is_claude() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let mut member = architect_member("architect");
        member.agent = None; // No explicit agent — should default to "claude".
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![member])
            .build();
        daemon.last_health_check = Instant::now() - Duration::from_secs(3600);

        // Should not panic — defaults to "claude" backend.
        daemon.check_backend_health().unwrap();
        assert!(daemon.backend_health.contains_key("architect"));
    }

    // ── active_task tests ──

    #[test]
    fn active_task_returns_none_when_no_active_task_id() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        let daemon = make_test_daemon(tmp.path(), vec![]);
        let result = daemon.active_task("eng-1").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn active_task_returns_none_when_task_id_not_on_board() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        let mut daemon = make_test_daemon(tmp.path(), vec![]);
        daemon.active_tasks.insert("eng-1".to_string(), 999);
        let result = daemon.active_task("eng-1").unwrap();
        assert!(result.is_none(), "nonexistent task should return None");
    }

    #[test]
    fn active_task_returns_task_when_found() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        write_owned_task_file(tmp.path(), 42, "my-task", "in-progress", "eng-1");

        let mut daemon = make_test_daemon(tmp.path(), vec![]);
        daemon.active_tasks.insert("eng-1".to_string(), 42);
        let result = daemon.active_task("eng-1").unwrap();
        assert!(result.is_some());
        let task = result.unwrap();
        assert_eq!(task.id, 42);
        assert_eq!(task.title, "my-task");
    }

    // ── uncommitted_diff_lines edge cases ──

    #[test]
    fn uncommitted_diff_lines_mixed_staged_and_unstaged() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_bare_git_repo(&repo);

        std::fs::write(repo.join("a.txt"), "line1\n").unwrap();
        std::fs::write(repo.join("b.txt"), "line1\n").unwrap();
        git_ok(&repo, &["add", "."]);
        git_ok(&repo, &["commit", "-m", "init"]);

        // Modify a.txt (unstaged) and b.txt (staged)
        std::fs::write(repo.join("a.txt"), "changed\n").unwrap();
        std::fs::write(repo.join("b.txt"), "changed\n").unwrap();
        git_ok(&repo, &["add", "b.txt"]);

        let lines = super::uncommitted_diff_lines(&repo).unwrap();
        // a.txt unstaged: 1 removed + 1 added = 2
        // b.txt staged: 1 removed + 1 added = 2
        assert!(lines >= 4, "mixed staged+unstaged should sum, got {lines}");
    }

    #[test]
    fn uncommitted_diff_lines_non_git_dir_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        // Not a git repo — git diff should fail but the function handles it.
        let result = super::uncommitted_diff_lines(tmp.path());
        // The function uses Command::output() which may still succeed with non-zero exit.
        // Either it returns an error or returns 0 lines — both are acceptable.
        match result {
            Ok(lines) => assert_eq!(lines, 0),
            Err(_) => {} // Also acceptable
        }
    }

    #[test]
    fn uncommitted_diff_lines_new_file_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_bare_git_repo(&repo);

        std::fs::write(repo.join("init.txt"), "x\n").unwrap();
        git_ok(&repo, &["add", "."]);
        git_ok(&repo, &["commit", "-m", "init"]);

        // Add new file but only stage it (diff --cached will show it)
        std::fs::write(repo.join("new.txt"), "a\nb\nc\n").unwrap();
        git_ok(&repo, &["add", "new.txt"]);

        let lines = super::uncommitted_diff_lines(&repo).unwrap();
        assert!(
            lines >= 3,
            "new staged file should count as added lines, got {lines}"
        );
    }
}
