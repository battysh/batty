//! Core team daemon — polling loop, agent lifecycle, message routing.
//!
//! The daemon ties together all team subsystems: it spawns agents in tmux
//! panes, monitors their output via `SessionWatcher`, routes messages between
//! roles, generates periodic standups, and emits structured events.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use uuid::Uuid;

use super::board;
use super::comms::{self, Channel};
#[cfg(test)]
use super::config::OrchestratorPosition;
use super::config::{RoleType, TeamConfig};
use super::events::{EventSink, TeamEvent};
use super::failure_patterns::{self, FailureWindow};
use super::hierarchy::MemberInstance;
use super::inbox;
use super::message;
use super::standup::{self, MemberState};
pub use super::task_loop::merge_engineer_branch;
use super::task_loop::{
    MergeLock, MergeOutcome, engineer_base_branch_name, next_unclaimed_task,
    prepare_engineer_assignment_worktree, read_task_title, run_tests_in_worktree,
    setup_engineer_worktree,
};
use super::watcher::{SessionTrackerConfig, SessionWatcher, WatcherState};
use super::{AssignmentDeliveryResult, AssignmentResultStatus, now_unix, store_assignment_result};
use crate::agent;
use crate::tmux;

/// Daemon configuration derived from TeamConfig.
pub struct DaemonConfig {
    pub project_root: PathBuf,
    pub team_config: TeamConfig,
    pub session: String,
    pub members: Vec<MemberInstance>,
    pub pane_map: HashMap<String, String>,
}

/// A scheduled nudge for a member: text to inject after sustained idleness.
///
/// The countdown starts when the member transitions from working to idle.
/// If the member starts working again before the timer fires, the countdown
/// resets. The nudge only fires once per idle period — the member must go
/// through working→idle again to start a new countdown.
struct NudgeSchedule {
    /// The nudge text extracted from the `## Nudge` section of the prompt .md.
    text: String,
    /// How long the member must stay idle before the nudge fires.
    interval: Duration,
    /// When the member last became idle (`None` if currently working/paused).
    idle_since: Option<Instant>,
    /// Whether the nudge has already fired for the current idle period.
    fired_this_idle: bool,
    /// Whether the timer is currently paused because the member is working.
    paused: bool,
}

/// The running team daemon.
pub struct TeamDaemon {
    config: DaemonConfig,
    watchers: HashMap<String, SessionWatcher>,
    states: HashMap<String, MemberState>,
    idle_started_at: HashMap<String, Instant>,
    active_tasks: HashMap<String, u32>,
    retry_counts: HashMap<String, u32>,
    triage_idle_epochs: HashMap<String, u64>,
    triage_interventions: HashMap<String, u64>,
    owned_task_interventions: HashMap<String, OwnedTaskInterventionState>,
    channels: HashMap<String, Box<dyn Channel>>,
    nudges: HashMap<String, NudgeSchedule>,
    telegram_bot: Option<super::telegram::TelegramBot>,
    failure_window: FailureWindow,
    last_pattern_notifications: HashMap<String, u32>,
    event_sink: EventSink,
    paused_standups: HashSet<String>,
    last_standup: HashMap<String, Instant>,
    last_board_rotation: Instant,
    last_auto_dispatch: Instant,
    retro_generated: bool,
    poll_interval: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OwnedTaskInterventionState {
    idle_epoch: u64,
    signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MemberWorktreeContext {
    path: PathBuf,
    branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReportDispatchSnapshot {
    name: String,
    is_working: bool,
    active_task_ids: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct LaunchIdentity {
    agent: String,
    prompt: String,
    session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AssignmentLaunch {
    branch: Option<String>,
    work_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedNudgeState {
    idle_elapsed_secs: Option<u64>,
    fired_this_idle: bool,
    paused: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedDaemonState {
    clean_shutdown: bool,
    saved_at: u64,
    states: HashMap<String, MemberState>,
    active_tasks: HashMap<String, u32>,
    retry_counts: HashMap<String, u32>,
    paused_standups: HashSet<String>,
    last_standup_elapsed_secs: HashMap<String, u64>,
    nudge_state: HashMap<String, PersistedNudgeState>,
}

#[derive(Debug, Clone)]
struct MemberLaunchPlan {
    short_cmd: String,
    identity: LaunchIdentity,
    initial_state: MemberState,
    activate_watcher: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageDelivery {
    Channel,
    LivePane,
    InboxQueued,
    SkippedUnknownRecipient,
}

impl TeamDaemon {
    /// Create a new daemon from resolved config and layout.
    pub fn new(config: DaemonConfig) -> Result<Self> {
        let team_config_dir = config.project_root.join(".batty").join("team_config");
        let events_path = team_config_dir.join("events.jsonl");
        let event_sink = EventSink::new(&events_path)?;

        // Create watchers for each pane member
        let mut watchers = HashMap::new();
        let stale_secs = config.team_config.standup.interval_secs * 2;
        for (name, pane_id) in &config.pane_map {
            let session_tracker = config
                .members
                .iter()
                .find(|member| member.name == *name)
                .and_then(|member| member_session_tracker_config(&config.project_root, member));
            watchers.insert(
                name.clone(),
                SessionWatcher::new(pane_id, name, stale_secs, session_tracker),
            );
        }

        // Create channels for user roles
        let mut channels: HashMap<String, Box<dyn Channel>> = HashMap::new();
        for role in &config.team_config.roles {
            if role.role_type == RoleType::User {
                if let (Some(ch_type), Some(ch_config)) = (&role.channel, &role.channel_config) {
                    match comms::channel_from_config(ch_type, ch_config) {
                        Ok(ch) => {
                            channels.insert(role.name.clone(), ch);
                        }
                        Err(e) => {
                            warn!(role = %role.name, error = %e, "failed to create channel");
                        }
                    }
                }
            }
        }

        // Create Telegram bot for inbound polling (if configured)
        let telegram_bot = config
            .team_config
            .roles
            .iter()
            .find(|r| r.role_type == RoleType::User && r.channel.as_deref() == Some("telegram"))
            .and_then(|r| r.channel_config.as_ref())
            .and_then(super::telegram::TelegramBot::from_config);

        let states = HashMap::new();

        // Build nudge schedules from role configs + prompt files
        let mut nudges = HashMap::new();
        for role in &config.team_config.roles {
            if let Some(interval_secs) = role.nudge_interval_secs {
                let prompt_file = role.prompt.as_deref().unwrap_or(match role.role_type {
                    RoleType::Architect => "architect.md",
                    RoleType::Manager => "manager.md",
                    RoleType::Engineer => "engineer.md",
                    RoleType::User => "architect.md",
                });
                let prompt_path = team_config_dir.join(prompt_file);
                if let Some(nudge_text) = extract_nudge_section(&prompt_path) {
                    // Apply nudge to all instances of this role
                    let instance_names: Vec<String> = config
                        .members
                        .iter()
                        .filter(|m| m.role_name == role.name)
                        .map(|m| m.name.clone())
                        .collect();
                    for name in instance_names {
                        info!(member = %name, interval_secs, "registered nudge");
                        nudges.insert(
                            name,
                            NudgeSchedule {
                                text: nudge_text.clone(),
                                interval: Duration::from_secs(interval_secs),
                                // All roles start idle, so begin the countdown
                                idle_since: Some(Instant::now()),
                                fired_this_idle: false,
                                paused: false,
                            },
                        );
                    }
                }
            }
        }

        Ok(Self {
            config,
            watchers,
            states,
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels,
            nudges,
            telegram_bot,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink,
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        })
    }

    /// Run the daemon loop. Blocks until the session is killed or an error occurs.
    ///
    /// If `resume` is true, agents are launched with session-resume flags
    /// (`claude --resume <session-id>` / `codex resume --last`) instead of fresh starts.
    pub fn run(&mut self, resume: bool) -> Result<()> {
        self.emit_event(TeamEvent::daemon_started());
        info!(session = %self.config.session, resume, "daemon started");
        self.record_orchestrator_action(format!(
            "runtime: orchestrator started (mode={}, resume={resume})",
            self.config.team_config.workflow_mode.as_str()
        ));

        // Install signal handler so we log clean shutdowns
        let shutdown_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag_clone = shutdown_flag.clone();
        if let Err(e) = ctrlc::set_handler(move || {
            flag_clone.store(true, std::sync::atomic::Ordering::SeqCst);
        }) {
            warn!(error = %e, "failed to install signal handler");
        }

        // Spawn agents in all panes
        self.spawn_all_agents(resume)?;
        if resume {
            self.restore_runtime_state();
        }
        self.persist_runtime_state(false)?;

        let started_at = Instant::now();
        let heartbeat_interval = Duration::from_secs(300); // 5 minutes
        let mut last_heartbeat = Instant::now();

        // Main polling loop
        let shutdown_reason;
        loop {
            // Check for signal-based shutdown
            if shutdown_flag.load(std::sync::atomic::Ordering::SeqCst) {
                shutdown_reason = "signal";
                info!("received shutdown signal");
                break;
            }

            if !tmux::session_exists(&self.config.session) {
                shutdown_reason = "session_gone";
                info!("tmux session gone, shutting down");
                break;
            }

            self.run_loop_step("restart_dead_members", |daemon| {
                daemon.restart_dead_members()
            });
            self.run_loop_step("poll_watchers", |daemon| daemon.poll_watchers());
            self.run_loop_step("sync_launch_state_session_ids", |daemon| {
                daemon.sync_launch_state_session_ids()
            });
            self.run_loop_step("drain_legacy_command_queue", |daemon| {
                daemon.drain_legacy_command_queue()
            });
            self.run_loop_step("deliver_inbox_messages", |daemon| {
                daemon.deliver_inbox_messages()
            });
            self.run_loop_step("maybe_intervene_triage_backlog", |daemon| {
                daemon.maybe_intervene_triage_backlog()
            });
            self.run_loop_step("maybe_intervene_review_backlog", |daemon| {
                daemon.maybe_intervene_review_backlog()
            });
            self.run_loop_step("maybe_intervene_owned_tasks", |daemon| {
                daemon.maybe_intervene_owned_tasks()
            });
            self.run_loop_step("maybe_auto_dispatch", |daemon| daemon.maybe_auto_dispatch());
            self.run_loop_step("maybe_intervene_manager_dispatch_gap", |daemon| {
                daemon.maybe_intervene_manager_dispatch_gap()
            });
            self.run_loop_step("maybe_intervene_architect_utilization", |daemon| {
                daemon.maybe_intervene_architect_utilization()
            });
            self.run_loop_step("poll_telegram", |daemon| daemon.poll_telegram());
            self.run_loop_step("deliver_user_inbox", |daemon| daemon.deliver_user_inbox());
            self.run_loop_step("maybe_fire_nudges", |daemon| daemon.maybe_fire_nudges());
            self.run_loop_step("maybe_generate_standup", |daemon| {
                daemon.maybe_generate_standup()
            });
            self.run_loop_step("maybe_rotate_board", |daemon| daemon.maybe_rotate_board());
            self.run_loop_step("maybe_generate_retrospective", |daemon| {
                daemon.maybe_generate_retrospective()
            });
            self.run_loop_step("maybe_notify_failure_patterns", |daemon| {
                daemon.maybe_notify_failure_patterns()
            });
            self.update_pane_status_labels();

            // Periodic heartbeat
            if last_heartbeat.elapsed() >= heartbeat_interval {
                let uptime = started_at.elapsed().as_secs();
                self.emit_event(TeamEvent::daemon_heartbeat(uptime));
                if let Err(error) = self.persist_runtime_state(false) {
                    warn!(error = %error, "failed to persist daemon checkpoint");
                }
                debug!(uptime_secs = uptime, "daemon heartbeat");
                last_heartbeat = Instant::now();
            }

            std::thread::sleep(self.poll_interval);
        }

        let uptime = started_at.elapsed().as_secs();
        if let Err(error) = self.persist_runtime_state(true) {
            warn!(error = %error, "failed to persist final daemon checkpoint");
        }
        self.emit_event(TeamEvent::daemon_stopped_with_reason(
            shutdown_reason,
            uptime,
        ));
        info!(
            reason = shutdown_reason,
            uptime_secs = uptime,
            "daemon stopped"
        );
        Ok(())
    }

    fn run_loop_step<F>(&mut self, step: &str, action: F)
    where
        F: FnOnce(&mut Self) -> Result<()>,
    {
        if let Err(error) = action(self) {
            warn!(step, error = %error, "daemon loop step failed; continuing");
            self.emit_event(TeamEvent::loop_step_error(step, &error.to_string()));
        }
    }

    fn emit_event(&mut self, event: TeamEvent) {
        self.failure_window.push(&event);
        if let Err(error) = self.event_sink.emit(event) {
            warn!(error = %error, "failed to write daemon event; continuing");
        }
    }

    fn maybe_notify_failure_patterns(&mut self) -> Result<()> {
        if !self.config.team_config.automation.failure_pattern_detection {
            return Ok(());
        }

        let patterns = self.failure_window.detect_failure_patterns();
        if patterns.is_empty() {
            return Ok(());
        }

        let notifications = failure_patterns::generate_pattern_notifications(&patterns, 3, 5);
        for (pattern, notification) in patterns
            .iter()
            .filter(|pattern| pattern.frequency >= 3)
            .zip(notifications)
        {
            let signature = format!(
                "{}:{}",
                pattern.pattern_type.as_str(),
                pattern.affected_entities.join(",")
            );
            let last_frequency = self
                .last_pattern_notifications
                .get(&signature)
                .copied()
                .unwrap_or(0);
            if notification.frequency <= last_frequency {
                continue;
            }
            self.last_pattern_notifications
                .insert(signature, notification.frequency);

            let managers: Vec<String> = self
                .config
                .members
                .iter()
                .filter(|member| member.role_type == RoleType::Manager)
                .map(|member| member.name.clone())
                .collect();
            let architects: Vec<String> = self
                .config
                .members
                .iter()
                .filter(|member| member.role_type == RoleType::Architect)
                .map(|member| member.name.clone())
                .collect();

            self.emit_event(TeamEvent::pattern_detected(
                notification.pattern_type.as_str(),
                notification.frequency,
            ));

            if notification.notify_manager {
                for recipient in &managers {
                    self.queue_daemon_message(recipient, &notification.message)?;
                }
            }

            if notification.notify_architect {
                for recipient in &architects {
                    self.queue_daemon_message(recipient, &notification.message)?;
                }
            }
        }

        Ok(())
    }

    fn orchestrator_enabled(&self) -> bool {
        self.config.team_config.orchestrator_enabled()
    }

    fn record_orchestrator_action(&self, action: impl AsRef<str>) {
        if !self.orchestrator_enabled() {
            return;
        }
        let path = super::orchestrator_log_path(&self.config.project_root);
        if let Err(error) = append_orchestrator_log_line(&path, action.as_ref()) {
            warn!(log = %path.display(), error = %error, "failed to append orchestrator log");
        }
    }

    fn prepare_member_launch(
        &self,
        member: &MemberInstance,
        resume: bool,
        previous_launch_state: &HashMap<String, LaunchIdentity>,
        duplicate_claude_session_ids: &HashSet<&str>,
    ) -> Result<MemberLaunchPlan> {
        let team_config_dir = self.config.project_root.join(".batty").join("team_config");

        let work_dir = if member.use_worktrees {
            let wt_dir = self
                .config
                .project_root
                .join(".batty")
                .join("worktrees")
                .join(&member.name);
            let branch_name = engineer_base_branch_name(&member.name);
            match setup_engineer_worktree(
                &self.config.project_root,
                &wt_dir,
                &branch_name,
                &team_config_dir,
            ) {
                Ok(path) => path,
                Err(error) => {
                    warn!(
                        member = %member.name,
                        error = %error,
                        "worktree setup failed, using project root"
                    );
                    self.config.project_root.clone()
                }
            }
        } else {
            self.config.project_root.clone()
        };

        let agent_name = member.agent.as_deref().unwrap_or("claude");
        let prompt_text = strip_nudge_section(&self.load_prompt(member, &team_config_dir));
        let idle = role_starts_idle();
        let normalized_agent = canonical_agent_name(agent_name);
        let requested_resume = should_resume_member(
            resume,
            previous_launch_state,
            &member.name,
            &normalized_agent,
            &prompt_text,
        );
        let previous_identity = previous_launch_state.get(&member.name);
        let claude_session_available = previous_identity
            .and_then(|identity| identity.session_id.as_deref())
            .is_none_or(claude_session_id_exists);
        let (member_resume, session_id) = resolve_member_launch_session(
            &normalized_agent,
            previous_identity,
            requested_resume,
            claude_session_available,
            previous_identity
                .and_then(|identity| identity.session_id.as_deref())
                .is_some_and(|existing| duplicate_claude_session_ids.contains(existing)),
        );

        let short_cmd = write_launch_script(
            &member.name,
            agent_name,
            &prompt_text,
            Some(&prompt_text),
            &work_dir,
            &self.config.project_root,
            idle,
            member_resume,
            session_id.as_deref(),
        )?;

        debug!(
            member = %member.name,
            agent = agent_name,
            idle,
            resume_requested = resume,
            member_resume,
            "prepared member launch"
        );

        Ok(MemberLaunchPlan {
            short_cmd,
            identity: LaunchIdentity {
                agent: normalized_agent,
                prompt: prompt_text,
                session_id,
            },
            initial_state: initial_member_state(idle, member_resume),
            activate_watcher: should_activate_watcher_on_spawn(idle, member_resume),
        })
    }

    fn apply_member_launch(
        &mut self,
        member: &MemberInstance,
        pane_id: &str,
        plan: &MemberLaunchPlan,
    ) -> Result<()> {
        if let Some(watcher) = self.watchers.get_mut(&member.name) {
            watcher.set_session_id(plan.identity.session_id.clone());
        }
        tmux::send_keys(pane_id, &plan.short_cmd, true)?;
        self.states.insert(member.name.clone(), plan.initial_state);
        self.update_automation_timers_for_state(&member.name, plan.initial_state);
        if plan.activate_watcher
            && let Some(watcher) = self.watchers.get_mut(&member.name)
        {
            watcher.activate();
        }
        self.emit_event(TeamEvent::agent_spawned(&member.name));
        Ok(())
    }

    fn persist_member_launch_identity(
        &self,
        member_name: &str,
        identity: LaunchIdentity,
    ) -> Result<()> {
        let mut launch_state = load_launch_state(&self.config.project_root);
        launch_state.insert(member_name.to_string(), identity);
        save_launch_state(&self.config.project_root, &launch_state)
    }

    fn restart_dead_members(&mut self) -> Result<()> {
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

    fn restart_member(&mut self, member_name: &str) -> Result<()> {
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
        self.emit_event(TeamEvent::member_crashed(&member.name, true));
        Ok(())
    }

    /// Spawn the correct agent in each member's pane.
    fn spawn_all_agents(&mut self, resume: bool) -> Result<()> {
        let previous_launch_state = load_launch_state(&self.config.project_root);
        let duplicate_claude_session_ids = duplicate_claude_session_ids(&previous_launch_state);
        let mut next_launch_state = HashMap::new();

        // Ensure inboxes exist for all members
        let inboxes = inbox::inboxes_root(&self.config.project_root);
        for member in &self.config.members {
            if let Err(e) = inbox::init_inbox(&inboxes, &member.name) {
                warn!(member = %member.name, error = %e, "failed to init inbox");
            }
        }

        let members = self.config.members.clone();
        for member in &members {
            if member.role_type == RoleType::User {
                continue;
            }

            let Some(pane_id) = self.config.pane_map.get(&member.name).cloned() else {
                warn!(member = %member.name, "no pane found for member");
                continue;
            };
            match self.prepare_member_launch(
                member,
                resume,
                &previous_launch_state,
                &duplicate_claude_session_ids,
            ) {
                Ok(plan) => {
                    if let Err(error) = self.apply_member_launch(member, &pane_id, &plan) {
                        warn!(member = %member.name, error = %error, "failed to launch member");
                        continue;
                    }
                    next_launch_state.insert(member.name.clone(), plan.identity);
                }
                Err(error) => {
                    warn!(
                        member = %member.name,
                        error = %error,
                        "failed to prepare member launch"
                    );
                }
            }
        }

        if let Err(error) = save_launch_state(&self.config.project_root, &next_launch_state) {
            warn!(error = %error, "failed to persist launch state after spawning agents");
        }

        Ok(())
    }

    /// Load the prompt template for a member, substituting role-specific info.
    fn load_prompt(&self, member: &MemberInstance, config_dir: &Path) -> String {
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
    fn poll_watchers(&mut self) -> Result<()> {
        let member_names: Vec<String> = self.watchers.keys().cloned().collect();

        for name in &member_names {
            let prev_state = self.states.get(name).copied();
            let (new_state, completion_observed) = {
                let watcher = self.watchers.get_mut(name).unwrap();
                match watcher.poll() {
                    Ok(new_state) => (new_state, watcher.take_completion_event()),
                    Err(e) => {
                        warn!(member = %name, error = %e, "watcher poll failed");
                        continue;
                    }
                }
            };

            let member_state = match new_state {
                WatcherState::Active => MemberState::Working,
                WatcherState::Idle => MemberState::Idle,
            };

            if prev_state != Some(member_state) {
                self.states.insert(name.clone(), member_state);

                // Update automation countdowns on state transitions.
                self.update_automation_timers_for_state(name, member_state);
            }

            if completion_observed && self.active_task_id(name).is_some() {
                info!(member = %name, "detected task completion");
                if let Err(error) = self.handle_engineer_completion(name) {
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

    fn handle_engineer_completion(&mut self, engineer: &str) -> Result<()> {
        let Some(task_id) = self.active_task_id(engineer) else {
            return Ok(());
        };

        let member = self.config.members.iter().find(|m| m.name == engineer);
        if !member.map(|m| m.use_worktrees).unwrap_or(false) {
            return Ok(());
        }

        let worktree_dir = self
            .config
            .project_root
            .join(".batty")
            .join("worktrees")
            .join(engineer);
        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let board_dir_str = board_dir.to_string_lossy().to_string();
        let manager_name = self
            .config
            .members
            .iter()
            .find(|m| m.name == engineer)
            .and_then(|member| member.reports_to.clone());

        let (tests_passed, output_truncated) = run_tests_in_worktree(&worktree_dir)?;
        if tests_passed {
            let task_title = read_task_title(&board_dir, task_id);
            let _lock = MergeLock::acquire(&self.config.project_root)
                .context("failed to acquire merge lock")?;

            match merge_engineer_branch(&self.config.project_root, engineer)? {
                MergeOutcome::Success => {
                    drop(_lock);

                    let board_update_ok = self.run_kanban_md_nonfatal(
                        &[
                            "move",
                            &task_id.to_string(),
                            "done",
                            "--claim",
                            engineer,
                            "--dir",
                            &board_dir_str,
                        ],
                        &format!("move task #{task_id} to done"),
                        manager_name
                            .as_deref()
                            .into_iter()
                            .chain(std::iter::once(engineer)),
                    );

                    if let Some(ref mgr_name) = manager_name {
                        let msg = format!(
                            "[{engineer}] Task #{task_id} completed.\nTitle: {task_title}\nTests: passed\nMerge: success{}",
                            if board_update_ok {
                                ""
                            } else {
                                "\nBoard: update failed; decide next board action manually."
                            }
                        );
                        self.queue_message(engineer, mgr_name, &msg)?;
                        self.mark_member_working(mgr_name);
                    }

                    if let Some(ref mgr_name) = manager_name {
                        let rollup = format!(
                            "Rollup: Task #{task_id} completed by {engineer}. Tests passed, merged to main.{}",
                            if board_update_ok {
                                ""
                            } else {
                                " Board automation failed; decide manually."
                            }
                        );
                        self.notify_reports_to(mgr_name, &rollup)?;
                    }

                    self.clear_active_task(engineer);
                    self.emit_event(TeamEvent::task_completed(engineer));
                    self.states.insert(engineer.to_string(), MemberState::Idle);
                    if let Some(watcher) = self.watchers.get_mut(engineer) {
                        watcher.deactivate();
                    }
                    self.update_automation_timers_for_state(engineer, MemberState::Idle);
                }
                MergeOutcome::RebaseConflict(conflict_info) => {
                    drop(_lock);

                    let attempt = self.increment_retry(engineer);
                    if attempt <= 2 {
                        let msg = format!(
                            "Merge conflict during rebase onto main (attempt {attempt}/2). Fix the conflicts in your worktree and try again:\n{conflict_info}"
                        );
                        self.queue_message("batty", engineer, &msg)?;
                        self.mark_member_working(engineer);
                        info!(engineer, attempt, "rebase conflict, sending back for retry");
                    } else {
                        if let Some(ref mgr_name) = manager_name {
                            let msg = format!(
                                "[{engineer}] task #{task_id} has unresolvable merge conflicts after 2 retries. Escalating.\n{conflict_info}"
                            );
                            self.queue_message(engineer, mgr_name, &msg)?;
                            self.mark_member_working(mgr_name);
                        }

                        self.emit_event(TeamEvent::task_escalated(engineer, &task_id.to_string()));

                        if let Some(ref mgr_name) = manager_name {
                            let escalation = format!(
                                "ESCALATION: Task #{task_id} assigned to {engineer} has unresolvable merge conflicts. Task blocked on board."
                            );
                            self.notify_reports_to(mgr_name, &escalation)?;
                        }

                        self.run_kanban_md_nonfatal(
                            &[
                                "edit",
                                &task_id.to_string(),
                                "--block",
                                "merge conflicts after 2 retries",
                                "--dir",
                                &board_dir_str,
                            ],
                            &format!("block task #{task_id} after merge conflict retries"),
                            manager_name
                                .as_deref()
                                .into_iter()
                                .chain(std::iter::once(engineer)),
                        );

                        self.clear_active_task(engineer);
                        self.states.insert(engineer.to_string(), MemberState::Idle);
                        if let Some(watcher) = self.watchers.get_mut(engineer) {
                            watcher.deactivate();
                        }
                        self.update_automation_timers_for_state(engineer, MemberState::Idle);
                    }
                }
                MergeOutcome::MergeFailure(merge_info) => {
                    drop(_lock);

                    let manager_notice = format!(
                        "Task #{task_id} from {engineer} passed tests but could not be merged to main.\n{merge_info}\nDecide whether to clean the main worktree, retry the merge, or redirect the engineer."
                    );
                    if let Some(ref mgr_name) = manager_name {
                        self.queue_message("daemon", mgr_name, &manager_notice)?;
                        self.mark_member_working(mgr_name);
                        self.notify_reports_to(mgr_name, &manager_notice)?;
                    }

                    let engineer_notice = format!(
                        "Your task passed tests, but Batty could not merge it into main.\n{merge_info}\nWait for lead direction before making more changes."
                    );
                    self.queue_message("daemon", engineer, &engineer_notice)?;

                    self.emit_event(TeamEvent::task_escalated(engineer, &task_id.to_string()));
                    self.clear_active_task(engineer);
                    self.states.insert(engineer.to_string(), MemberState::Idle);
                    if let Some(watcher) = self.watchers.get_mut(engineer) {
                        watcher.deactivate();
                    }
                    self.update_automation_timers_for_state(engineer, MemberState::Idle);
                    warn!(
                        engineer,
                        task_id,
                        error = %merge_info,
                        "merge into main failed after passing tests; escalated without exiting daemon"
                    );
                }
            }
            return Ok(());
        }

        let attempt = self.increment_retry(engineer);
        if attempt <= 2 {
            let msg = format!(
                "Tests failed (attempt {attempt}/2). Fix the failures and try again:\n{output_truncated}"
            );
            self.queue_message("batty", engineer, &msg)?;
            self.mark_member_working(engineer);
            info!(engineer, attempt, "test failure, sending back for retry");
            return Ok(());
        }

        if let Some(ref mgr_name) = manager_name {
            let msg = format!(
                "[{engineer}] task #{task_id} failed tests after 2 retries. Escalating.\nLast output:\n{output_truncated}"
            );
            self.queue_message(engineer, mgr_name, &msg)?;
            self.mark_member_working(mgr_name);
        }

        self.emit_event(TeamEvent::task_escalated(engineer, &task_id.to_string()));

        if let Some(ref mgr_name) = manager_name {
            let escalation = format!(
                "ESCALATION: Task #{task_id} assigned to {engineer} failed tests after 2 retries. Task blocked on board."
            );
            self.notify_reports_to(mgr_name, &escalation)?;
        }

        self.run_kanban_md_nonfatal(
            &[
                "edit",
                &task_id.to_string(),
                "--block",
                "tests failed after 2 retries",
                "--dir",
                &board_dir_str,
            ],
            &format!("block task #{task_id} after max test retries"),
            manager_name
                .as_deref()
                .into_iter()
                .chain(std::iter::once(engineer)),
        );

        self.clear_active_task(engineer);
        self.states.insert(engineer.to_string(), MemberState::Idle);
        if let Some(watcher) = self.watchers.get_mut(engineer) {
            watcher.deactivate();
        }
        self.update_automation_timers_for_state(engineer, MemberState::Idle);
        info!(engineer, task_id, "escalated to manager after max retries");
        Ok(())
    }

    fn mark_member_working(&mut self, member_name: &str) {
        self.states
            .insert(member_name.to_string(), MemberState::Working);
        if let Some(watcher) = self.watchers.get_mut(member_name) {
            watcher.activate();
        }
        self.update_automation_timers_for_state(member_name, MemberState::Working);
    }

    /// After injecting a message, verify the agent started working.
    ///
    /// Polls the pane for up to `max_attempts` rounds. If the pane still shows
    /// an agent prompt (idle), resends Enter to unstick the submission.
    /// Returns true if the agent transitioned to working, false if still stuck.
    fn verify_message_delivered(&mut self, recipient: &str, max_attempts: u32) -> bool {
        let Some(pane_id) = self.config.pane_map.get(recipient).cloned() else {
            return true; // No pane to verify
        };

        for attempt in 1..=max_attempts {
            // Wait for the agent to start processing
            std::thread::sleep(Duration::from_secs(2));

            // Capture the pane and check state
            let capture = match tmux::capture_pane(&pane_id) {
                Ok(c) => c,
                Err(e) => {
                    warn!(recipient, error = %e, "failed to capture pane for delivery verification");
                    return true; // Can't verify, assume OK
                }
            };

            // If pane shows spinner or interrupt footer, agent is working
            if !super::watcher::is_at_agent_prompt(&capture) {
                debug!(
                    recipient,
                    attempt, "message delivery verified: agent is working"
                );
                return true;
            }

            // Agent is still at prompt — the Enter didn't land. Retry.
            warn!(
                recipient,
                attempt, "agent still at prompt after message injection; resending Enter"
            );
            if let Err(e) = tmux::send_keys(&pane_id, "", true) {
                warn!(recipient, error = %e, "failed to resend Enter");
            }
        }

        // After all attempts, check one final time
        std::thread::sleep(Duration::from_secs(2));
        let capture = tmux::capture_pane(&pane_id).unwrap_or_default();
        let accepted = !super::watcher::is_at_agent_prompt(&capture);
        if !accepted {
            warn!(
                recipient,
                max_attempts, "message may be stuck — agent still at prompt after retries"
            );
        }
        accepted
    }

    fn active_task_id(&self, engineer: &str) -> Option<u32> {
        self.active_tasks.get(engineer).copied()
    }

    fn increment_retry(&mut self, engineer: &str) -> u32 {
        let count = self.retry_counts.entry(engineer.to_string()).or_insert(0);
        *count += 1;
        *count
    }

    fn clear_active_task(&mut self, engineer: &str) {
        self.active_tasks.remove(engineer);
        self.retry_counts.remove(engineer);
    }

    fn run_kanban_md_nonfatal<'a, I>(&mut self, args: &[&str], action: &str, recipients: I) -> bool
    where
        I: IntoIterator<Item = &'a str>,
    {
        match std::process::Command::new("kanban-md").args(args).output() {
            Ok(output) if output.status.success() => true,
            Ok(output) => {
                let detail = describe_command_failure("kanban-md", args, &output);
                self.report_nonfatal_kanban_failure(action, &detail, recipients);
                false
            }
            Err(error) => {
                let detail = format!("failed to execute `kanban-md {}`: {error}", args.join(" "));
                self.report_nonfatal_kanban_failure(action, &detail, recipients);
                false
            }
        }
    }

    fn report_nonfatal_kanban_failure<'a, I>(&mut self, action: &str, detail: &str, recipients: I)
    where
        I: IntoIterator<Item = &'a str>,
    {
        warn!(
            action,
            error = detail,
            "kanban-md command failed; continuing"
        );

        let body = format!(
            "Board automation failed while trying to {action}.\n{detail}\nDecide the next board action manually."
        );
        let mut notified = HashSet::new();
        for recipient in recipients {
            if !notified.insert(recipient.to_string()) {
                continue;
            }
            if let Err(error) = self.queue_daemon_message(recipient, &body) {
                warn!(to = recipient, error = %error, "failed to relay kanban-md failure");
            }
        }
    }

    fn queue_daemon_message(&mut self, recipient: &str, body: &str) -> Result<MessageDelivery> {
        let visible_sender = self.automation_sender_for(recipient);
        self.deliver_message(&visible_sender, recipient, body)
    }

    fn queue_message(&mut self, from: &str, recipient: &str, body: &str) -> Result<()> {
        self.deliver_message(from, recipient, body).map(|_| ())
    }

    fn deliver_message(
        &mut self,
        from: &str,
        recipient: &str,
        body: &str,
    ) -> Result<MessageDelivery> {
        if let Some(channel) = self.channels.get(recipient) {
            channel.send(body)?;
            self.emit_event(TeamEvent::message_routed(from, recipient));
            return Ok(MessageDelivery::Channel);
        }

        let known_recipient = self.config.pane_map.contains_key(recipient)
            || self
                .config
                .members
                .iter()
                .any(|member| member.name == recipient);
        if !known_recipient {
            debug!(from, recipient, "skipping message for unknown recipient");
            return Ok(MessageDelivery::SkippedUnknownRecipient);
        }

        if let Some(pane_id) = self.config.pane_map.get(recipient) {
            match message::inject_message(pane_id, from, body) {
                Ok(()) => {
                    self.emit_event(TeamEvent::message_routed(from, recipient));
                    self.verify_message_delivered(recipient, 3);
                    return Ok(MessageDelivery::LivePane);
                }
                Err(error) => {
                    warn!(
                        from,
                        to = recipient,
                        error = %error,
                        "live message delivery failed; queueing to inbox"
                    );
                }
            }
        }

        let root = inbox::inboxes_root(&self.config.project_root);
        let msg = inbox::InboxMessage::new_send(from, recipient, body);
        inbox::deliver_to_inbox(&root, &msg)?;
        self.emit_event(TeamEvent::message_routed(from, recipient));
        Ok(MessageDelivery::InboxQueued)
    }

    fn notify_assignment_sender_success(
        &mut self,
        sender: &str,
        engineer: &str,
        msg_id: &str,
        task: &str,
        launch: &AssignmentLaunch,
    ) {
        let mut body = format!(
            "Assignment delivered.\nEngineer: {engineer}\nMessage ID: {msg_id}\nTask: {}",
            summarize_assignment(task)
        );
        if let Some(branch) = launch.branch.as_deref() {
            body.push_str(&format!("\nBranch: {branch}"));
        }
        body.push_str(&format!("\nWorktree: {}", launch.work_dir.display()));

        if let Err(error) = self.queue_daemon_message(sender, &body) {
            warn!(to = sender, error = %error, "failed to notify assignment sender");
        }
    }

    fn record_assignment_success(
        &self,
        engineer: &str,
        msg_id: &str,
        task: &str,
        launch: &AssignmentLaunch,
    ) {
        let result = AssignmentDeliveryResult {
            message_id: msg_id.to_string(),
            status: AssignmentResultStatus::Delivered,
            engineer: engineer.to_string(),
            task_summary: summarize_assignment(task),
            branch: launch.branch.clone(),
            work_dir: Some(launch.work_dir.display().to_string()),
            detail: "assignment launched".to_string(),
            ts: now_unix(),
        };
        if let Err(error) = store_assignment_result(&self.config.project_root, &result) {
            warn!(id = msg_id, error = %error, "failed to record assignment success");
        }
    }

    fn notify_assignment_sender_failure(
        &mut self,
        sender: &str,
        engineer: &str,
        msg_id: &str,
        task: &str,
        error: &anyhow::Error,
    ) {
        let body = format!(
            "Assignment failed.\nEngineer: {engineer}\nMessage ID: {msg_id}\nTask: {}\nReason: {error}",
            summarize_assignment(task)
        );

        if let Err(notify_error) = self.queue_daemon_message(sender, &body) {
            warn!(
                to = sender,
                error = %notify_error,
                "failed to notify assignment sender of failure"
            );
        }
    }

    fn record_assignment_failure(
        &self,
        engineer: &str,
        msg_id: &str,
        task: &str,
        error: &anyhow::Error,
    ) {
        let work_dir = self
            .config
            .project_root
            .join(".batty")
            .join("worktrees")
            .join(engineer);
        let result = AssignmentDeliveryResult {
            message_id: msg_id.to_string(),
            status: AssignmentResultStatus::Failed,
            engineer: engineer.to_string(),
            task_summary: summarize_assignment(task),
            branch: None,
            work_dir: Some(work_dir.display().to_string()),
            detail: error.to_string(),
            ts: now_unix(),
        };
        if let Err(store_error) = store_assignment_result(&self.config.project_root, &result) {
            warn!(id = msg_id, error = %store_error, "failed to record assignment failure");
        }
    }

    fn notify_reports_to(&mut self, from_role: &str, msg: &str) -> Result<()> {
        let parent = self
            .config
            .members
            .iter()
            .find(|m| m.name == from_role)
            .and_then(|m| m.reports_to.clone());
        let Some(parent_name) = parent else {
            return Ok(());
        };
        self.queue_message(from_role, &parent_name, msg)?;
        self.mark_member_working(&parent_name);
        Ok(())
    }

    /// Drain the legacy `commands.jsonl` queue into Maildir inboxes.
    ///
    /// This provides backward compatibility during migration. Commands written
    /// to the old queue file are converted to inbox messages and delivered.
    fn drain_legacy_command_queue(&mut self) -> Result<()> {
        let queue_path = message::command_queue_path(&self.config.project_root);
        let commands = message::read_command_queue(&queue_path)?;
        if commands.is_empty() {
            return Ok(());
        }

        let root = inbox::inboxes_root(&self.config.project_root);
        let mut remaining_commands = Vec::new();
        for cmd in commands {
            let result: Result<()> = (|| match &cmd {
                message::QueuedCommand::Send {
                    from,
                    to,
                    message: msg,
                } => {
                    // Route user-role messages via channel, others to inbox
                    let is_user = self
                        .config
                        .team_config
                        .roles
                        .iter()
                        .any(|r| r.name == to.as_str() && r.role_type == RoleType::User);

                    if is_user {
                        if let Some(channel) = self.channels.get(to.as_str()) {
                            let formatted = format!("[From {from}]\n{msg}");
                            channel.send(&formatted)?;
                        }
                        self.emit_event(TeamEvent::message_routed(from, to));
                    } else {
                        let inbox_msg = inbox::InboxMessage::new_send(from, to, msg);
                        inbox::deliver_to_inbox(&root, &inbox_msg)?;
                        debug!(from, to, "legacy command routed to inbox");
                    }
                    Ok(())
                }
                message::QueuedCommand::Assign {
                    from,
                    engineer,
                    task,
                } => {
                    let msg = inbox::InboxMessage::new_assign(from, engineer, task);
                    inbox::deliver_to_inbox(&root, &msg)?;
                    debug!(engineer, "legacy assign routed to inbox");
                    Ok(())
                }
            })();

            if let Err(error) = result {
                warn!(error = %error, "failed to process legacy command; preserving in queue");
                remaining_commands.push(cmd);
            }
        }

        message::write_command_queue(&queue_path, &remaining_commands)?;

        Ok(())
    }

    /// Deliver inbox messages to agents that are at their prompt.
    ///
    /// For each member with a pane, check their `new/` inbox. If the agent's
    /// watcher state is Idle, inject the message via tmux and move it to
    /// `cur/`. If the agent is actively working, messages stay in `new/`
    /// and survive daemon restarts.
    fn deliver_inbox_messages(&mut self) -> Result<()> {
        let root = inbox::inboxes_root(&self.config.project_root);

        let member_names: Vec<String> = self.config.pane_map.keys().cloned().collect();

        for name in &member_names {
            let is_ready = self
                .watchers
                .get(name)
                .map(|w| matches!(w.state, WatcherState::Idle))
                .unwrap_or(true);

            if !is_ready {
                continue;
            }

            let messages = match inbox::pending_messages(&root, name) {
                Ok(msgs) => msgs,
                Err(e) => {
                    debug!(member = %name, error = %e, "failed to read inbox");
                    continue;
                }
            };

            if messages.is_empty() {
                continue;
            }

            let Some(pane_id) = self.config.pane_map.get(name).cloned() else {
                continue;
            };

            let mut delivered_any = false;
            for msg in &messages {
                // Enforce routing rules: resolve sender/recipient role names
                let from_role = self.resolve_role_name(&msg.from);
                let to_role = self.resolve_role_name(name);
                if !self.config.team_config.can_talk(&from_role, &to_role) {
                    warn!(
                        from = %msg.from, from_role, to = %name, to_role,
                        "blocked message: routing not allowed"
                    );
                    // Still mark as delivered so it doesn't retry forever
                    let _ = inbox::mark_delivered(&root, name, &msg.id);
                    continue;
                }

                let is_send = matches!(msg.msg_type, inbox::MessageType::Send);
                let delivery_result = match msg.msg_type {
                    inbox::MessageType::Send => {
                        info!(from = %msg.from, to = %name, id = %msg.id, "delivering inbox message");
                        message::inject_message(&pane_id, &msg.from, &msg.body)
                    }
                    inbox::MessageType::Assign => {
                        info!(to = %name, id = %msg.id, "delivering inbox assignment");
                        self.assign_task(name, &msg.body).map(|launch| {
                            self.record_assignment_success(name, &msg.id, &msg.body, &launch);
                            self.notify_assignment_sender_success(
                                &msg.from, name, &msg.id, &msg.body, &launch,
                            );
                        })
                    }
                };

                let mut mark_delivered = false;
                match delivery_result {
                    Ok(()) => {
                        delivered_any = true;
                        mark_delivered = true;
                        if is_send {
                            self.verify_message_delivered(name, 3);
                        }
                    }
                    Err(error) => {
                        warn!(
                            from = %msg.from,
                            to = %name,
                            id = %msg.id,
                            error = %error,
                            "failed to deliver inbox message"
                        );
                        if matches!(msg.msg_type, inbox::MessageType::Assign) {
                            mark_delivered = true;
                            self.record_assignment_failure(name, &msg.id, &msg.body, &error);
                            self.notify_assignment_sender_failure(
                                &msg.from, name, &msg.id, &msg.body, &error,
                            );
                        }
                    }
                }

                if !mark_delivered {
                    continue;
                }

                // Mark as delivered (move new/ → cur/)
                if let Err(error) = inbox::mark_delivered(&root, name, &msg.id) {
                    warn!(
                        member = %name,
                        id = %msg.id,
                        error = %error,
                        "failed to mark delivered"
                    );
                } else {
                    self.emit_event(TeamEvent::message_routed(&msg.from, name));
                }

                // Small delay between multiple messages
                std::thread::sleep(Duration::from_secs(1));
            }

            // Re-activate watcher after delivering messages
            if delivered_any {
                self.mark_member_working(name);
            }
        }

        Ok(())
    }

    /// Assign a task to an engineer: reset context, inject new prompt.
    fn assign_task(&mut self, engineer: &str, task: &str) -> Result<AssignmentLaunch> {
        self.assign_task_with_task_id(engineer, task, None)
    }

    fn assign_task_with_task_id(
        &mut self,
        engineer: &str,
        task: &str,
        task_id: Option<u32>,
    ) -> Result<AssignmentLaunch> {
        info!(engineer, task, "assigning task");

        let Some(pane_id) = self.config.pane_map.get(engineer).cloned() else {
            bail!("no pane found for engineer '{engineer}'");
        };

        // Find member to determine agent type
        let member = self.config.members.iter().find(|m| m.name == engineer);

        let agent_name = member.and_then(|m| m.agent.as_deref()).unwrap_or("claude");

        let team_config_dir = self.config.project_root.join(".batty").join("team_config");
        let use_worktrees = member.map(|m| m.use_worktrees).unwrap_or(false);
        let task_branch = use_worktrees.then(|| engineer_task_branch_name(engineer, task, task_id));
        let work_dir = if let Some(task_branch) = task_branch.as_deref() {
            let work_dir = self
                .config
                .project_root
                .join(".batty")
                .join("worktrees")
                .join(engineer);
            prepare_engineer_assignment_worktree(
                &self.config.project_root,
                &work_dir,
                engineer,
                task_branch,
                &team_config_dir,
            )?
        } else {
            self.config.project_root.clone()
        };

        // Reset agent context after the new worktree branch is ready.
        let adapter = agent::adapter_from_name(agent_name);
        if let Some(adapter) = &adapter {
            for (keys, enter) in adapter.reset_context_keys() {
                tmux::send_keys(&pane_id, &keys, enter)?;
                std::thread::sleep(Duration::from_millis(500));
            }
        }

        let role_context =
            member.map(|m| strip_nudge_section(&self.load_prompt(m, &team_config_dir)));
        let normalized_agent = canonical_agent_name(agent_name);
        let session_id = new_member_session_id(&normalized_agent);

        // Wait for agent to reset, then launch with new task (never resume for assignments)
        std::thread::sleep(Duration::from_secs(1));
        let short_cmd = write_launch_script(
            engineer,
            agent_name,
            task,
            role_context.as_deref(),
            &work_dir,
            &self.config.project_root,
            false,
            false,
            session_id.as_deref(),
        )?;
        if let Some(watcher) = self.watchers.get_mut(engineer) {
            watcher.set_session_id(session_id.clone());
        }
        tmux::send_keys(&pane_id, &short_cmd, true)?;
        if let Some(session_id) = session_id.as_deref() {
            self.persist_member_session_id(engineer, session_id)?;
        }

        // Update state
        self.mark_member_working(engineer);

        self.emit_event(TeamEvent::task_assigned(engineer, task));

        Ok(AssignmentLaunch {
            branch: task_branch,
            work_dir,
        })
    }

    fn idle_engineer_names(&self) -> Vec<String> {
        self.config
            .members
            .iter()
            .filter(|member| member.role_type == RoleType::Engineer)
            .filter(|member| self.states.get(&member.name) == Some(&MemberState::Idle))
            .map(|member| member.name.clone())
            .collect()
    }

    fn auto_dispatch(&mut self) -> Result<()> {
        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let board_dir_str = board_dir.to_string_lossy().to_string();

        for engineer_name in self.idle_engineer_names() {
            let Some(task) = next_unclaimed_task(&board_dir)? else {
                break;
            };

            let board_failure_recipients: Vec<String> = self
                .config
                .members
                .iter()
                .filter(|member| {
                    matches!(member.role_type, RoleType::Architect | RoleType::Manager)
                })
                .map(|member| member.name.clone())
                .collect();
            if !self.run_kanban_md_nonfatal(
                &[
                    "pick",
                    "--claim",
                    &engineer_name,
                    "--move",
                    "in-progress",
                    "--dir",
                    &board_dir_str,
                ],
                &format!("pick the next task for {engineer_name}"),
                board_failure_recipients.iter().map(String::as_str),
            ) {
                break;
            }

            let assignment_message =
                format!("Task #{}: {}\n\n{}", task.id, task.title, task.description);
            self.assign_task_with_task_id(&engineer_name, &assignment_message, Some(task.id))?;
            self.active_tasks.insert(engineer_name.clone(), task.id);
            self.retry_counts.remove(&engineer_name);
            self.record_orchestrator_action(format!(
                "dependency resolution: selected runnable task #{} ({}) and dispatched it to {}",
                task.id, task.title, engineer_name
            ));
            info!(
                engineer = %engineer_name,
                task_id = task.id,
                task_title = %task.title,
                "auto-dispatched task"
            );
        }

        Ok(())
    }

    fn maybe_auto_dispatch(&mut self) -> Result<()> {
        if !self.config.team_config.board.auto_dispatch {
            return Ok(());
        }

        if self.last_auto_dispatch.elapsed() < Duration::from_secs(10) {
            return Ok(());
        }

        if let Err(e) = self.auto_dispatch() {
            warn!(error = %e, "auto-dispatch failed");
        }
        self.last_auto_dispatch = Instant::now();
        Ok(())
    }

    /// Update `@batty_status` on each pane border with state + timer countdowns.
    fn update_pane_status_labels(&self) {
        let globally_paused = super::pause_marker_path(&self.config.project_root).exists();
        let inbox_root = inbox::inboxes_root(&self.config.project_root);
        let direct_reports = super::direct_reports_by_member(&self.config.members);
        let owned_task_buckets =
            super::owned_task_buckets(&self.config.project_root, &self.config.members);

        for member in &self.config.members {
            if member.role_type == RoleType::User {
                continue;
            }
            let Some(pane_id) = self.config.pane_map.get(&member.name) else {
                continue;
            };

            let state = self
                .states
                .get(&member.name)
                .copied()
                .unwrap_or(MemberState::Idle);

            let pending_inbox = match inbox::pending_message_count(&inbox_root, &member.name) {
                Ok(count) => count,
                Err(error) => {
                    warn!(member = %member.name, error = %error, "failed to count pending inbox messages");
                    0
                }
            };
            let triage_backlog = match direct_reports.get(&member.name) {
                Some(reports) => {
                    match super::delivered_direct_report_triage_count(
                        &inbox_root,
                        &member.name,
                        reports,
                    ) {
                        Ok(count) => count,
                        Err(error) => {
                            warn!(member = %member.name, error = %error, "failed to compute triage backlog");
                            0
                        }
                    }
                }
                None => 0,
            };
            let member_owned_tasks = owned_task_buckets
                .get(&member.name)
                .cloned()
                .unwrap_or_default();

            let label = if globally_paused {
                compose_pane_status_label(
                    state,
                    pending_inbox,
                    triage_backlog,
                    &member_owned_tasks.active,
                    &member_owned_tasks.review,
                    true,
                    "",
                    "",
                )
            } else {
                let nudge_str = format_nudge_status(self.nudges.get(&member.name));
                let standup_str = self
                    .standup_interval_for_member_name(&member.name)
                    .map(|standup_interval| {
                        format_standup_status(
                            self.last_standup.get(&member.name).copied(),
                            standup_interval,
                            self.paused_standups.contains(&member.name),
                        )
                    })
                    .unwrap_or_default();
                compose_pane_status_label(
                    state,
                    pending_inbox,
                    triage_backlog,
                    &member_owned_tasks.active,
                    &member_owned_tasks.review,
                    false,
                    &nudge_str,
                    &standup_str,
                )
            };

            let _ = std::process::Command::new("tmux")
                .args(["set-option", "-p", "-t", pane_id, "@batty_status", &label])
                .output();
        }
    }

    /// Update automation countdowns when a member's state changes.
    fn update_automation_timers_for_state(&mut self, member_name: &str, new_state: MemberState) {
        match new_state {
            MemberState::Idle => {
                self.idle_started_at
                    .insert(member_name.to_string(), Instant::now());
            }
            MemberState::Working => {
                self.idle_started_at.remove(member_name);
            }
        }
        self.update_nudge_for_state(member_name, new_state);
        self.update_standup_for_state(member_name, new_state);
        self.update_triage_intervention_for_state(member_name, new_state);
    }

    /// Update the nudge countdown when a member's state changes.
    ///
    /// - Transition to idle: start the countdown if needed.
    /// - Transition to working: pause the countdown and restart it on next idle.
    fn update_nudge_for_state(&mut self, member_name: &str, new_state: MemberState) {
        if let Some(schedule) = self.nudges.get_mut(member_name) {
            match new_state {
                MemberState::Idle => {
                    if schedule.paused || schedule.idle_since.is_none() {
                        schedule.idle_since = Some(Instant::now());
                        schedule.fired_this_idle = false;
                    }
                    schedule.paused = false;
                }
                MemberState::Working => {
                    schedule.idle_since = None;
                    schedule.fired_this_idle = false;
                    schedule.paused = true;
                }
            }
        }
    }

    /// Update the standup countdown when a member's state changes.
    ///
    /// Standups are intended to wake up idle members, not interrupt active work.
    /// When a member starts working, pause the standup timer and require a fresh
    /// idle period before the next standup countdown begins.
    fn update_standup_for_state(&mut self, member_name: &str, new_state: MemberState) {
        if self.standup_interval_for_member_name(member_name).is_none() {
            self.paused_standups.remove(member_name);
            self.last_standup.remove(member_name);
            return;
        }

        match new_state {
            MemberState::Working => {
                self.paused_standups.insert(member_name.to_string());
                self.last_standup.remove(member_name);
            }
            MemberState::Idle => {
                let was_paused = self.paused_standups.remove(member_name);
                if was_paused || !self.last_standup.contains_key(member_name) {
                    self.last_standup
                        .insert(member_name.to_string(), Instant::now());
                }
            }
        }
    }

    /// Track idle epochs so triage interventions fire once per post-work idle period.
    ///
    /// Startup idle does not count as an intervention-worthy idle period. A member must
    /// first enter working state, then become idle again to arm a triage intervention.
    fn update_triage_intervention_for_state(&mut self, member_name: &str, new_state: MemberState) {
        match new_state {
            MemberState::Working => {
                self.triage_idle_epochs
                    .entry(member_name.to_string())
                    .or_insert(0);
            }
            MemberState::Idle => {
                let had_epoch = self.triage_idle_epochs.contains_key(member_name);
                let epoch = self
                    .triage_idle_epochs
                    .entry(member_name.to_string())
                    .or_insert(0);
                if had_epoch {
                    *epoch += 1;
                }
            }
        }
    }

    fn standup_interval_for_member_name(&self, member_name: &str) -> Option<Duration> {
        let member = self.config.members.iter().find(|m| m.name == member_name)?;
        let role_def = self
            .config
            .team_config
            .roles
            .iter()
            .find(|r| r.name == member.role_name);

        let receives = role_def
            .and_then(|r| r.receives_standup)
            .unwrap_or(matches!(
                member.role_type,
                RoleType::Manager | RoleType::Architect
            ));
        if !receives {
            return None;
        }

        let interval_secs = role_def
            .and_then(|r| r.standup_interval_secs)
            .unwrap_or(self.config.team_config.standup.interval_secs);
        Some(Duration::from_secs(interval_secs))
    }

    fn is_member_idle(&self, member_name: &str) -> bool {
        self.watchers
            .get(member_name)
            .map(|watcher| matches!(watcher.state, WatcherState::Idle))
            .unwrap_or(matches!(
                self.states.get(member_name),
                Some(MemberState::Idle) | None
            ))
    }

    fn manager_for_member_name(&self, member_name: &str) -> Option<&str> {
        self.config
            .members
            .iter()
            .find(|member| member.name == member_name)
            .and_then(|member| member.reports_to.as_deref())
    }

    fn automation_idle_grace_duration(&self) -> Duration {
        Duration::from_secs(
            self.config
                .team_config
                .automation
                .intervention_idle_grace_secs,
        )
    }

    fn automation_idle_grace_elapsed(&self, member_name: &str) -> bool {
        let grace = self.automation_idle_grace_duration();
        self.idle_started_at
            .get(member_name)
            .is_some_and(|started_at| started_at.elapsed() >= grace)
    }

    fn member_has_pending_inbox(&self, inbox_root: &Path, member_name: &str) -> bool {
        match inbox::pending_message_count(inbox_root, member_name) {
            Ok(count) => count > 0,
            Err(error) => {
                warn!(member = %member_name, error = %error, "failed to count pending inbox before automation");
                true
            }
        }
    }

    fn ready_for_idle_automation(&self, inbox_root: &Path, member_name: &str) -> bool {
        self.automation_idle_grace_elapsed(member_name)
            && !self.member_has_pending_inbox(inbox_root, member_name)
    }

    fn sync_launch_state_session_ids(&self) -> Result<()> {
        let mut launch_state = load_launch_state(&self.config.project_root);
        let mut changed = false;

        for (member_name, watcher) in &self.watchers {
            let Some(session_id) = watcher.current_session_id() else {
                continue;
            };
            let Some(entry) = launch_state.get_mut(member_name) else {
                continue;
            };
            if entry.session_id.as_deref() == Some(session_id.as_str()) {
                continue;
            }
            entry.session_id = Some(session_id);
            changed = true;
        }

        if changed {
            save_launch_state(&self.config.project_root, &launch_state)?;
        }

        Ok(())
    }

    fn persist_member_session_id(&self, member_name: &str, session_id: &str) -> Result<()> {
        let mut launch_state = load_launch_state(&self.config.project_root);
        let Some(entry) = launch_state.get_mut(member_name) else {
            return Ok(());
        };
        if entry.session_id.as_deref() == Some(session_id) {
            return Ok(());
        }
        entry.session_id = Some(session_id.to_string());
        save_launch_state(&self.config.project_root, &launch_state)
    }

    fn restore_runtime_state(&mut self) {
        let Some(state) = load_daemon_state(&self.config.project_root) else {
            return;
        };

        self.states = state.states;
        self.idle_started_at = self
            .states
            .iter()
            .filter_map(|(member, state)| {
                matches!(state, MemberState::Idle).then(|| (member.clone(), Instant::now()))
            })
            .collect();
        self.active_tasks = state.active_tasks;
        self.retry_counts = state.retry_counts;
        self.paused_standups = state.paused_standups;
        self.last_standup = state
            .last_standup_elapsed_secs
            .into_iter()
            .map(|(member, elapsed_secs)| {
                (
                    member,
                    Instant::now()
                        .checked_sub(Duration::from_secs(elapsed_secs))
                        .unwrap_or_else(Instant::now),
                )
            })
            .collect();

        for (member_name, persisted) in state.nudge_state {
            let Some(schedule) = self.nudges.get_mut(&member_name) else {
                continue;
            };
            schedule.idle_since = persisted.idle_elapsed_secs.map(|elapsed_secs| {
                Instant::now()
                    .checked_sub(Duration::from_secs(elapsed_secs))
                    .unwrap_or_else(Instant::now)
            });
            schedule.fired_this_idle = persisted.fired_this_idle;
            schedule.paused = persisted.paused;
        }
    }

    fn persist_runtime_state(&self, clean_shutdown: bool) -> Result<()> {
        let state = PersistedDaemonState {
            clean_shutdown,
            saved_at: now_unix(),
            states: self.states.clone(),
            active_tasks: self.active_tasks.clone(),
            retry_counts: self.retry_counts.clone(),
            paused_standups: self.paused_standups.clone(),
            last_standup_elapsed_secs: self
                .last_standup
                .iter()
                .map(|(member, instant)| (member.clone(), instant.elapsed().as_secs()))
                .collect(),
            nudge_state: self
                .nudges
                .iter()
                .map(|(member, schedule)| {
                    (
                        member.clone(),
                        PersistedNudgeState {
                            idle_elapsed_secs: schedule.idle_since.map(|t| t.elapsed().as_secs()),
                            fired_this_idle: schedule.fired_this_idle,
                            paused: schedule.paused,
                        },
                    )
                })
                .collect(),
        };
        save_daemon_state(&self.config.project_root, &state)
    }

    /// Fire nudges for members that have been idle long enough.
    ///
    /// The nudge only fires once per idle period. The member must transition
    /// back to working and then to idle again before a new nudge can fire.
    /// Skipped entirely when the pause marker file exists.
    fn maybe_fire_nudges(&mut self) -> Result<()> {
        if !self.config.team_config.automation.timeout_nudges {
            return Ok(());
        }
        if super::pause_marker_path(&self.config.project_root).exists() {
            return Ok(());
        }
        let inbox_root = inbox::inboxes_root(&self.config.project_root);
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
                    false // currently working — no nudge
                }
            };

            if fire {
                let text = self.nudges[&name].text.clone();
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

    fn maybe_intervene_triage_backlog(&mut self) -> Result<()> {
        if !self.config.team_config.automation.triage_interventions {
            return Ok(());
        }
        if super::pause_marker_path(&self.config.project_root).exists() {
            return Ok(());
        }

        let inbox_root = inbox::inboxes_root(&self.config.project_root);
        let direct_reports = super::direct_reports_by_member(&self.config.members);
        let member_names: Vec<String> = self.config.pane_map.keys().cloned().collect();

        for name in member_names {
            let Some(member) = self
                .config
                .members
                .iter()
                .find(|member| member.name == name)
            else {
                continue;
            };
            let is_idle = self
                .watchers
                .get(&name)
                .map(|w| matches!(w.state, WatcherState::Idle))
                .unwrap_or(matches!(
                    self.states.get(&name),
                    Some(MemberState::Idle) | None
                ));
            if !is_idle {
                continue;
            }
            if !self.ready_for_idle_automation(&inbox_root, &name) {
                continue;
            }

            let Some(reports) = direct_reports.get(&name) else {
                continue;
            };

            let triage_state = match super::delivered_direct_report_triage_state(
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

            let text = self.build_triage_intervention_message(member, reports, triage_state.count);
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
            if delivered_live {
                self.mark_member_working(&name);
            }
        }

        Ok(())
    }

    fn maybe_intervene_owned_tasks(&mut self) -> Result<()> {
        if !self.config.team_config.automation.owned_task_interventions {
            return Ok(());
        }
        if super::pause_marker_path(&self.config.project_root).exists() {
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
        let direct_reports = super::direct_reports_by_member(&self.config.members);
        let member_names: Vec<String> = self.config.pane_map.keys().cloned().collect();

        for name in member_names {
            let Some(member) = self
                .config
                .members
                .iter()
                .find(|member| member.name == name)
            else {
                continue;
            };
            let is_idle = self
                .watchers
                .get(&name)
                .map(|w| matches!(w.state, WatcherState::Idle))
                .unwrap_or(matches!(
                    self.states.get(&name),
                    Some(MemberState::Idle) | None
                ));
            if !is_idle {
                continue;
            }
            let owned_tasks: Vec<&crate::task::Task> = tasks
                .iter()
                .filter(|task| task.claimed_by.as_deref() == Some(name.as_str()))
                .filter(|task| task_needs_owned_intervention(task.status.as_str()))
                .collect();
            if owned_tasks.is_empty() {
                self.owned_task_interventions.remove(&name);
                continue;
            }

            let idle_epoch = self.triage_idle_epochs.get(&name).copied().unwrap_or(0);

            let signature = owned_task_intervention_signature(&owned_tasks);
            if let Some(state) = self.owned_task_interventions.get_mut(&name) {
                if state.signature == signature {
                    state.idle_epoch = idle_epoch;
                    continue;
                }
            }

            if !self.ready_for_idle_automation(&inbox_root, &name) {
                continue;
            }

            let reports = direct_reports.get(&name).cloned().unwrap_or_default();
            let text = self.build_owned_task_intervention_message(member, &owned_tasks, &reports);
            info!(
                member = %name,
                owned_task_count = owned_tasks.len(),
                "firing owned-task intervention"
            );
            let delivered_live = match self.queue_daemon_message(&name, &text) {
                Ok(MessageDelivery::LivePane) => true,
                Ok(_) => false,
                Err(error) => {
                    warn!(member = %name, error = %error, "failed to deliver owned-task intervention");
                    continue;
                }
            };
            self.record_orchestrator_action(format!(
                "recovery: owned-task intervention for {} covering {} active task(s)",
                name,
                owned_tasks.len()
            ));
            self.owned_task_interventions.insert(
                name.clone(),
                OwnedTaskInterventionState {
                    idle_epoch,
                    signature,
                },
            );
            if delivered_live {
                self.mark_member_working(&name);
            }
        }

        Ok(())
    }

    fn maybe_intervene_review_backlog(&mut self) -> Result<()> {
        if !self.config.team_config.automation.review_interventions {
            return Ok(());
        }
        if super::pause_marker_path(&self.config.project_root).exists() {
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
        let member_names: Vec<String> = self.config.pane_map.keys().cloned().collect();

        for name in member_names {
            let Some(member) = self
                .config
                .members
                .iter()
                .find(|member| member.name == name)
            else {
                continue;
            };
            let is_idle = self
                .watchers
                .get(&name)
                .map(|w| matches!(w.state, WatcherState::Idle))
                .unwrap_or(matches!(
                    self.states.get(&name),
                    Some(MemberState::Idle) | None
                ));
            if !is_idle {
                continue;
            }
            if !self.ready_for_idle_automation(&inbox_root, &name) {
                continue;
            }

            let review_tasks: Vec<&crate::task::Task> = tasks
                .iter()
                .filter(|task| {
                    review_backlog_owner_for_task(task, &self.config.members).as_deref()
                        == Some(name.as_str())
                })
                .collect();
            if review_tasks.is_empty() {
                self.owned_task_interventions
                    .remove(&review_intervention_key(&name));
                continue;
            }

            let idle_epoch = self.triage_idle_epochs.get(&name).copied().unwrap_or(0);
            if idle_epoch == 0 {
                continue;
            }

            let signature = review_task_intervention_signature(&review_tasks);
            let review_key = review_intervention_key(&name);
            if self
                .owned_task_interventions
                .get(&review_key)
                .is_some_and(|state| state.signature == signature)
            {
                continue;
            }

            let text = self.build_review_intervention_message(member, &review_tasks);
            info!(
                member = %name,
                review_task_count = review_tasks.len(),
                "firing review intervention"
            );
            let delivered_live = match self.queue_daemon_message(&name, &text) {
                Ok(MessageDelivery::LivePane) => true,
                Ok(_) => false,
                Err(error) => {
                    warn!(member = %name, error = %error, "failed to deliver review intervention");
                    continue;
                }
            };
            self.record_orchestrator_action(format!(
                "recovery: review intervention for {} covering {} queued review task(s)",
                name,
                review_tasks.len()
            ));
            self.owned_task_interventions.insert(
                review_key,
                OwnedTaskInterventionState {
                    idle_epoch,
                    signature,
                },
            );
            if delivered_live {
                self.mark_member_working(&name);
            }
        }

        Ok(())
    }

    fn maybe_intervene_manager_dispatch_gap(&mut self) -> Result<()> {
        if !self
            .config
            .team_config
            .automation
            .manager_dispatch_interventions
        {
            return Ok(());
        }
        if super::pause_marker_path(&self.config.project_root).exists() {
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
        let direct_reports = super::direct_reports_by_member(&self.config.members);
        let member_names: Vec<String> = self.config.pane_map.keys().cloned().collect();

        for name in member_names {
            let Some(member) = self
                .config
                .members
                .iter()
                .find(|member| member.name == name)
            else {
                continue;
            };
            if member.role_type != RoleType::Manager {
                continue;
            }
            if !self.is_member_idle(&name) {
                continue;
            }
            if !self.ready_for_idle_automation(&inbox_root, &name) {
                continue;
            }

            let Some(reports) = direct_reports.get(&name) else {
                continue;
            };
            if reports.is_empty() {
                continue;
            }

            let triage_state =
                super::delivered_direct_report_triage_state(&inbox_root, &name, reports)?;
            if triage_state.count > 0 {
                continue;
            }

            let review_count = tasks
                .iter()
                .filter(|task| {
                    review_backlog_owner_for_task(task, &self.config.members).as_deref()
                        == Some(name.as_str())
                })
                .count();
            if review_count > 0 {
                continue;
            }

            let report_snapshots: Vec<ReportDispatchSnapshot> = reports
                .iter()
                .map(|report| ReportDispatchSnapshot {
                    name: report.clone(),
                    is_working: !self.is_member_idle(report),
                    active_task_ids: tasks
                        .iter()
                        .filter(|task| task.claimed_by.as_deref() == Some(report.as_str()))
                        .filter(|task| task_needs_owned_intervention(task.status.as_str()))
                        .map(|task| task.id)
                        .collect(),
                })
                .collect();

            if report_snapshots.iter().any(|snapshot| snapshot.is_working) {
                continue;
            }

            let idle_active_reports: Vec<&ReportDispatchSnapshot> = report_snapshots
                .iter()
                .filter(|snapshot| !snapshot.active_task_ids.is_empty())
                .collect();
            let idle_unassigned_reports: Vec<&ReportDispatchSnapshot> = report_snapshots
                .iter()
                .filter(|snapshot| snapshot.active_task_ids.is_empty())
                .collect();

            let unassigned_open_tasks: Vec<&crate::task::Task> = tasks
                .iter()
                .filter(|task| task.claimed_by.is_none())
                .filter(|task| matches!(task.status.as_str(), "backlog" | "todo"))
                .collect();

            if idle_active_reports.is_empty() && unassigned_open_tasks.is_empty() {
                continue;
            }

            let dispatch_key = manager_dispatch_intervention_key(&name);
            let signature = manager_dispatch_intervention_signature(
                &idle_active_reports,
                &idle_unassigned_reports,
                &unassigned_open_tasks,
            );
            if self
                .owned_task_interventions
                .get(&dispatch_key)
                .is_some_and(|state| state.signature == signature)
            {
                continue;
            }

            let text = self.build_manager_dispatch_gap_message(
                member,
                &idle_active_reports,
                &idle_unassigned_reports,
                &unassigned_open_tasks,
            );
            info!(
                member = %name,
                idle_active_reports = idle_active_reports.len(),
                idle_unassigned_reports = idle_unassigned_reports.len(),
                unassigned_open_tasks = unassigned_open_tasks.len(),
                "firing manager dispatch-gap intervention"
            );
            let delivered_live = match self.queue_daemon_message(&name, &text) {
                Ok(MessageDelivery::LivePane) => true,
                Ok(_) => false,
                Err(error) => {
                    warn!(member = %name, error = %error, "failed to deliver manager dispatch-gap intervention");
                    continue;
                }
            };
            self.record_orchestrator_action(format!(
                "recovery: dispatch-gap intervention for {} (idle reports with active work: {}, unassigned reports: {}, open tasks: {})",
                name,
                idle_active_reports.len(),
                idle_unassigned_reports.len(),
                unassigned_open_tasks.len()
            ));
            let idle_epoch = self.triage_idle_epochs.get(&name).copied().unwrap_or(0);
            self.owned_task_interventions.insert(
                dispatch_key,
                OwnedTaskInterventionState {
                    idle_epoch,
                    signature,
                },
            );
            if delivered_live {
                self.mark_member_working(&name);
            }
        }

        Ok(())
    }

    fn maybe_intervene_architect_utilization(&mut self) -> Result<()> {
        if !self
            .config
            .team_config
            .automation
            .architect_utilization_interventions
        {
            return Ok(());
        }
        if super::pause_marker_path(&self.config.project_root).exists() {
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
        let direct_reports = super::direct_reports_by_member(&self.config.members);
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
        let unassigned_open_tasks: Vec<&crate::task::Task> = tasks
            .iter()
            .filter(|task| task.claimed_by.is_none())
            .filter(|task| matches!(task.status.as_str(), "backlog" | "todo"))
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

        for architect in &architect_members {
            if !self.is_member_idle(&architect.name) {
                continue;
            }
            if !self.ready_for_idle_automation(&inbox_root, &architect.name) {
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

            let text = self.build_architect_utilization_message(
                architect,
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
                utilization_key,
                OwnedTaskInterventionState {
                    idle_epoch,
                    signature,
                },
            );
            if delivered_live {
                self.mark_member_working(&architect.name);
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
            "Triage backlog detected: you have {triage_count} delivered direct-report result packet(s) waiting for review. Reports in scope: {report_list}.\n\
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
        message
    }

    fn build_owned_task_intervention_message(
        &self,
        member: &MemberInstance,
        owned_tasks: &[&crate::task::Task],
        direct_reports: &[String],
    ) -> String {
        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let board_dir_str = board_dir.display();
        let task_summary = owned_tasks
            .iter()
            .map(|task| format!("#{} ({}) {}", task.id, task.status, task.title))
            .collect::<Vec<_>>()
            .join("; ");
        let task_context_cmds = owned_tasks
            .iter()
            .map(|task| {
                format!(
                    "- `kanban-md show --dir {board_dir_str} {task_id}`\n- `sed -n '1,220p' {task_path}`",
                    task_id = task.id,
                    task_path = task.source_path.display(),
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let first_task = owned_tasks[0];

        let mut message = format!(
            "Owned active task backlog detected: you are idle but still own active board task(s): {task_summary}.\n\
Retrieve task context now:\n\
1. `kanban-md list --dir {board_dir_str} --status in-progress`\n\
2. Review each owned task:\n{task_context_cmds}",
        );

        if let Some(first_report) = direct_reports.first() {
            let report_is_engineer = self
                .config
                .members
                .iter()
                .find(|candidate| candidate.name == *first_report)
                .is_some_and(|candidate| candidate.role_type == RoleType::Engineer);
            if report_is_engineer {
                message.push_str(&format!(
                    "\n3. If the task can move, assign the next concrete slice now with `batty assign {first_report} \"Task #{task_id}: <scoped subtask>\"`.",
                    task_id = first_task.id,
                ));
            } else {
                message.push_str(&format!(
                    "\n3. If the task can move, delegate the next concrete step now with `batty send {first_report} \"Task #{task_id}: <next step>\"`.",
                    task_id = first_task.id,
                ));
            }
        }

        if let Some(parent) = &member.reports_to {
            message.push_str(&format!(
                "\n4. If the lane is blocked, escalate explicitly with `batty send {parent} \"Task #{task_id} blocker: <exact blocker and next decision>\"`.",
                task_id = first_task.id,
            ));
        }

        message.push_str(&format!(
            "\n5. If the work is complete or ready for review, update board state now with `kanban-md move --dir {board_dir_str} {task_id} review` or `kanban-md move --dir {board_dir_str} {task_id} done` as appropriate.",
            task_id = first_task.id,
        ));
        message.push_str(
            "\nDo not stay idle while owning active work. Either move the task forward, split it, or escalate the blocker now. Batty will remind you again the next time you become idle while you still own unfinished tasks.",
        );
        message
    }

    fn build_review_intervention_message(
        &self,
        member: &MemberInstance,
        review_tasks: &[&crate::task::Task],
    ) -> String {
        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let board_dir_str = board_dir.display();
        let task_summary = review_tasks
            .iter()
            .map(|task| {
                let claimed_by = task.claimed_by.as_deref().unwrap_or("unknown");
                if let Some(context) = self.member_worktree_context(claimed_by) {
                    match context.branch {
                        Some(branch) => format!(
                            "#{} by {} [branch: {} | worktree: {}]",
                            task.id,
                            claimed_by,
                            branch,
                            context.path.display()
                        ),
                        None => format!(
                            "#{} by {} [worktree: {}]",
                            task.id,
                            claimed_by,
                            context.path.display()
                        ),
                    }
                } else {
                    format!("#{} by {}", task.id, claimed_by)
                }
            })
            .collect::<Vec<_>>()
            .join("; ");
        let task_context_cmds = review_tasks
            .iter()
            .map(|task| {
                let claimed_by = task.claimed_by.as_deref().unwrap_or("unknown");
                let mut lines = vec![
                    format!("- `kanban-md show --dir {board_dir_str} {}`", task.id),
                    format!("- `sed -n '1,220p' {}`", task.source_path.display()),
                ];
                if let Some(context) = self.member_worktree_context(claimed_by) {
                    lines.push(format!(
                        "- worktree: `{}`{}",
                        context.path.display(),
                        context
                            .branch
                            .as_deref()
                            .map(|branch| format!(" (branch `{branch}`)"))
                            .unwrap_or_default()
                    ));
                }
                lines.join("\n")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let first_task = review_tasks[0];
        let first_report = first_task.claimed_by.as_deref().unwrap_or("engineer");
        let first_report_is_engineer = self
            .config
            .members
            .iter()
            .find(|candidate| candidate.name == first_report)
            .is_some_and(|candidate| candidate.role_type == RoleType::Engineer);

        let mut message = format!(
            "Review backlog detected: direct-report work has completed and is waiting for your review: {task_summary}.\n\
Review and disposition it now:\n\
1. `kanban-md list --dir {board_dir_str} --status review`\n\
2. `batty inbox {member_name}` then `batty read {member_name} <ref>` to inspect the completion packet(s).\n\
3. Review each task and its lane context:\n{task_context_cmds}",
            member_name = member.name,
        );

        if first_report_is_engineer {
            message.push_str(&format!(
                "\n4. To accept engineer work, run `batty merge {first_report}` then `kanban-md move --dir {board_dir_str} {task_id} done`.",
                task_id = first_task.id,
            ));
        } else {
            message.push_str(&format!(
                "\n4. To accept the review packet, move it forward with `kanban-md move --dir {board_dir_str} {task_id} done` and send the disposition to `{first_report}`.",
                task_id = first_task.id,
            ));
        }

        message.push_str(&format!(
            "\n5. To discard it, run `kanban-md move --dir {board_dir_str} {task_id} archived` and `batty send {first_report} \"Task #{task_id} discarded: <reason>\"`.",
            task_id = first_task.id,
        ));
        let rework_command = if first_report_is_engineer {
            format!(
                "`batty assign {first_report} \"Task #{task_id}: <required changes>\"`",
                task_id = first_task.id
            )
        } else {
            format!(
                "`batty send {first_report} \"Task #{task_id}: <required changes>\"`",
                task_id = first_task.id
            )
        };
        message.push_str(&format!(
            "\n6. To request rework, run `kanban-md move --dir {board_dir_str} {task_id} in-progress` and {rework_command}.",
            task_id = first_task.id,
        ));

        if let Some(parent) = &member.reports_to {
            message.push_str(&format!(
                "\n7. After each review decision, report upward with `batty send {parent} \"Reviewed Task #{task_id}: merged / archived / rework sent to {first_report}\"`.",
                task_id = first_task.id,
            ));
        }

        message.push_str(
            "\nDo not leave completed direct-report work parked in review. Merge it, discard it, or send exact rework now. Batty will remind you again if review backlog remains unchanged.",
        );
        message
    }

    fn build_manager_dispatch_gap_message(
        &self,
        member: &MemberInstance,
        idle_active_reports: &[&ReportDispatchSnapshot],
        idle_unassigned_reports: &[&ReportDispatchSnapshot],
        unassigned_open_tasks: &[&crate::task::Task],
    ) -> String {
        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let board_dir_str = board_dir.display();
        let active_report_summary = if idle_active_reports.is_empty() {
            "none".to_string()
        } else {
            idle_active_reports
                .iter()
                .map(|snapshot| {
                    let ids = snapshot
                        .active_task_ids
                        .iter()
                        .map(|id| format!("#{id}"))
                        .collect::<Vec<_>>()
                        .join(",");
                    format!("{} on {}", snapshot.name, ids)
                })
                .collect::<Vec<_>>()
                .join("; ")
        };
        let unassigned_report_summary = if idle_unassigned_reports.is_empty() {
            "none".to_string()
        } else {
            idle_unassigned_reports
                .iter()
                .map(|snapshot| snapshot.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let open_task_summary = if unassigned_open_tasks.is_empty() {
            "none".to_string()
        } else {
            unassigned_open_tasks
                .iter()
                .take(3)
                .map(|task| format!("#{} ({}) {}", task.id, task.status, task.title))
                .collect::<Vec<_>>()
                .join("; ")
        };

        let mut message = format!(
            "Dispatch recovery needed: you are idle, your reports are idle, and the lane has no triage/review backlog. Idle reports still holding active work: {active_report_summary}. Idle reports with no active task: {unassigned_report_summary}. Unassigned open board work: {open_task_summary}.\n\
Recover the lane now:\n\
1. `batty status`\n\
2. `kanban-md list --dir {board_dir_str} --status in-progress`\n\
3. `kanban-md list --dir {board_dir_str} --status todo`\n\
4. `kanban-md list --dir {board_dir_str} --status backlog`"
        );

        if let Some(first_active) = idle_active_reports.first() {
            let first_task_id = first_active.active_task_ids[0];
            message.push_str(&format!(
                "\n5. For an idle active lane, intervene directly with `batty send {report} \"Task #{task_id} is idle under your ownership. Either move it forward now, report the exact blocker, or request board normalization.\"`.",
                report = first_active.name,
                task_id = first_task_id,
            ));
        }

        if let (Some(first_unassigned_report), Some(first_open_task)) = (
            idle_unassigned_reports.first(),
            unassigned_open_tasks.first(),
        ) {
            message.push_str(&format!(
                "\n6. If executable work exists, start it now with `batty assign {report} \"Task #{task_id}: {title}\"`.",
                report = first_unassigned_report.name,
                task_id = first_open_task.id,
                title = first_open_task.title,
            ));
        }

        if let Some(parent) = &member.reports_to {
            message.push_str(&format!(
                "\n7. If the lane has no executable next step, escalate explicitly with `batty send {parent} \"lane blocked: all reports idle; need new dispatch or decision\"`."
            ));
        }

        message.push_str(
            "\nDo not let the entire lane sit idle. Either wake an active task, assign new executable work, or escalate the exact blockage now.",
        );
        message
    }

    fn build_architect_utilization_message(
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

        let mut message = format!(
            "Utilization recovery needed: you are idle while team throughput is low. Working engineers: {working_summary}. Idle engineers still holding active work: {idle_active_summary}. Idle engineers with no active task: {idle_unassigned_summary}. Unassigned open board work: {open_task_summary}.\n\
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
        message
    }

    fn member_worktree_context(&self, member_name: &str) -> Option<MemberWorktreeContext> {
        let worktree_path = self
            .config
            .project_root
            .join(".batty")
            .join("worktrees")
            .join(member_name);
        if !worktree_path.exists() {
            return None;
        }

        let branch = std::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(&worktree_path)
            .output()
            .ok()
            .and_then(|output| {
                output
                    .status
                    .success()
                    .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
            })
            .filter(|branch| !branch.is_empty());

        Some(MemberWorktreeContext {
            path: worktree_path,
            branch,
        })
    }

    /// Generate and inject standup for each recipient whose interval has elapsed.
    ///
    /// Each recipient gets a scoped standup showing only their direct reports.
    /// The interval is per-role: `standup_interval_secs` on the role definition
    /// takes precedence, falling back to the global `standup.interval_secs`.
    /// Skipped entirely when the pause marker file exists.
    fn maybe_generate_standup(&mut self) -> Result<()> {
        if !self.config.team_config.automation.standups {
            return Ok(());
        }
        if super::pause_marker_path(&self.config.project_root).exists() {
            return Ok(());
        }
        let global_interval = self.config.team_config.standup.interval_secs;
        if global_interval == 0 {
            return Ok(());
        }

        // Build list of recipients with their per-role intervals.
        // Default: managers and architects get standups, others don't unless configured.
        let mut recipients: Vec<(MemberInstance, Duration)> = Vec::new();
        for role in &self.config.team_config.roles {
            let receives = role.receives_standup.unwrap_or(matches!(
                role.role_type,
                RoleType::Manager | RoleType::Architect
            ));
            if !receives {
                continue;
            }
            let interval_secs = role.standup_interval_secs.unwrap_or(global_interval);
            let interval = Duration::from_secs(interval_secs);
            for member in &self.config.members {
                if member.role_name == role.name {
                    recipients.push((member.clone(), interval));
                }
            }
        }

        let mut any_generated = false;

        for (recipient, interval) in &recipients {
            if self.paused_standups.contains(&recipient.name) {
                continue;
            }

            // Check per-member timer
            let last = self.last_standup.get(&recipient.name).copied();
            let should_fire = match last {
                Some(t) => t.elapsed() >= *interval,
                None => true, // first standup — wait one full interval
            };

            if last.is_none() {
                // Initialize timer so first standup fires after one interval
                self.last_standup
                    .insert(recipient.name.clone(), Instant::now());
                continue;
            }

            if !should_fire {
                continue;
            }

            let board_dir = super::team_config_dir(&self.config.project_root).join("board");
            let report = standup::generate_board_aware_standup_for(
                recipient,
                &self.config.members,
                &self.watchers,
                &self.states,
                self.config.team_config.standup.output_lines as usize,
                Some(&board_dir),
            );

            match recipient.role_type {
                RoleType::User => {
                    if let Some(bot) = &self.telegram_bot {
                        let chat_id = self
                            .config
                            .team_config
                            .roles
                            .iter()
                            .find(|role| {
                                role.role_type == RoleType::User && role.name == recipient.role_name
                            })
                            .and_then(|role| role.channel_config.as_ref())
                            .map(|config| config.target.clone());

                        match chat_id {
                            Some(chat_id) => {
                                if let Err(e) = bot.send_message(&chat_id, &report) {
                                    warn!(
                                        member = %recipient.name,
                                        target = %chat_id,
                                        error = %e,
                                        "failed to send standup via telegram"
                                    );
                                } else {
                                    self.emit_event(TeamEvent::standup_generated(&recipient.name));
                                    any_generated = true;
                                }
                            }
                            None => {
                                warn!(
                                    member = %recipient.name,
                                    "telegram standup delivery skipped: missing target"
                                );
                            }
                        }
                    } else {
                        match standup::write_standup_file(&self.config.project_root, &report) {
                            Ok(path) => {
                                info!(member = %recipient.name, path = %path.display(), "standup written to file");
                                self.emit_event(TeamEvent::standup_generated(&recipient.name));
                                any_generated = true;
                            }
                            Err(e) => {
                                warn!(member = %recipient.name, error = %e, "failed to write standup file");
                            }
                        }
                    }
                }
                _ => {
                    if let Some(pane_id) = self.config.pane_map.get(&recipient.name) {
                        if let Err(e) = standup::inject_standup(pane_id, &report) {
                            warn!(member = %recipient.name, error = %e, "failed to inject standup");
                        } else {
                            self.emit_event(TeamEvent::standup_generated(&recipient.name));
                            any_generated = true;
                        }
                    }
                }
            }

            self.last_standup
                .insert(recipient.name.clone(), Instant::now());
        }

        if any_generated {
            info!("standups generated and delivered");
        }
        Ok(())
    }

    /// Rotate the board if enough time has passed.
    ///
    /// When using kanban-md (board/ directory), rotation is not needed — each
    /// task is an individual file. Only rotates the legacy plain kanban.md.
    fn maybe_rotate_board(&mut self) -> Result<()> {
        // Check every 10 minutes
        if self.last_board_rotation.elapsed() < Duration::from_secs(600) {
            return Ok(());
        }

        self.last_board_rotation = Instant::now();

        let config_dir = self.config.project_root.join(".batty").join("team_config");

        // kanban-md uses a board/ directory — no rotation needed
        let board_dir = config_dir.join("board");
        if board_dir.is_dir() {
            return Ok(());
        }

        // Legacy plain kanban.md — rotate done items
        let kanban_path = config_dir.join("kanban.md");
        let archive_path = config_dir.join("kanban-archive.md");

        if kanban_path.exists() {
            match board::rotate_done_items(
                &kanban_path,
                &archive_path,
                self.config.team_config.board.rotation_threshold,
            ) {
                Ok(rotated) if rotated > 0 => {
                    info!(rotated, "board rotation completed");
                }
                Ok(_) => {}
                Err(e) => {
                    warn!(error = %e, "board rotation failed");
                }
            }
        }

        Ok(())
    }

    fn maybe_generate_retrospective(&mut self) -> Result<()> {
        if self.retro_generated {
            return Ok(());
        }

        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let tasks_dir = board_dir.join("tasks");
        if !tasks_dir.is_dir() {
            return Ok(());
        }

        let tasks = crate::task::load_tasks_from_dir(&tasks_dir)?;
        let active_tasks: Vec<&crate::task::Task> = tasks
            .iter()
            .filter(|task| task.status != "archived")
            .collect();
        if active_tasks.is_empty() || active_tasks.iter().any(|task| task.status != "done") {
            return Ok(());
        }

        let events_path = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        let Some(stats) = super::retrospective::analyze_event_log(&events_path)? else {
            return Ok(());
        };

        let report_path =
            super::retrospective::generate_retrospective(&self.config.project_root, &stats)?;
        self.retro_generated = true;
        self.emit_event(TeamEvent::retro_generated());
        info!(path = %report_path.display(), "retrospective generated");
        Ok(())
    }

    /// Poll Telegram for inbound messages from the human user.
    /// Routes them as inbox messages from:human to the user role's talks_to targets.
    fn poll_telegram(&mut self) -> Result<()> {
        let Some(bot) = &mut self.telegram_bot else {
            return Ok(());
        };

        let messages = match bot.poll_updates() {
            Ok(msgs) => msgs,
            Err(e) => {
                debug!(error = %e, "telegram poll failed");
                return Ok(());
            }
        };

        if messages.is_empty() {
            return Ok(());
        }

        let root = inbox::inboxes_root(&self.config.project_root);

        // Find the user role's talks_to targets
        let targets: Vec<String> = self
            .config
            .team_config
            .roles
            .iter()
            .find(|r| r.role_type == RoleType::User)
            .map(|r| r.talks_to.clone())
            .unwrap_or_default();

        for msg in messages {
            info!(
                from_user = msg.from_user_id,
                text_len = msg.text.len(),
                "telegram inbound"
            );

            // Route to each talks_to target (typically just "architect")
            for target in &targets {
                let inbox_msg = inbox::InboxMessage::new_send("human", target, &msg.text);
                if let Err(e) = inbox::deliver_to_inbox(&root, &inbox_msg) {
                    warn!(to = %target, error = %e, "failed to deliver telegram message to inbox");
                }
            }

            self.emit_event(TeamEvent::message_routed("human", "telegram"));
        }

        Ok(())
    }

    /// Deliver pending messages in user-role inboxes via their channel.
    /// This handles outbound messages from agents TO the human user.
    fn deliver_user_inbox(&mut self) -> Result<()> {
        let root = inbox::inboxes_root(&self.config.project_root);

        // Find user roles (they have channels, not panes)
        let user_roles: Vec<String> = self
            .config
            .team_config
            .roles
            .iter()
            .filter(|r| r.role_type == RoleType::User)
            .map(|r| r.name.clone())
            .collect();

        for user_name in &user_roles {
            let messages = match inbox::pending_messages(&root, user_name) {
                Ok(msgs) => msgs,
                Err(e) => {
                    debug!(user = %user_name, error = %e, "failed to read user inbox");
                    continue;
                }
            };

            if messages.is_empty() {
                continue;
            }

            for msg in &messages {
                info!(from = %msg.from, to = %user_name, id = %msg.id, "delivering to user channel");

                let formatted = format!("--- Message from {} ---\n{}", msg.from, msg.body);
                let send_result = match self.channels.get(user_name) {
                    Some(channel) => channel.send(&formatted),
                    None => {
                        debug!(user = %user_name, "no channel for user role");
                        break;
                    }
                };
                if let Err(e) = send_result {
                    warn!(to = %user_name, error = %e, "failed to send via channel");
                    // Don't mark as delivered on failure — retry next cycle
                    continue;
                }

                if let Err(e) = inbox::mark_delivered(&root, user_name, &msg.id) {
                    warn!(user = %user_name, id = %msg.id, error = %e, "failed to mark delivered");
                }

                self.emit_event(TeamEvent::message_routed(&msg.from, user_name));
            }
        }

        Ok(())
    }

    /// Resolve a member instance name to its role definition name.
    fn resolve_role_name(&self, member_name: &str) -> String {
        if member_name == "human" || member_name == "daemon" {
            return member_name.to_string();
        }
        self.config
            .members
            .iter()
            .find(|m| m.name == member_name)
            .map(|m| m.role_name.clone())
            .unwrap_or_else(|| member_name.to_string())
    }

    fn automation_sender_for(&self, recipient: &str) -> String {
        let recipient_member = self
            .config
            .members
            .iter()
            .find(|member| member.name == recipient);

        if let Some(member) = recipient_member {
            if let Some(parent) = &member.reports_to {
                return parent.clone();
            }
        }

        if let Some(sender) = &self.config.team_config.automation_sender {
            return sender.clone();
        }

        "daemon".to_string()
    }
}

fn describe_command_failure(command: &str, args: &[&str], output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let details = if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        format!("process exited with status {}", output.status)
    };

    format!("`{command} {}` failed: {details}", args.join(" "))
}

/// Extract the `## Nudge` section from a prompt .md file.
///
/// Returns the text after `## Nudge` up to the next `## ` heading or EOF.
/// Returns `None` if no `## Nudge` section is found.
fn extract_nudge_section(prompt_path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(prompt_path).ok()?;
    let mut in_nudge = false;
    let mut lines = Vec::new();

    for line in content.lines() {
        if line.starts_with("## Nudge") {
            in_nudge = true;
            continue;
        }
        if in_nudge {
            // Stop at next heading
            if line.starts_with("## ") {
                break;
            }
            lines.push(line);
        }
    }

    if lines.is_empty() {
        return None;
    }

    let text = lines.join("\n").trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

/// Strip the `## Nudge` section from prompt text so it's not sent to the agent.
///
/// The nudge content is daemon-only — injected periodically, not part of the
/// initial prompt.
fn strip_nudge_section(prompt: &str) -> String {
    let mut lines = Vec::new();
    let mut in_nudge = false;

    for line in prompt.lines() {
        if line.starts_with("## Nudge") {
            in_nudge = true;
            continue;
        }
        if in_nudge && line.starts_with("## ") {
            in_nudge = false;
        }
        if !in_nudge {
            lines.push(line);
        }
    }

    lines.join("\n").trim_end().to_string()
}

fn format_nudge_status(schedule: Option<&NudgeSchedule>) -> String {
    let Some(schedule) = schedule else {
        return String::new();
    };

    if schedule.fired_this_idle {
        return " #[fg=magenta]nudge sent#[default]".to_string();
    }

    if schedule.paused {
        return " #[fg=244]nudge paused#[default]".to_string();
    }

    let Some(idle_since) = schedule.idle_since else {
        // No active idle countdown to display.
        return String::new();
    };

    let elapsed = idle_since.elapsed();
    if elapsed < schedule.interval {
        let remaining = schedule.interval - elapsed;
        let mins = remaining.as_secs() / 60;
        let secs = remaining.as_secs() % 60;
        format!(" #[fg=magenta]nudge {mins}:{secs:02}#[default]")
    } else {
        " #[fg=magenta]nudge now#[default]".to_string()
    }
}

fn format_inbox_status(pending_count: usize) -> String {
    if pending_count == 0 {
        " #[fg=244]inbox 0#[default]".to_string()
    } else {
        format!(" #[fg=colour214,bold]inbox {pending_count}#[default]")
    }
}

fn format_active_task_status(active_task_ids: &[u32]) -> String {
    match active_task_ids {
        [] => String::new(),
        [task_id] => format!(" #[fg=green,bold]task {task_id}#[default]"),
        _ => format!(" #[fg=green,bold]tasks {}#[default]", active_task_ids.len()),
    }
}

fn format_review_task_status(review_task_ids: &[u32]) -> String {
    match review_task_ids {
        [] => String::new(),
        [task_id] => format!(" #[fg=blue,bold]review {task_id}#[default]"),
        _ => format!(" #[fg=blue,bold]review {}#[default]", review_task_ids.len()),
    }
}

fn compose_pane_status_label(
    state: MemberState,
    pending_inbox: usize,
    triage_backlog: usize,
    active_task_ids: &[u32],
    review_task_ids: &[u32],
    globally_paused: bool,
    nudge_status: &str,
    standup_status: &str,
) -> String {
    let state_str = match state {
        MemberState::Idle => "#[fg=yellow]idle#[default]",
        MemberState::Working => "#[fg=cyan]working#[default]",
    };
    let inbox_str = format_inbox_status(pending_inbox);
    let triage_str = if triage_backlog > 0 {
        format!(" #[fg=red,bold]triage {triage_backlog}#[default]")
    } else {
        String::new()
    };
    let active_task_str = format_active_task_status(active_task_ids);
    let review_task_str = format_review_task_status(review_task_ids);

    if globally_paused {
        return format!(
            "{state_str}{inbox_str}{triage_str}{active_task_str}{review_task_str} #[fg=red]PAUSED#[default]"
        );
    }

    format!(
        "{state_str}{inbox_str}{triage_str}{active_task_str}{review_task_str}{nudge_status}{standup_status}"
    )
}

fn task_needs_owned_intervention(status: &str) -> bool {
    !matches!(status, "review" | "done" | "archived")
}

fn manager_dispatch_intervention_key(member_name: &str) -> String {
    format!("dispatch::{member_name}")
}

fn manager_dispatch_intervention_signature(
    idle_active_reports: &[&ReportDispatchSnapshot],
    idle_unassigned_reports: &[&ReportDispatchSnapshot],
    unassigned_open_tasks: &[&crate::task::Task],
) -> String {
    let mut parts = Vec::new();
    for snapshot in idle_active_reports {
        let task_ids = snapshot
            .active_task_ids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        parts.push(format!("active:{}:{task_ids}", snapshot.name));
    }
    for snapshot in idle_unassigned_reports {
        parts.push(format!("idle:{}", snapshot.name));
    }
    for task in unassigned_open_tasks {
        parts.push(format!("open:{}:{}", task.id, task.status));
    }
    parts.sort();
    parts.join("|")
}

fn owned_task_intervention_signature(tasks: &[&crate::task::Task]) -> String {
    let mut parts = tasks
        .iter()
        .map(|task| format!("{}:{}", task.id, task.status))
        .collect::<Vec<_>>();
    parts.sort();
    parts.join("|")
}

fn review_backlog_owner_for_task(
    task: &crate::task::Task,
    members: &[MemberInstance],
) -> Option<String> {
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

fn review_intervention_key(member_name: &str) -> String {
    format!("review::{member_name}")
}

fn architect_utilization_intervention_key(member_name: &str) -> String {
    format!("utilization::{member_name}")
}

fn architect_utilization_intervention_signature(
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

fn review_task_intervention_signature(tasks: &[&crate::task::Task]) -> String {
    let mut parts = tasks
        .iter()
        .map(|task| {
            format!(
                "{}:{}:{}",
                task.id,
                task.status,
                task.claimed_by.as_deref().unwrap_or("unknown")
            )
        })
        .collect::<Vec<_>>();
    parts.sort();
    parts.join("|")
}

fn append_orchestrator_log_line(path: &Path, message: &str) -> Result<()> {
    use std::io::Write;
    let mut file = super::open_log_for_append(path)?;
    writeln!(file, "[{}] {}", now_unix(), message)?;
    file.flush()?;
    Ok(())
}

fn format_standup_status(
    last_standup: Option<Instant>,
    interval: Duration,
    paused: bool,
) -> String {
    if paused {
        return " #[fg=244]standup paused#[default]".to_string();
    }

    let Some(last_standup) = last_standup else {
        return String::new();
    };

    let elapsed = last_standup.elapsed();
    if elapsed < interval {
        let remaining = interval - elapsed;
        let mins = remaining.as_secs() / 60;
        let secs = remaining.as_secs() % 60;
        format!(" #[fg=blue]standup {mins}:{secs:02}#[default]")
    } else {
        " #[fg=blue]standup now#[default]".to_string()
    }
}

/// Write a launch script to a temp file and return the short command to execute it.
///
/// This avoids pasting huge prompt strings via tmux paste-buffer, which garbles
/// long text. Instead we write a self-contained bash script and paste just
/// `bash /tmp/batty-launch-<member>.sh`.
/// Write a launch script to a temp file and return the short command to execute it.
///
/// `idle`: if true, the role prompt goes into `--append-system-prompt` and the
/// agent launches with no initial user message (sits at the `>` prompt waiting
/// for the daemon to inject work). If false, the prompt is sent as the first
/// user message so the agent starts working immediately.
#[allow(clippy::too_many_arguments)]
fn write_launch_script(
    member_name: &str,
    agent_name: &str,
    prompt: &str,
    role_context: Option<&str>,
    work_dir: &Path,
    project_root: &Path,
    idle: bool,
    resume: bool,
    session_id: Option<&str>,
) -> Result<String> {
    let script_path = std::env::temp_dir().join(format!("batty-launch-{member_name}.sh"));
    let escaped_prompt = prompt.replace('\'', "'\\''");
    let launch_dir = match agent_name {
        "codex" | "codex-cli" => prepare_codex_context(member_name, role_context, work_dir)?,
        _ => work_dir.to_path_buf(),
    };
    let launch_dir_str = launch_dir.to_string_lossy();

    let agent_cmd = match agent_name {
        "codex" | "codex-cli" => {
            if resume {
                // Resume the most recent Codex session in this working directory
                "exec codex resume --last --dangerously-bypass-approvals-and-sandbox".to_string()
            } else {
                let prefix = "exec codex --dangerously-bypass-approvals-and-sandbox";
                if idle {
                    prefix.to_string()
                } else {
                    format!("{prefix} '{escaped_prompt}'")
                }
            }
        }
        _ => {
            if resume {
                let session_id = session_id.context("missing Claude session ID for resume")?;
                format!("exec claude --dangerously-skip-permissions --resume '{session_id}'")
            } else if idle {
                let session_flag = session_id
                    .map(|id| format!(" --session-id '{id}'"))
                    .unwrap_or_default();
                format!(
                    "exec claude --dangerously-skip-permissions{session_flag} --append-system-prompt '{escaped_prompt}'"
                )
            } else {
                let session_flag = session_id
                    .map(|id| format!(" --session-id '{id}'"))
                    .unwrap_or_default();
                format!(
                    "exec claude --dangerously-skip-permissions{session_flag} '{escaped_prompt}'"
                )
            }
        }
    };

    // Create wrapper scripts in a per-member bin directory prepended to PATH.
    // This ensures agents use the correct binaries regardless of their environment.
    let wrapper_dir = std::env::temp_dir().join(format!("batty-bin-{member_name}"));
    std::fs::create_dir_all(&wrapper_dir).ok();

    #[cfg(unix)]
    let set_executable = |path: &std::path::Path| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).ok();
    };
    #[cfg(not(unix))]
    let set_executable = |_path: &std::path::Path| {};

    // kanban-md wrapper: auto-adds --dir pointing to the project board
    let board_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board");
    let real_kanban = resolve_binary("kanban-md");
    let kanban_wrapper = wrapper_dir.join("kanban-md");
    std::fs::write(
        &kanban_wrapper,
        format!(
            "#!/bin/bash\nexec '{}' \"$@\" --dir '{}'\n",
            real_kanban,
            board_dir.to_string_lossy()
        ),
    )
    .ok();
    set_executable(&kanban_wrapper);

    // batty wrapper: resolve the installed binary from PATH.
    // Do NOT use current_exe() — if the daemon was launched from a debug/test
    // build (target/debug/deps/batty-*), agents would inherit that test binary,
    // causing `batty send` to run cargo tests instead of the real command.
    let real_batty = resolve_binary("batty");
    let batty_wrapper = wrapper_dir.join("batty");
    std::fs::write(
        &batty_wrapper,
        format!("#!/bin/bash\nexec '{}' \"$@\"\n", real_batty),
    )
    .ok();
    set_executable(&batty_wrapper);

    let script = format!(
        "#!/bin/bash\nexport PATH='{}':\"$PATH\"\ncd '{launch_dir_str}'\n{agent_cmd}\n",
        wrapper_dir.to_string_lossy()
    );
    std::fs::write(&script_path, &script)
        .with_context(|| format!("failed to write launch script {}", script_path.display()))?;

    Ok(format!("bash '{}'", script_path.to_string_lossy()))
}

fn prepare_codex_context(
    member_name: &str,
    role_context: Option<&str>,
    work_dir: &Path,
) -> Result<PathBuf> {
    let context_dir = work_dir
        .join(".batty")
        .join("codex-context")
        .join(member_name);
    std::fs::create_dir_all(&context_dir)
        .with_context(|| format!("failed to create {}", context_dir.display()))?;

    if let Some(role_context) = role_context {
        let agents_path = context_dir.join("AGENTS.md");
        let content = format!(
            "# Batty Role Context: {member_name}\n\n\
             This file is generated by Batty for the Codex agent running as `{member_name}`.\n\
             Follow these instructions in addition to any repository-level `AGENTS.md` files.\n\n\
             {role_context}\n"
        );
        std::fs::write(&agents_path, content)
            .with_context(|| format!("failed to write {}", agents_path.display()))?;
    }

    Ok(context_dir)
}

fn role_starts_idle() -> bool {
    true
}

fn initial_member_state(idle: bool, resume: bool) -> MemberState {
    if idle && !resume {
        MemberState::Idle
    } else {
        MemberState::Working
    }
}

fn should_activate_watcher_on_spawn(idle: bool, resume: bool) -> bool {
    !idle || resume
}

fn canonical_agent_name(agent_name: &str) -> String {
    agent::adapter_from_name(agent_name)
        .map(|adapter| adapter.name().to_string())
        .unwrap_or_else(|| agent_name.to_string())
}

fn launch_state_path(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("launch-state.json")
}

fn daemon_state_path(project_root: &Path) -> PathBuf {
    super::daemon_state_path(project_root)
}

fn load_launch_state(project_root: &Path) -> HashMap<String, LaunchIdentity> {
    let path = launch_state_path(project_root);
    let Ok(content) = fs::read_to_string(&path) else {
        return HashMap::new();
    };

    match serde_json::from_str(&content) {
        Ok(state) => state,
        Err(error) => {
            warn!(path = %path.display(), error = %error, "failed to parse launch state, ignoring");
            HashMap::new()
        }
    }
}

fn save_launch_state(project_root: &Path, state: &HashMap<String, LaunchIdentity>) -> Result<()> {
    let path = launch_state_path(project_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let content =
        serde_json::to_string_pretty(state).context("failed to serialize launch state")?;
    fs::write(&path, content).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn load_daemon_state(project_root: &Path) -> Option<PersistedDaemonState> {
    let path = daemon_state_path(project_root);
    let Ok(content) = fs::read_to_string(&path) else {
        return None;
    };

    match serde_json::from_str(&content) {
        Ok(state) => Some(state),
        Err(error) => {
            warn!(path = %path.display(), error = %error, "failed to parse daemon state, ignoring");
            None
        }
    }
}

fn save_daemon_state(project_root: &Path, state: &PersistedDaemonState) -> Result<()> {
    let path = daemon_state_path(project_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let content =
        serde_json::to_string_pretty(state).context("failed to serialize daemon state")?;
    fs::write(&path, content).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn new_member_session_id(agent_name: &str) -> Option<String> {
    (agent_name == "claude-code").then(|| Uuid::new_v4().to_string())
}

fn summarize_assignment(task: &str) -> String {
    let summary = task
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("task")
        .trim();
    if summary.len() <= 120 {
        summary.to_string()
    } else {
        format!("{}...", &summary[..117])
    }
}

fn engineer_task_branch_name(engineer: &str, task: &str, explicit_task_id: Option<u32>) -> String {
    let suffix = explicit_task_id
        .or_else(|| parse_assignment_task_id(task))
        .map(|task_id| format!("task-{task_id}"))
        .unwrap_or_else(|| {
            let slug = slugify_task_branch(task);
            let unique = Uuid::new_v4().simple().to_string();
            format!("task-{slug}-{}", &unique[..8])
        });
    format!("{engineer}/{suffix}")
}

fn parse_assignment_task_id(task: &str) -> Option<u32> {
    let mut candidates = Vec::new();
    if let Some(first_line) = task.lines().find(|line| !line.trim().is_empty()) {
        candidates.push(first_line);
    }
    candidates.push(task.trim());
    for candidate in candidates {
        for marker in ["Task #", "task #", "#"] {
            let Some(start) = candidate.find(marker) else {
                continue;
            };
            let digits: String = candidate[start + marker.len()..]
                .chars()
                .take_while(|ch| ch.is_ascii_digit())
                .collect();
            if !digits.is_empty() {
                return digits.parse().ok();
            }
        }
    }
    None
}

fn slugify_task_branch(task: &str) -> String {
    let source = task
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("task");
    let mut slug = String::new();
    let mut last_was_dash = false;
    for ch in source.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            slug.push(lower);
            last_was_dash = false;
        } else if !last_was_dash && !slug.is_empty() {
            slug.push('-');
            last_was_dash = true;
        }
        if slug.len() >= 24 {
            break;
        }
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "task".to_string()
    } else {
        slug
    }
}

fn default_claude_projects_root() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
        .join(".claude")
        .join("projects")
}

fn claude_session_id_exists(session_id: &str) -> bool {
    claude_session_id_exists_in(&default_claude_projects_root(), session_id)
}

fn claude_session_id_exists_in(projects_root: &Path, session_id: &str) -> bool {
    let session_file = format!("{session_id}.jsonl");
    let Ok(entries) = fs::read_dir(projects_root) else {
        return false;
    };

    entries.flatten().any(|entry| {
        let path = entry.path();
        path.is_dir() && path.join(&session_file).exists()
    })
}

fn resolve_member_launch_session(
    agent_name: &str,
    previous_identity: Option<&LaunchIdentity>,
    resume_requested: bool,
    claude_session_available: bool,
    duplicate_session_id: bool,
) -> (bool, Option<String>) {
    let Some(session_id) = new_member_session_id(agent_name) else {
        return (resume_requested, None);
    };

    if !resume_requested {
        return (false, Some(session_id));
    }

    if duplicate_session_id {
        return (false, Some(session_id));
    }

    if let Some(previous_session_id) =
        previous_identity.and_then(|identity| identity.session_id.clone())
    {
        if !claude_session_available {
            return (false, Some(session_id));
        }
        return (true, Some(previous_session_id));
    }

    // Older launch-state files did not persist Claude session IDs. Starting
    // fresh is safer than ambiguous `claude --continue` in a shared cwd.
    (false, Some(session_id))
}

fn duplicate_claude_session_ids(state: &HashMap<String, LaunchIdentity>) -> HashSet<&str> {
    let mut counts = HashMap::new();
    for identity in state.values() {
        if identity.agent != "claude-code" {
            continue;
        }
        let Some(session_id) = identity.session_id.as_deref() else {
            continue;
        };
        *counts.entry(session_id).or_insert(0usize) += 1;
    }

    counts
        .into_iter()
        .filter_map(|(session_id, count)| (count > 1).then_some(session_id))
        .collect()
}

fn should_resume_member(
    resume_requested: bool,
    previous_state: &HashMap<String, LaunchIdentity>,
    member_name: &str,
    current_agent: &str,
    current_prompt: &str,
) -> bool {
    if !resume_requested {
        return false;
    }

    let Some(previous) = previous_state.get(member_name) else {
        return true;
    };

    if previous.agent == current_agent && previous.prompt == current_prompt {
        return true;
    }

    info!(
        member = member_name,
        previous_agent = %previous.agent,
        current_agent,
        prompt_changed = previous.prompt != current_prompt,
        "launch identity changed, forcing fresh start instead of resume"
    );
    false
}

fn member_session_tracker_config(
    project_root: &Path,
    member: &MemberInstance,
) -> Option<SessionTrackerConfig> {
    let work_dir = if member.use_worktrees {
        project_root
            .join(".batty")
            .join("worktrees")
            .join(&member.name)
    } else {
        project_root.to_path_buf()
    };

    match member.agent.as_deref() {
        Some("codex") | Some("codex-cli") => Some(SessionTrackerConfig::Codex {
            cwd: work_dir
                .join(".batty")
                .join("codex-context")
                .join(&member.name),
        }),
        Some("claude") | Some("claude-code") | None => {
            Some(SessionTrackerConfig::Claude { cwd: work_dir })
        }
        _ => None,
    }
}

/// Resolve the absolute path to a binary via `which`.
fn resolve_binary(name: &str) -> String {
    std::process::Command::new("which")
        .arg(name)
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::config::AutomationConfig;
    use std::collections::HashMap;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};
    use std::sync::{Arc, Mutex};

    use crate::team::comms::Channel;
    use crate::team::config::{
        BoardConfig, ChannelConfig, RoleDef, StandupConfig, WorkflowMode, WorkflowPolicy,
    };
    use crate::team::events::EventSink;
    use crate::team::watcher::WatcherState;
    use serial_test::serial;

    struct RecordingChannel {
        messages: Arc<Mutex<Vec<String>>>,
    }

    impl Channel for RecordingChannel {
        fn send(&self, message: &str) -> Result<()> {
            self.messages.lock().unwrap().push(message.to_string());
            Ok(())
        }

        fn channel_type(&self) -> &str {
            "test"
        }
    }

    struct FailingChannel;

    impl Channel for FailingChannel {
        fn send(&self, _message: &str) -> Result<()> {
            bail!("synthetic channel failure")
        }

        fn channel_type(&self) -> &str {
            "test-failing"
        }
    }

    struct FailingWriter;

    impl io::Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("synthetic event sink failure"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("synthetic event sink failure"))
        }
    }

    fn git(dir: &Path, args: &[&str]) -> Output {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap()
    }

    fn git_ok(dir: &Path, args: &[&str]) {
        let output = git(dir, args);
        assert!(
            output.status.success(),
            "git {:?} failed:\nstdout={}\nstderr={}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_git_repo(tmp: &tempfile::TempDir) -> PathBuf {
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::create_dir_all(repo.join(".batty").join("team_config")).unwrap();
        std::fs::write(
            repo.join("Cargo.toml"),
            "[package]\nname = \"batty-daemon-test\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src").join("lib.rs"),
            "pub fn smoke() -> bool { true }\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn smoke_test() {\n        assert!(smoke());\n    }\n}\n",
        )
        .unwrap();
        git_ok(tmp.path(), &["init", "-b", "main", repo.to_str().unwrap()]);
        git_ok(&repo, &["config", "user.email", "batty@example.com"]);
        git_ok(&repo, &["config", "user.name", "Batty Tests"]);
        git_ok(&repo, &["add", "."]);
        git_ok(&repo, &["commit", "-m", "initial"]);
        repo
    }

    fn write_task_file(project_root: &Path, id: u32, title: &str) {
        let tasks_dir = project_root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join(format!("{id:03}-{title}.md")),
            format!(
                "---\nid: {id}\ntitle: {title}\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\nclass: standard\n---\n\nTask description.\n"
            ),
        )
        .unwrap();
    }

    fn write_owned_task_file(
        project_root: &Path,
        id: u32,
        title: &str,
        status: &str,
        claimed_by: &str,
    ) {
        let tasks_dir = project_root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join(format!("{id:03}-{title}.md")),
            format!(
                "---\nid: {id}\ntitle: {title}\nstatus: {status}\npriority: critical\nclaimed_by: {claimed_by}\nclass: standard\n---\n\nTask description.\n"
            ),
        )
        .unwrap();
    }

    fn write_event_log(project_root: &Path, events: &[TeamEvent]) {
        let events_path = project_root
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        let mut sink = EventSink::new(&events_path).unwrap();
        for event in events {
            sink.emit(event.clone()).unwrap();
        }
    }

    fn backdate_idle_grace(daemon: &mut TeamDaemon, member_name: &str) {
        let grace = daemon.automation_idle_grace_duration() + Duration::from_secs(1);
        daemon
            .idle_started_at
            .insert(member_name.to_string(), Instant::now() - grace);
        if let Some(schedule) = daemon.nudges.get_mut(member_name) {
            schedule.idle_since = Some(Instant::now() - schedule.interval.max(grace));
        }
    }

    #[test]
    fn launch_script_active_sends_prompt_as_user_message() {
        let cmd = write_launch_script(
            "arch-1",
            "claude",
            "plan the project",
            None,
            Path::new("/project"),
            Path::new("/project"),
            false,
            false,
            Some("11111111-1111-4111-8111-111111111111"),
        )
        .unwrap();
        assert!(cmd.contains("batty-launch-arch-1.sh"));
        let script_path = std::env::temp_dir().join("batty-launch-arch-1.sh");
        let content = std::fs::read_to_string(&script_path).unwrap();
        assert!(content.contains(
            "claude --dangerously-skip-permissions --session-id '11111111-1111-4111-8111-111111111111' 'plan the project'"
        ));
        assert!(!content.contains("--append-system-prompt"));
    }

    #[test]
    fn launch_script_idle_uses_system_prompt() {
        let cmd = write_launch_script(
            "mgr-1",
            "claude",
            "You are the manager.",
            None,
            Path::new("/project"),
            Path::new("/project"),
            true,
            false,
            Some("22222222-2222-4222-8222-222222222222"),
        )
        .unwrap();
        assert!(cmd.contains("batty-launch-mgr-1.sh"));
        let script_path = std::env::temp_dir().join("batty-launch-mgr-1.sh");
        let content = std::fs::read_to_string(&script_path).unwrap();
        assert!(content.contains(
            "--session-id '22222222-2222-4222-8222-222222222222' --append-system-prompt"
        ));
        assert!(content.contains("--append-system-prompt"));
        assert!(!content.contains("'You are the manager.''\n"));
    }

    #[test]
    fn launch_script_idle_codex_uses_context_overlay() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = tmp.path().join("wt");
        std::fs::create_dir_all(&work_dir).unwrap();

        write_launch_script(
            "eng-1",
            "codex",
            "role context",
            Some("role context"),
            &work_dir,
            tmp.path(),
            true,
            false,
            None,
        )
        .unwrap();
        let script_path = std::env::temp_dir().join("batty-launch-eng-1.sh");
        let content = std::fs::read_to_string(&script_path).unwrap();
        let context_dir = work_dir.join(".batty").join("codex-context").join("eng-1");
        let agents_path = context_dir.join("AGENTS.md");
        assert!(content.contains(&format!("cd '{}'", context_dir.display())));
        assert_eq!(
            content.trim().lines().last().unwrap().trim(),
            "exec codex --dangerously-bypass-approvals-and-sandbox"
        );
        let agents = std::fs::read_to_string(&agents_path).unwrap();
        assert!(agents.contains("role context"));
    }

    #[test]
    fn launch_script_active_codex_uses_dangerous_flag_and_context_overlay() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = tmp.path().join("wt");
        std::fs::create_dir_all(&work_dir).unwrap();

        let cmd = write_launch_script(
            "codex-active-test",
            "codex",
            "work the task",
            Some("role context"),
            &work_dir,
            tmp.path(),
            false,
            false,
            None,
        )
        .unwrap();
        assert!(cmd.contains("batty-launch-codex-active-test.sh"));
        let script_path = std::env::temp_dir().join("batty-launch-codex-active-test.sh");
        let content = std::fs::read_to_string(&script_path).unwrap();
        let context_dir = work_dir
            .join(".batty")
            .join("codex-context")
            .join("codex-active-test");
        let agents_path = context_dir.join("AGENTS.md");
        assert!(content.contains(&format!("cd '{}'", context_dir.display())));
        assert!(
            content
                .contains("exec codex --dangerously-bypass-approvals-and-sandbox 'work the task'")
        );
        let agents = std::fs::read_to_string(&agents_path).unwrap();
        assert!(agents.contains("role context"));
    }

    #[test]
    fn roles_start_idle_by_default() {
        assert!(role_starts_idle());
    }

    #[test]
    fn resumed_idle_member_starts_working() {
        assert_eq!(initial_member_state(true, true), MemberState::Working);
        assert!(should_activate_watcher_on_spawn(true, true));
    }

    #[test]
    fn fresh_idle_member_stays_idle_until_assigned() {
        assert_eq!(initial_member_state(true, false), MemberState::Idle);
        assert!(!should_activate_watcher_on_spawn(true, false));
    }

    #[test]
    fn engineer_task_branch_name_uses_explicit_task_id() {
        assert_eq!(
            engineer_task_branch_name("eng-1-3", "freeform task body", Some(123)),
            "eng-1-3/task-123"
        );
    }

    #[test]
    fn engineer_task_branch_name_extracts_task_id_from_assignment_text() {
        assert_eq!(
            engineer_task_branch_name("eng-1-3", "Task #456: fix move generation", None),
            "eng-1-3/task-456"
        );
    }

    #[test]
    fn engineer_task_branch_name_falls_back_to_slugged_branch() {
        let branch = engineer_task_branch_name("eng-1-3", "Fix castling rights sync", None);
        assert!(branch.starts_with("eng-1-3/task-fix-castling-rights-sy"));
    }

    #[test]
    fn summarize_assignment_uses_first_non_empty_line() {
        assert_eq!(
            summarize_assignment("\n\nTask #9: fix move ordering\n\nDetails below"),
            "Task #9: fix move ordering"
        );
    }

    #[test]
    fn launch_script_escapes_single_quotes() {
        write_launch_script(
            "eng-2",
            "claude",
            "fix the user's bug",
            None,
            Path::new("/tmp"),
            Path::new("/tmp"),
            false,
            false,
            Some("33333333-3333-4333-8333-333333333333"),
        )
        .unwrap();
        let script_path = std::env::temp_dir().join("batty-launch-eng-2.sh");
        let content = std::fs::read_to_string(&script_path).unwrap();
        assert!(content.contains("user'\\''s"));
    }

    #[test]
    fn launch_script_resume_claude_uses_explicit_session_id() {
        write_launch_script(
            "architect",
            "claude",
            "ignored",
            None,
            Path::new("/project"),
            Path::new("/project"),
            true,
            true,
            Some("44444444-4444-4444-8444-444444444444"),
        )
        .unwrap();
        let script_path = std::env::temp_dir().join("batty-launch-architect.sh");
        let content = std::fs::read_to_string(&script_path).unwrap();
        assert!(content.contains(
            "exec claude --dangerously-skip-permissions --resume '44444444-4444-4444-8444-444444444444'"
        ));
    }

    #[test]
    fn strip_nudge_removes_section() {
        let prompt = "# Architect\n\n## Responsibilities\n\n- plan\n\n## Nudge\n\nDo a check-in.\n1. Review work\n2. Update roadmap\n\n## Communication\n\n- talk to manager\n";
        let stripped = strip_nudge_section(prompt);
        assert!(stripped.contains("# Architect"));
        assert!(stripped.contains("## Responsibilities"));
        assert!(stripped.contains("## Communication"));
        assert!(!stripped.contains("## Nudge"));
        assert!(!stripped.contains("Do a check-in"));
    }

    #[test]
    fn strip_nudge_noop_when_absent() {
        let prompt = "# Engineer\n\n## Workflow\n\n- code\n";
        let stripped = strip_nudge_section(prompt);
        assert_eq!(stripped, prompt.trim_end());
    }

    #[test]
    fn extract_nudge_from_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            "# Architect\n\n## Nudge\n\nCheck work.\nUpdate roadmap.\n\n## Other\n\nstuff\n",
        )
        .unwrap();
        let nudge = extract_nudge_section(tmp.path()).unwrap();
        assert!(nudge.contains("Check work."));
        assert!(nudge.contains("Update roadmap."));
        assert!(!nudge.contains("## Other"));
    }

    #[test]
    fn extract_nudge_returns_none_when_absent() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "# Engineer\n\n## Workflow\n\n- code\n").unwrap();
        assert!(extract_nudge_section(tmp.path()).is_none());
    }

    #[test]
    fn format_nudge_status_marks_sent_after_fire() {
        let schedule = NudgeSchedule {
            text: "check in".to_string(),
            interval: Duration::from_secs(600),
            idle_since: Some(Instant::now() - Duration::from_secs(601)),
            fired_this_idle: true,
            paused: false,
        };

        assert_eq!(
            format_nudge_status(Some(&schedule)),
            " #[fg=magenta]nudge sent#[default]"
        );
    }

    #[test]
    fn format_nudge_status_marks_paused_while_member_is_working() {
        let schedule = NudgeSchedule {
            text: "check in".to_string(),
            interval: Duration::from_secs(600),
            idle_since: None,
            fired_this_idle: false,
            paused: true,
        };

        assert_eq!(
            format_nudge_status(Some(&schedule)),
            " #[fg=244]nudge paused#[default]"
        );
    }

    #[test]
    fn format_standup_status_marks_paused_while_member_is_working() {
        assert_eq!(
            format_standup_status(Some(Instant::now()), Duration::from_secs(600), true),
            " #[fg=244]standup paused#[default]"
        );
    }

    #[test]
    fn compose_pane_status_label_shows_pending_inbox_count() {
        let label = compose_pane_status_label(
            MemberState::Idle,
            3,
            2,
            &[191],
            &[193, 194],
            false,
            " #[fg=magenta]nudge 0:30#[default]",
            "",
        );
        assert!(label.contains("idle"));
        assert!(label.contains("inbox 3"));
        assert!(label.contains("triage 2"));
        assert!(label.contains("task 191"));
        assert!(label.contains("review 2"));
        assert!(label.contains("nudge 0:30"));
    }

    #[test]
    fn compose_pane_status_label_shows_zero_inbox_and_pause_state() {
        let label = compose_pane_status_label(MemberState::Working, 0, 0, &[], &[], true, "", "");
        assert!(label.contains("working"));
        assert!(label.contains("inbox 0"));
        assert!(label.contains("PAUSED"));
    }

    #[test]
    fn canonical_agent_name_normalizes_aliases() {
        assert_eq!(canonical_agent_name("claude"), "claude-code");
        assert_eq!(canonical_agent_name("claude-code"), "claude-code");
        assert_eq!(canonical_agent_name("codex"), "codex-cli");
        assert_eq!(canonical_agent_name("codex-cli"), "codex-cli");
    }

    #[test]
    fn launch_state_round_trip_preserves_agent_and_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = HashMap::new();
        state.insert(
            "architect".to_string(),
            LaunchIdentity {
                agent: "claude-code".to_string(),
                prompt: "role prompt".to_string(),
                session_id: Some("11111111-1111-4111-8111-111111111111".to_string()),
            },
        );

        save_launch_state(tmp.path(), &state).unwrap();

        let loaded = load_launch_state(tmp.path());
        assert_eq!(loaded, state);
    }

    #[test]
    fn daemon_state_round_trip_preserves_runtime_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let state = PersistedDaemonState {
            clean_shutdown: false,
            saved_at: 123,
            states: HashMap::from([("eng-1".to_string(), MemberState::Working)]),
            active_tasks: HashMap::from([("eng-1".to_string(), 42)]),
            retry_counts: HashMap::from([("eng-1".to_string(), 2)]),
            paused_standups: HashSet::from(["manager".to_string()]),
            last_standup_elapsed_secs: HashMap::from([("architect".to_string(), 55)]),
            nudge_state: HashMap::from([(
                "eng-1".to_string(),
                PersistedNudgeState {
                    idle_elapsed_secs: Some(88),
                    fired_this_idle: true,
                    paused: false,
                },
            )]),
        };

        save_daemon_state(tmp.path(), &state).unwrap();

        let loaded = load_daemon_state(tmp.path()).unwrap();
        assert_eq!(loaded, state);
    }

    #[test]
    fn should_resume_member_rejects_agent_change() {
        let mut previous = HashMap::new();
        previous.insert(
            "architect".to_string(),
            LaunchIdentity {
                agent: "codex-cli".to_string(),
                prompt: "same prompt".to_string(),
                session_id: None,
            },
        );

        assert!(!should_resume_member(
            true,
            &previous,
            "architect",
            "claude-code",
            "same prompt",
        ));
    }

    #[test]
    fn should_resume_member_rejects_prompt_change() {
        let mut previous = HashMap::new();
        previous.insert(
            "architect".to_string(),
            LaunchIdentity {
                agent: "claude-code".to_string(),
                prompt: "old prompt".to_string(),
                session_id: Some("11111111-1111-4111-8111-111111111111".to_string()),
            },
        );

        assert!(!should_resume_member(
            true,
            &previous,
            "architect",
            "claude-code",
            "new prompt",
        ));
    }

    #[test]
    fn should_resume_member_accepts_matching_launch_identity() {
        let mut previous = HashMap::new();
        previous.insert(
            "architect".to_string(),
            LaunchIdentity {
                agent: "claude-code".to_string(),
                prompt: "same prompt".to_string(),
                session_id: Some("11111111-1111-4111-8111-111111111111".to_string()),
            },
        );

        assert!(should_resume_member(
            true,
            &previous,
            "architect",
            "claude-code",
            "same prompt",
        ));
    }

    #[test]
    fn resolve_member_launch_session_reuses_saved_claude_session_id() {
        let previous = LaunchIdentity {
            agent: "claude-code".to_string(),
            prompt: "same prompt".to_string(),
            session_id: Some("11111111-1111-4111-8111-111111111111".to_string()),
        };

        let (resume, session_id) =
            resolve_member_launch_session("claude-code", Some(&previous), true, true, false);

        assert!(resume);
        assert_eq!(
            session_id.as_deref(),
            Some("11111111-1111-4111-8111-111111111111")
        );
    }

    #[test]
    fn resolve_member_launch_session_starts_fresh_when_claude_session_id_missing() {
        let previous = LaunchIdentity {
            agent: "claude-code".to_string(),
            prompt: "same prompt".to_string(),
            session_id: None,
        };

        let (resume, session_id) =
            resolve_member_launch_session("claude-code", Some(&previous), true, true, false);

        assert!(!resume);
        assert!(session_id.is_some());
    }

    #[test]
    fn resolve_member_launch_session_starts_fresh_when_session_id_is_duplicated() {
        let previous = LaunchIdentity {
            agent: "claude-code".to_string(),
            prompt: "same prompt".to_string(),
            session_id: Some("11111111-1111-4111-8111-111111111111".to_string()),
        };

        let (resume, session_id) =
            resolve_member_launch_session("claude-code", Some(&previous), true, true, true);

        assert!(!resume);
        assert!(session_id.is_some());
        assert_ne!(
            session_id.as_deref(),
            Some("11111111-1111-4111-8111-111111111111")
        );
    }

    #[test]
    fn resolve_member_launch_session_starts_fresh_when_saved_claude_session_is_missing() {
        let previous = LaunchIdentity {
            agent: "claude-code".to_string(),
            prompt: "same prompt".to_string(),
            session_id: Some("11111111-1111-4111-8111-111111111111".to_string()),
        };

        let (resume, session_id) =
            resolve_member_launch_session("claude-code", Some(&previous), true, false, false);

        assert!(!resume);
        assert!(session_id.is_some());
        assert_ne!(
            session_id.as_deref(),
            Some("11111111-1111-4111-8111-111111111111")
        );
    }

    #[test]
    fn claude_session_id_exists_in_finds_exact_session_file() {
        let tmp = tempfile::tempdir().unwrap();
        let projects_root = tmp.path();
        let project_dir = projects_root.join("project-a");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join("11111111-1111-4111-8111-111111111111.jsonl"),
            "{}\n",
        )
        .unwrap();

        assert!(claude_session_id_exists_in(
            projects_root,
            "11111111-1111-4111-8111-111111111111"
        ));
        assert!(!claude_session_id_exists_in(
            projects_root,
            "22222222-2222-4222-8222-222222222222"
        ));
    }

    #[test]
    fn member_session_tracker_config_uses_engineer_worktree_for_claude() {
        let tmp = tempfile::tempdir().unwrap();
        let member = MemberInstance {
            name: "eng-1-1".to_string(),
            role_name: "engineer".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: true,
        };

        let tracker = member_session_tracker_config(tmp.path(), &member);

        assert!(matches!(
            tracker,
            Some(SessionTrackerConfig::Claude { cwd })
                if cwd == tmp
                    .path()
                    .join(".batty")
                    .join("worktrees")
                    .join("eng-1-1")
        ));
    }

    #[test]
    fn queue_daemon_message_routes_to_channel_for_user_roles() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: Vec::new(),
                pane_map: HashMap::new(),
            },
            watchers: HashMap::new(),
            states: HashMap::new(),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

        let sent = Arc::new(Mutex::new(Vec::new()));
        daemon.channels.insert(
            "human".to_string(),
            Box::new(RecordingChannel {
                messages: Arc::clone(&sent),
            }),
        );

        daemon
            .queue_daemon_message("human", "Assignment delivered.")
            .unwrap();

        assert_eq!(sent.lock().unwrap().as_slice(), ["Assignment delivered."]);
    }

    #[test]
    fn queue_daemon_message_ignores_event_sink_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: Vec::new(),
                pane_map: HashMap::new(),
            },
            watchers: HashMap::new(),
            states: HashMap::new(),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::from_writer(
                tmp.path().join("broken-events.jsonl").as_path(),
                FailingWriter,
            ),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

        let sent = Arc::new(Mutex::new(Vec::new()));
        daemon.channels.insert(
            "human".to_string(),
            Box::new(RecordingChannel {
                messages: Arc::clone(&sent),
            }),
        );

        daemon
            .queue_daemon_message("human", "Event sink can fail without breaking delivery.")
            .unwrap();

        assert_eq!(
            sent.lock().unwrap().as_slice(),
            ["Event sink can fail without breaking delivery."]
        );
    }

    #[test]
    fn maybe_notify_failure_patterns_routes_severe_patterns_to_manager_and_architect() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = vec![
            RoleDef {
                name: "architect".to_string(),
                role_type: RoleType::Architect,
                agent: Some("claude".to_string()),
                instances: 1,
                prompt: None,
                talks_to: vec![],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                use_worktrees: false,
            },
            RoleDef {
                name: "manager".to_string(),
                role_type: RoleType::Manager,
                agent: Some("claude".to_string()),
                instances: 1,
                prompt: None,
                talks_to: vec![],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                use_worktrees: false,
            },
        ];
        let mut config = daemon_config_with_roles(&tmp, roles);
        config.members = vec![
            MemberInstance {
                name: "architect".to_string(),
                role_name: "architect".to_string(),
                role_type: RoleType::Architect,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: Some("human".to_string()),
                use_worktrees: false,
            },
            MemberInstance {
                name: "manager".to_string(),
                role_name: "manager".to_string(),
                role_type: RoleType::Manager,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: Some("architect".to_string()),
                use_worktrees: false,
            },
        ];

        let mut daemon = TeamDaemon::new(config).unwrap();
        for index in 0..5 {
            let mut event = TeamEvent::task_escalated("eng-1", &format!("{}", 100 + index));
            event.ts = index as u64 + 1;
            daemon.emit_event(event);
        }

        daemon.maybe_notify_failure_patterns().unwrap();

        let root = inbox::inboxes_root(tmp.path());
        let manager_messages = inbox::pending_messages(&root, "manager").unwrap();
        let architect_messages = inbox::pending_messages(&root, "architect").unwrap();

        assert_eq!(manager_messages.len(), 1);
        assert_eq!(architect_messages.len(), 1);
        assert!(manager_messages[0].body.contains("Review blockers"));
        assert!(architect_messages[0].body.contains("Review blockers"));
    }

    #[test]
    fn drain_legacy_command_queue_preserves_failed_commands() {
        let tmp = tempfile::tempdir().unwrap();
        let queue_path = message::command_queue_path(tmp.path());
        message::enqueue_command(
            &queue_path,
            &message::QueuedCommand::Send {
                from: "architect".into(),
                to: "human".into(),
                message: "status".into(),
            },
        )
        .unwrap();
        message::enqueue_command(
            &queue_path,
            &message::QueuedCommand::Assign {
                from: "manager".into(),
                engineer: "eng-1".into(),
                task: "Task #7: recover".into(),
            },
        )
        .unwrap();

        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: vec![RoleDef {
                        name: "human".to_string(),
                        role_type: RoleType::User,
                        agent: None,
                        instances: 1,
                        prompt: None,
                        talks_to: vec![],
                        channel: Some("telegram".to_string()),
                        channel_config: Some(ChannelConfig {
                            target: "123".to_string(),
                            provider: "fake".to_string(),
                            bot_token: None,
                            allowed_user_ids: vec![],
                        }),
                        nudge_interval_secs: None,
                        receives_standup: None,
                        standup_interval_secs: None,
                        owns: Vec::new(),
                        use_worktrees: false,
                    }],
                },
                session: "test".to_string(),
                members: vec![MemberInstance {
                    name: "eng-1".to_string(),
                    role_name: "eng-1".to_string(),
                    role_type: RoleType::Engineer,
                    agent: Some("claude".to_string()),
                    prompt: None,
                    reports_to: None,
                    use_worktrees: false,
                }],
                pane_map: HashMap::new(),
            },
            watchers: HashMap::new(),
            states: HashMap::new(),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::from([(
                "human".to_string(),
                Box::new(FailingChannel) as Box<dyn Channel>,
            )]),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

        daemon.drain_legacy_command_queue().unwrap();

        let remaining = message::read_command_queue(&queue_path).unwrap();
        assert_eq!(remaining.len(), 1);
        match &remaining[0] {
            message::QueuedCommand::Send { to, message, .. } => {
                assert_eq!(to, "human");
                assert_eq!(message, "status");
            }
            other => panic!("expected failed send command to remain queued, got {other:?}"),
        }

        let engineer_pending =
            inbox::pending_messages(&inbox::inboxes_root(tmp.path()), "eng-1").unwrap();
        assert_eq!(engineer_pending.len(), 1);
        assert_eq!(engineer_pending[0].from, "manager");
        assert!(engineer_pending[0].body.contains("Task #7: recover"));
    }

    #[test]
    fn deliver_inbox_messages_reports_failed_assignment_without_crashing() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = vec![
            RoleDef {
                name: "manager".to_string(),
                role_type: RoleType::Manager,
                agent: Some("claude".to_string()),
                instances: 1,
                prompt: None,
                talks_to: vec![],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                use_worktrees: false,
            },
            RoleDef {
                name: "eng-1".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("claude".to_string()),
                instances: 1,
                prompt: None,
                talks_to: vec![],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                use_worktrees: false,
            },
        ];
        let members = vec![
            MemberInstance {
                name: "manager".to_string(),
                role_name: "manager".to_string(),
                role_type: RoleType::Manager,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: None,
                use_worktrees: false,
            },
            MemberInstance {
                name: "eng-1".to_string(),
                role_name: "eng-1".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: Some("manager".to_string()),
                use_worktrees: false,
            },
        ];

        let mut pane_map = HashMap::new();
        pane_map.insert("eng-1".to_string(), "%999".to_string());

        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                roles,
            },
            session: "test".to_string(),
            members,
            pane_map,
        })
        .unwrap();

        let root = inbox::inboxes_root(tmp.path());
        let assign = inbox::InboxMessage::new_assign("manager", "eng-1", "Task #13: fix it");
        let id = inbox::deliver_to_inbox(&root, &assign).unwrap();

        daemon.deliver_inbox_messages().unwrap();

        let engineer_pending = inbox::pending_messages(&root, "eng-1").unwrap();
        assert!(engineer_pending.is_empty());

        let engineer_all = inbox::all_messages(&root, "eng-1").unwrap();
        assert!(
            engineer_all
                .iter()
                .any(|(msg, delivered)| msg.id == id && *delivered)
        );

        let manager_pending = inbox::pending_messages(&root, "manager").unwrap();
        assert_eq!(manager_pending.len(), 1);
        assert_eq!(manager_pending[0].from, "daemon");
        assert!(manager_pending[0].body.contains("Assignment failed."));
        assert!(manager_pending[0].body.contains("Engineer: eng-1"));
        assert!(manager_pending[0].body.contains("Message ID:"));
        let result = crate::team::load_assignment_result(tmp.path(), &id)
            .unwrap()
            .unwrap();
        assert_eq!(result.status, AssignmentResultStatus::Failed);
        assert_eq!(result.engineer, "eng-1");
        assert_eq!(daemon.states.get("eng-1"), None);
    }

    #[test]
    fn test_auto_dispatch_filters_idle_engineers_only() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = vec![
            RoleDef {
                name: "architect".to_string(),
                role_type: RoleType::Architect,
                agent: Some("claude".to_string()),
                instances: 1,
                prompt: None,
                talks_to: vec![],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                use_worktrees: false,
            },
            RoleDef {
                name: "manager".to_string(),
                role_type: RoleType::Manager,
                agent: Some("claude".to_string()),
                instances: 1,
                prompt: None,
                talks_to: vec![],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                use_worktrees: false,
            },
            RoleDef {
                name: "eng-1".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("claude".to_string()),
                instances: 1,
                prompt: None,
                talks_to: vec![],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                use_worktrees: false,
            },
            RoleDef {
                name: "eng-2".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("claude".to_string()),
                instances: 1,
                prompt: None,
                talks_to: vec![],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                use_worktrees: false,
            },
        ];
        let members = vec![
            MemberInstance {
                name: "architect".to_string(),
                role_name: "architect".to_string(),
                role_type: RoleType::Architect,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: None,
                use_worktrees: false,
            },
            MemberInstance {
                name: "manager".to_string(),
                role_name: "manager".to_string(),
                role_type: RoleType::Manager,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: Some("architect".to_string()),
                use_worktrees: false,
            },
            MemberInstance {
                name: "eng-1".to_string(),
                role_name: "eng-1".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: Some("manager".to_string()),
                use_worktrees: false,
            },
            MemberInstance {
                name: "eng-2".to_string(),
                role_name: "eng-2".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: Some("manager".to_string()),
                use_worktrees: false,
            },
        ];

        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                roles,
            },
            session: "test".to_string(),
            members,
            pane_map: HashMap::new(),
        })
        .unwrap();

        daemon
            .states
            .insert("architect".to_string(), MemberState::Idle);
        daemon
            .states
            .insert("manager".to_string(), MemberState::Idle);
        daemon.states.insert("eng-1".to_string(), MemberState::Idle);
        daemon
            .states
            .insert("eng-2".to_string(), MemberState::Working);

        let board_dir = tmp.path().join(".batty").join("team_config").join("board");
        let tasks_dir = board_dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join("001-auto-task.md"),
            "---\nid: 1\ntitle: auto-task\nstatus: todo\npriority: high\nclass: standard\n---\n\nTask description.\n",
        )
        .unwrap();

        assert_eq!(daemon.idle_engineer_names(), vec!["eng-1".to_string()]);
        let task = next_unclaimed_task(&board_dir).unwrap().unwrap();
        assert_eq!(task.id, 1);
    }

    #[test]
    fn test_maybe_auto_dispatch_respects_rate_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: Vec::new(),
                pane_map: HashMap::new(),
            },
            watchers: HashMap::new(),
            states: HashMap::new(),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

        let before = daemon.last_auto_dispatch;
        daemon.maybe_auto_dispatch().unwrap();
        assert_eq!(daemon.last_auto_dispatch, before);
    }

    #[test]
    fn test_maybe_auto_dispatch_skips_when_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig {
                        auto_dispatch: false,
                        ..BoardConfig::default()
                    },
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: Vec::new(),
                pane_map: HashMap::new(),
            },
            watchers: HashMap::new(),
            states: HashMap::new(),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now() - Duration::from_secs(30),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

        let before = daemon.last_auto_dispatch;
        daemon.maybe_auto_dispatch().unwrap();
        assert_eq!(daemon.last_auto_dispatch, before);
    }

    #[test]
    fn test_retry_count_increments_and_resets() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: Vec::new(),
                pane_map: HashMap::new(),
            },
            watchers: HashMap::new(),
            states: HashMap::new(),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

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
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: Vec::new(),
                pane_map: HashMap::new(),
            },
            watchers: HashMap::new(),
            states: HashMap::new(),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

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
        let daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: Vec::new(),
                pane_map: HashMap::new(),
            },
            watchers: HashMap::new(),
            states: HashMap::new(),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

        assert_eq!(daemon.active_task_id("eng-1"), None);
    }

    #[test]
    fn test_handle_completion_routes_engineers_with_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng-1".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: vec![engineer],
                pane_map: HashMap::new(),
            },
            watchers: HashMap::new(),
            states: HashMap::new(),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

        daemon.active_tasks.insert("eng-1".into(), 42);
        daemon.handle_engineer_completion("eng-1").unwrap();
        assert_eq!(daemon.active_task_id("eng-1"), Some(42));
    }

    #[test]
    fn nonfatal_kanban_failures_are_relayed_to_known_members() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: vec![manager],
                pane_map: HashMap::new(),
            },
            watchers: HashMap::new(),
            states: HashMap::new(),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

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
    fn queue_message_falls_back_to_inbox_when_live_delivery_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: vec![manager],
                pane_map: HashMap::from([("manager".to_string(), "%999".to_string())]),
            },
            watchers: HashMap::new(),
            states: HashMap::new(),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

        daemon
            .queue_message("eng-1", "manager", "Need review on merge handling.")
            .unwrap();

        let messages =
            inbox::pending_messages(&inbox::inboxes_root(tmp.path()), "manager").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].from, "eng-1");
        assert!(messages[0].body.contains("Need review on merge handling."));
    }

    #[test]
    fn handle_engineer_completion_escalates_merge_failures_without_crashing() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        write_task_file(&repo, 42, "merge-blocked-task");

        std::fs::write(repo.join("journal.md"), "base\n").unwrap();
        git_ok(&repo, &["add", "journal.md"]);
        git_ok(&repo, &["commit", "-m", "add journal"]);

        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("journal.md"), "engineer version\n").unwrap();
        git_ok(&worktree_dir, &["add", "journal.md"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer update"]);

        std::fs::write(repo.join("journal.md"), "dirty main\n").unwrap();

        let members = vec![
            MemberInstance {
                name: "manager".to_string(),
                role_name: "manager".to_string(),
                role_type: RoleType::Manager,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: None,
                use_worktrees: false,
            },
            MemberInstance {
                name: "eng-1".to_string(),
                role_name: "eng-1".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: Some("manager".to_string()),
                use_worktrees: true,
            },
        ];

        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: repo.clone(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members,
                pane_map: HashMap::new(),
            },
            watchers: HashMap::new(),
            states: HashMap::new(),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(
                &repo.join(".batty").join("team_config").join("events.jsonl"),
            )
            .unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

        daemon.active_tasks.insert("eng-1".to_string(), 42);
        daemon
            .states
            .insert("eng-1".to_string(), MemberState::Working);

        daemon.handle_engineer_completion("eng-1").unwrap();

        assert_eq!(daemon.active_task_id("eng-1"), None);
        assert_eq!(daemon.states.get("eng-1"), Some(&MemberState::Idle));

        let manager_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
        assert_eq!(manager_messages.len(), 1);
        assert_eq!(manager_messages[0].from, "daemon");
        assert!(
            manager_messages[0]
                .body
                .contains("could not be merged to main")
        );
        assert!(
            manager_messages[0]
                .body
                .contains("would be overwritten by merge")
                || manager_messages[0]
                    .body
                    .contains("Please commit your changes or stash them")
        );

        let engineer_messages =
            inbox::pending_messages(&inbox::inboxes_root(&repo), "eng-1").unwrap();
        assert_eq!(engineer_messages.len(), 1);
        assert_eq!(engineer_messages[0].from, "daemon");
        assert!(
            engineer_messages[0]
                .body
                .contains("could not merge it into main")
        );
    }

    #[test]
    #[serial]
    fn restart_dead_members_respawns_member_and_records_event() {
        let session = "batty-test-restart-dead-member";
        let _ = crate::tmux::kill_session(session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "architect-restart";
        let fake_bin = std::env::temp_dir().join(format!("batty-bin-{member_name}"));
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
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                roles: Vec::new(),
            },
            session: session.to_string(),
            members: vec![member],
            pane_map: HashMap::from([(member_name.to_string(), pane_id.clone())]),
        })
        .unwrap();
        daemon
            .states
            .insert(member_name.to_string(), MemberState::Working);

        crate::tmux::send_keys(&pane_id, "exit", true).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(crate::tmux::pane_dead(&pane_id).unwrap());

        daemon.restart_dead_members().unwrap();
        std::thread::sleep(Duration::from_millis(700));

        assert!(!crate::tmux::pane_dead(&pane_id).unwrap_or(true));
        assert_eq!(daemon.states.get(member_name), Some(&MemberState::Idle));

        let log = (0..20)
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
        assert!(events.contains("\"event\":\"member_crashed\""));
        assert!(events.contains(&format!("\"role\":\"{member_name}\"")));
        assert!(events.contains("\"restart\":true"));

        crate::tmux::kill_session(session).unwrap();
        let _ = std::fs::remove_dir_all(&fake_bin);
    }

    #[test]
    fn mark_member_working_updates_state_and_watcher() {
        let tmp = tempfile::tempdir().unwrap();
        let mut watchers = HashMap::new();
        watchers.insert(
            "architect".to_string(),
            SessionWatcher::new("%0", "architect", 300, None),
        );

        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: Vec::new(),
                pane_map: HashMap::new(),
            },
            watchers,
            states: HashMap::new(),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

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
    fn automation_timers_pause_while_working_and_restart_on_idle() {
        let tmp = tempfile::tempdir().unwrap();
        let member = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let role = RoleDef {
            name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            instances: 1,
            prompt: None,
            talks_to: vec![],
            channel: None,
            channel_config: None,
            nudge_interval_secs: None,
            receives_standup: Some(true),
            standup_interval_secs: Some(600),
            owns: Vec::new(),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: vec![role],
                },
                session: "test".to_string(),
                members: vec![member],
                pane_map: HashMap::new(),
            },
            watchers: HashMap::new(),
            states: HashMap::new(),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::from([(
                "manager".to_string(),
                NudgeSchedule {
                    text: "check in".to_string(),
                    interval: Duration::from_secs(600),
                    idle_since: Some(Instant::now() - Duration::from_secs(90)),
                    fired_this_idle: false,
                    paused: false,
                },
            )]),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::from([(
                "manager".to_string(),
                Instant::now() - Duration::from_secs(120),
            )]),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

        daemon.update_automation_timers_for_state("manager", MemberState::Working);

        let paused_nudge = daemon.nudges.get("manager").unwrap();
        assert!(paused_nudge.paused);
        assert!(paused_nudge.idle_since.is_none());
        assert!(daemon.paused_standups.contains("manager"));
        assert!(!daemon.last_standup.contains_key("manager"));

        daemon.update_automation_timers_for_state("manager", MemberState::Idle);

        let restarted_nudge = daemon.nudges.get("manager").unwrap();
        assert!(!restarted_nudge.paused);
        assert!(!restarted_nudge.fired_this_idle);
        assert!(restarted_nudge.idle_since.unwrap().elapsed() < Duration::from_secs(1));
        assert!(!daemon.paused_standups.contains("manager"));
        assert!(daemon.last_standup["manager"].elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn maybe_generate_standup_skips_when_global_interval_is_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let member = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let role = RoleDef {
            name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            instances: 1,
            prompt: None,
            talks_to: vec![],
            channel: None,
            channel_config: None,
            nudge_interval_secs: None,
            receives_standup: Some(true),
            standup_interval_secs: Some(600),
            owns: Vec::new(),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig {
                        interval_secs: 0,
                        output_lines: 30,
                    },
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: false,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: vec![role],
                },
                session: "test".to_string(),
                members: vec![member],
                pane_map: HashMap::new(),
            },
            watchers: HashMap::new(),
            states: HashMap::new(),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

        daemon.maybe_generate_standup().unwrap();

        assert!(daemon.last_standup.is_empty());
    }

    #[test]
    fn maybe_generate_standup_writes_user_report_to_file_without_telegram_bot() {
        let tmp = tempfile::tempdir().unwrap();
        let user = MemberInstance {
            name: "user".to_string(),
            role_name: "user".to_string(),
            role_type: RoleType::User,
            agent: None,
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let architect = MemberInstance {
            name: "architect".to_string(),
            role_name: "architect".to_string(),
            role_type: RoleType::Architect,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("user".to_string()),
            use_worktrees: false,
        };
        let user_role = RoleDef {
            name: "user".to_string(),
            role_type: RoleType::User,
            agent: None,
            instances: 1,
            prompt: None,
            talks_to: vec!["architect".to_string()],
            channel: None,
            channel_config: None,
            nudge_interval_secs: None,
            receives_standup: Some(true),
            standup_interval_secs: Some(1),
            owns: Vec::new(),
            use_worktrees: false,
        };
        let architect_role = RoleDef {
            name: "architect".to_string(),
            role_type: RoleType::Architect,
            agent: Some("claude".to_string()),
            instances: 1,
            prompt: None,
            talks_to: vec![],
            channel: None,
            channel_config: None,
            nudge_interval_secs: None,
            receives_standup: Some(false),
            standup_interval_secs: None,
            owns: Vec::new(),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig {
                        interval_secs: 1,
                        output_lines: 30,
                    },
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: false,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: vec![user_role, architect_role],
                },
                session: "test".to_string(),
                members: vec![user.clone(), architect],
                pane_map: HashMap::new(),
            },
            watchers: HashMap::new(),
            states: HashMap::from([("architect".to_string(), MemberState::Working)]),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::from([(
                user.name.clone(),
                Instant::now() - Duration::from_secs(5),
            )]),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

        daemon.maybe_generate_standup().unwrap();

        let standups_dir = tmp.path().join(".batty").join("standups");
        let entries = std::fs::read_dir(&standups_dir)
            .unwrap()
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(entries.len(), 1);

        let report = std::fs::read_to_string(entries[0].path()).unwrap();
        assert!(report.contains("=== STANDUP for user ==="));
        assert!(report.contains("[architect] status: working"));

        let events = std::fs::read_to_string(tmp.path().join("events.jsonl")).unwrap();
        assert!(events.contains("\"event\":\"standup_generated\""));
        assert!(events.contains("\"recipient\":\"user\""));
    }

    #[test]
    #[serial]
    fn maybe_fire_nudges_marks_member_working_after_live_delivery() {
        let session = "batty-test-nudge-live-delivery";
        let _ = crate::tmux::kill_session(session);

        crate::tmux::create_session(session, "cat", &[], "/tmp").unwrap();
        let pane_id = crate::tmux::pane_id(session).unwrap();
        std::thread::sleep(Duration::from_millis(100));

        let tmp = tempfile::tempdir().unwrap();
        let member = MemberInstance {
            name: "scientist".to_string(),
            role_name: "scientist".to_string(),
            role_type: RoleType::Architect,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let mut watchers = HashMap::new();
        watchers.insert(
            "scientist".to_string(),
            SessionWatcher::new(&pane_id, "scientist", 300, None),
        );
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: session.to_string(),
                members: vec![member],
                pane_map: HashMap::from([("scientist".to_string(), pane_id.clone())]),
            },
            watchers,
            states: HashMap::from([("scientist".to_string(), MemberState::Idle)]),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::from([(
                "scientist".to_string(),
                NudgeSchedule {
                    text: "Please make progress.".to_string(),
                    interval: Duration::from_secs(1),
                    idle_since: Some(Instant::now() - Duration::from_secs(5)),
                    fired_this_idle: false,
                    paused: false,
                },
            )]),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

        backdate_idle_grace(&mut daemon, "scientist");
        daemon.maybe_fire_nudges().unwrap();

        assert_eq!(daemon.states.get("scientist"), Some(&MemberState::Working));
        let schedule = daemon.nudges.get("scientist").unwrap();
        assert!(schedule.paused);
        assert!(schedule.idle_since.is_none());
        assert!(!schedule.fired_this_idle);

        crate::tmux::kill_session(session).unwrap();
    }

    #[test]
    #[serial]
    fn maybe_intervene_triage_backlog_marks_member_working_after_live_delivery() {
        let session = "batty-test-triage-live-delivery";
        let _ = crate::tmux::kill_session(session);

        crate::tmux::create_session(session, "cat", &[], "/tmp").unwrap();
        let pane_id = crate::tmux::pane_id(session).unwrap();
        std::thread::sleep(Duration::from_millis(100));

        let tmp = tempfile::tempdir().unwrap();
        let lead = MemberInstance {
            name: "lead".to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: Some("lead".to_string()),
            use_worktrees: false,
        };
        let mut watchers = HashMap::new();
        watchers.insert(
            "lead".to_string(),
            SessionWatcher::new(&pane_id, "lead", 300, None),
        );
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: session.to_string(),
                members: vec![lead, engineer],
                pane_map: HashMap::from([("lead".to_string(), pane_id.clone())]),
            },
            watchers,
            states: HashMap::from([("lead".to_string(), MemberState::Idle)]),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        inbox::init_inbox(&root, "eng-1").unwrap();
        let mut result = inbox::InboxMessage::new_send("eng-1", "lead", "Task complete.");
        result.timestamp = 42;
        let id = inbox::deliver_to_inbox(&root, &result).unwrap();
        inbox::mark_delivered(&root, "lead", &id).unwrap();

        daemon.update_automation_timers_for_state("lead", MemberState::Working);
        daemon.update_automation_timers_for_state("lead", MemberState::Idle);
        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_triage_backlog().unwrap();

        assert_eq!(daemon.triage_interventions.get("lead"), Some(&1));
        if daemon.states.get("lead") == Some(&MemberState::Working) {
            let pane = tmux::capture_pane(&pane_id).unwrap_or_default();
            assert!(pane.contains("batty inbox lead"));
            assert!(pane.contains("batty read lead <ref>"));
            assert!(pane.contains("batty send eng-1"));
            assert!(pane.contains("batty assign eng-1"));
            assert!(pane.contains("batty send architect"));
            assert!(pane.contains("next time you become idle"));
        } else {
            let pending = inbox::pending_messages(&root, "lead").unwrap();
            assert_eq!(pending.len(), 1);
            assert!(pending[0].body.contains("batty inbox lead"));
        }

        crate::tmux::kill_session(session).unwrap();
    }

    #[test]
    fn maybe_intervene_triage_backlog_queues_when_live_delivery_falls_back_to_inbox() {
        let tmp = tempfile::tempdir().unwrap();
        let lead = MemberInstance {
            name: "lead".to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: Some("lead".to_string()),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: vec![lead, engineer],
                pane_map: HashMap::from([("lead".to_string(), "%9999999".to_string())]),
            },
            watchers: HashMap::new(),
            states: HashMap::from([("lead".to_string(), MemberState::Idle)]),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        inbox::init_inbox(&root, "eng-1").unwrap();
        let mut result = inbox::InboxMessage::new_send("eng-1", "lead", "Task complete.");
        result.timestamp = 42;
        let id = inbox::deliver_to_inbox(&root, &result).unwrap();
        inbox::mark_delivered(&root, "lead", &id).unwrap();

        daemon.update_automation_timers_for_state("lead", MemberState::Working);
        daemon.update_automation_timers_for_state("lead", MemberState::Idle);
        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_triage_backlog().unwrap();

        assert_eq!(daemon.states.get("lead"), Some(&MemberState::Idle));
        assert_eq!(daemon.triage_interventions.get("lead"), Some(&1));
        let pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].from, "architect");
        assert!(pending[0].body.contains("Triage backlog detected"));
        assert!(pending[0].body.contains("batty inbox lead"));
        assert!(pending[0].body.contains("batty read lead <ref>"));
        assert!(pending[0].body.contains("batty send eng-1"));
        assert!(pending[0].body.contains("batty assign eng-1"));
        assert!(pending[0].body.contains("batty send architect"));
        assert!(pending[0].body.contains("next time you become idle"));
    }

    #[test]
    fn maybe_intervene_triage_backlog_does_not_fire_on_startup_idle() {
        let tmp = tempfile::tempdir().unwrap();
        let lead = MemberInstance {
            name: "lead".to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: Some("lead".to_string()),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: vec![lead, engineer],
                pane_map: HashMap::from([("lead".to_string(), "%999".to_string())]),
            },
            watchers: HashMap::new(),
            states: HashMap::from([("lead".to_string(), MemberState::Idle)]),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        inbox::init_inbox(&root, "eng-1").unwrap();
        let mut result = inbox::InboxMessage::new_send("eng-1", "lead", "Task complete.");
        result.timestamp = 42;
        let id = inbox::deliver_to_inbox(&root, &result).unwrap();
        inbox::mark_delivered(&root, "lead", &id).unwrap();

        daemon.maybe_intervene_triage_backlog().unwrap();

        assert!(daemon.triage_interventions.get("lead").is_none());
        assert!(inbox::pending_messages(&root, "lead").unwrap().is_empty());
        assert_eq!(daemon.states.get("lead"), Some(&MemberState::Idle));
    }

    #[test]
    fn maybe_intervene_owned_tasks_queues_when_idle_member_owns_unfinished_task() {
        let tmp = tempfile::tempdir().unwrap();
        let lead = MemberInstance {
            name: "lead".to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: Some("lead".to_string()),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: vec![lead, engineer],
                pane_map: HashMap::from([("lead".to_string(), "%999".to_string())]),
            },
            watchers: HashMap::new(),
            states: HashMap::from([("lead".to_string(), MemberState::Idle)]),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        write_owned_task_file(tmp.path(), 191, "owned-task", "in-progress", "lead");

        daemon.update_automation_timers_for_state("lead", MemberState::Working);
        daemon.update_automation_timers_for_state("lead", MemberState::Idle);
        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_owned_tasks().unwrap();

        assert_eq!(daemon.states.get("lead"), Some(&MemberState::Idle));
        assert_eq!(
            daemon
                .owned_task_interventions
                .get("lead")
                .map(|state| state.idle_epoch),
            Some(1)
        );
        let pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].from, "architect");
        assert!(pending[0].body.contains("Task #191"));
        assert!(
            pending[0]
                .body
                .contains("Owned active task backlog detected")
        );
        assert!(pending[0].body.contains("kanban-md list --dir"));
        assert!(pending[0].body.contains("kanban-md show --dir"));
        assert!(pending[0].body.contains("191"));
        assert!(pending[0].body.contains("sed -n '1,220p'"));
        assert!(pending[0].body.contains("batty assign eng-1"));
        assert!(pending[0].body.contains("batty send architect"));
        assert!(pending[0].body.contains("kanban-md move --dir"));
        assert!(pending[0].body.contains("next time you become idle"));
    }

    #[test]
    fn maybe_intervene_owned_tasks_fires_for_persistent_startup_idle_state() {
        let tmp = tempfile::tempdir().unwrap();
        let lead = MemberInstance {
            name: "lead".to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: Some("lead".to_string()),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: vec![lead, engineer],
                pane_map: HashMap::from([("lead".to_string(), "%999".to_string())]),
            },
            watchers: HashMap::new(),
            states: HashMap::from([("lead".to_string(), MemberState::Idle)]),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        write_owned_task_file(tmp.path(), 191, "owned-task", "in-progress", "lead");

        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_owned_tasks().unwrap();

        let pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].from, "architect");
        assert!(pending[0].body.contains("Task #191"));
        assert_eq!(
            daemon
                .owned_task_interventions
                .get("lead")
                .map(|state| state.idle_epoch),
            Some(0)
        );
        assert_eq!(daemon.states.get("lead"), Some(&MemberState::Idle));
    }

    #[test]
    fn maybe_intervene_owned_tasks_waits_for_idle_grace() {
        let tmp = tempfile::tempdir().unwrap();
        let lead = MemberInstance {
            name: "lead".to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: vec![lead],
                pane_map: HashMap::from([("lead".to_string(), "%999".to_string())]),
            },
            watchers: HashMap::new(),
            states: HashMap::from([("lead".to_string(), MemberState::Idle)]),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        write_owned_task_file(tmp.path(), 191, "owned-task", "in-progress", "lead");

        daemon.update_automation_timers_for_state("lead", MemberState::Idle);
        daemon.maybe_intervene_owned_tasks().unwrap();
        assert!(inbox::pending_messages(&root, "lead").unwrap().is_empty());

        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_owned_tasks().unwrap();
        assert_eq!(inbox::pending_messages(&root, "lead").unwrap().len(), 1);
    }

    #[test]
    fn maybe_intervene_owned_tasks_skips_when_pending_inbox_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let lead = MemberInstance {
            name: "lead".to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: vec![lead],
                pane_map: HashMap::from([("lead".to_string(), "%999".to_string())]),
            },
            watchers: HashMap::new(),
            states: HashMap::from([("lead".to_string(), MemberState::Idle)]),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        let message = inbox::InboxMessage::new_send("architect", "lead", "Check this first.");
        inbox::deliver_to_inbox(&root, &message).unwrap();
        write_owned_task_file(tmp.path(), 191, "owned-task", "in-progress", "lead");

        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_owned_tasks().unwrap();

        let pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].from, "architect");
        assert!(
            daemon.owned_task_interventions.get("lead").is_none(),
            "pending inbox should block new interventions"
        );
    }

    #[test]
    fn maybe_intervene_owned_tasks_ignores_review_only_claims() {
        let tmp = tempfile::tempdir().unwrap();
        let lead = MemberInstance {
            name: "lead".to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: vec![lead],
                pane_map: HashMap::from([("lead".to_string(), "%999".to_string())]),
            },
            watchers: HashMap::new(),
            states: HashMap::from([("lead".to_string(), MemberState::Idle)]),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        write_owned_task_file(tmp.path(), 191, "review-task", "review", "lead");

        daemon.update_automation_timers_for_state("lead", MemberState::Working);
        daemon.update_automation_timers_for_state("lead", MemberState::Idle);
        daemon.maybe_intervene_owned_tasks().unwrap();

        assert!(daemon.owned_task_interventions.get("lead").is_none());
        assert!(inbox::pending_messages(&root, "lead").unwrap().is_empty());
    }

    #[test]
    fn maybe_intervene_owned_tasks_dedupes_same_active_signature_across_idle_epochs() {
        let tmp = tempfile::tempdir().unwrap();
        let lead = MemberInstance {
            name: "lead".to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: vec![lead],
                pane_map: HashMap::from([("lead".to_string(), "%999".to_string())]),
            },
            watchers: HashMap::new(),
            states: HashMap::from([("lead".to_string(), MemberState::Idle)]),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        write_owned_task_file(tmp.path(), 191, "owned-task", "in-progress", "lead");

        daemon.update_automation_timers_for_state("lead", MemberState::Working);
        daemon.update_automation_timers_for_state("lead", MemberState::Idle);
        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_owned_tasks().unwrap();

        daemon
            .states
            .insert("lead".to_string(), MemberState::Working);
        daemon.update_automation_timers_for_state("lead", MemberState::Working);
        daemon.states.insert("lead".to_string(), MemberState::Idle);
        daemon.update_automation_timers_for_state("lead", MemberState::Idle);
        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_owned_tasks().unwrap();

        let pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            daemon
                .owned_task_interventions
                .get("lead")
                .map(|state| state.idle_epoch),
            Some(2)
        );
    }

    #[test]
    fn auto_retro_fires_when_all_done() {
        let tmp = tempfile::tempdir().unwrap();
        let events_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        write_owned_task_file(tmp.path(), 45, "retro-task", "done", "eng-1");
        write_event_log(
            tmp.path(),
            &[
                TeamEvent::daemon_started(),
                TeamEvent::task_assigned("eng-1", "45"),
                TeamEvent::task_completed("eng-1"),
                TeamEvent::daemon_stopped(),
            ],
        );

        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: false,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: Vec::new(),
                pane_map: HashMap::new(),
            },
            watchers: HashMap::new(),
            states: HashMap::new(),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            event_sink: EventSink::new(&events_path).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            poll_interval: Duration::from_secs(5),
        };

        daemon.maybe_generate_retrospective().unwrap();

        assert!(daemon.retro_generated);
        let retro_dir = tmp.path().join(".batty").join("retrospectives");
        let reports = std::fs::read_dir(&retro_dir).unwrap().count();
        assert_eq!(reports, 1);

        let events = super::super::events::read_events(&events_path).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event == "retro_generated")
                .count(),
            1
        );
    }

    #[test]
    fn auto_retro_does_not_fire_twice() {
        let tmp = tempfile::tempdir().unwrap();
        let events_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        write_owned_task_file(tmp.path(), 45, "retro-task", "done", "eng-1");
        write_event_log(
            tmp.path(),
            &[
                TeamEvent::daemon_started(),
                TeamEvent::task_assigned("eng-1", "45"),
                TeamEvent::task_completed("eng-1"),
                TeamEvent::daemon_stopped(),
            ],
        );

        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: false,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: Vec::new(),
                pane_map: HashMap::new(),
            },
            watchers: HashMap::new(),
            states: HashMap::new(),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            event_sink: EventSink::new(&events_path).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            poll_interval: Duration::from_secs(5),
        };

        daemon.maybe_generate_retrospective().unwrap();
        daemon.maybe_generate_retrospective().unwrap();

        let retro_dir = tmp.path().join(".batty").join("retrospectives");
        let reports = std::fs::read_dir(&retro_dir).unwrap().count();
        assert_eq!(reports, 1);

        let events = super::super::events::read_events(&events_path).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event == "retro_generated")
                .count(),
            1
        );
    }

    #[test]
    fn maybe_intervene_review_backlog_queues_for_idle_manager_with_branch_and_worktree_context() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let team_config_dir = repo.join(".batty").join("team_config");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
        write_owned_task_file(&repo, 191, "review-task", "review", "eng-1");

        let lead = MemberInstance {
            name: "lead".to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: Some("lead".to_string()),
            use_worktrees: true,
        };
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: repo.clone(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: vec![lead, engineer],
                pane_map: HashMap::from([("lead".to_string(), "%999".to_string())]),
            },
            watchers: HashMap::new(),
            states: HashMap::from([("lead".to_string(), MemberState::Idle)]),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(
                &repo.join(".batty").join("team_config").join("events.jsonl"),
            )
            .unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

        let root = inbox::inboxes_root(&repo);
        inbox::init_inbox(&root, "lead").unwrap();

        daemon.update_automation_timers_for_state("lead", MemberState::Working);
        daemon.update_automation_timers_for_state("lead", MemberState::Idle);
        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_review_backlog().unwrap();

        let pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].from, "architect");
        assert!(pending[0].body.contains("Review backlog detected"));
        assert!(pending[0].body.contains("#191 by eng-1"));
        assert!(pending[0].body.contains("batty inbox lead"));
        assert!(pending[0].body.contains("batty read lead <ref>"));
        assert!(pending[0].body.contains("batty merge eng-1"));
        assert!(pending[0].body.contains("kanban-md move --dir"));
        assert!(pending[0].body.contains("191 done"));
        assert!(pending[0].body.contains("191 archived"));
        assert!(pending[0].body.contains("191 in-progress"));
        assert!(pending[0].body.contains("batty assign eng-1"));
        assert!(pending[0].body.contains("batty send architect"));
        assert!(
            pending[0]
                .body
                .contains(worktree_dir.to_string_lossy().as_ref())
        );
        assert!(pending[0].body.contains("branch: eng-1"));
        assert_eq!(
            daemon
                .owned_task_interventions
                .get("review::lead")
                .map(|state| state.idle_epoch),
            Some(1)
        );
    }

    #[test]
    fn maybe_intervene_review_backlog_does_not_fire_on_startup_idle() {
        let tmp = tempfile::tempdir().unwrap();
        write_owned_task_file(tmp.path(), 191, "review-task", "review", "eng-1");

        let lead = MemberInstance {
            name: "lead".to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: Some("lead".to_string()),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: vec![lead, engineer],
                pane_map: HashMap::from([("lead".to_string(), "%999".to_string())]),
            },
            watchers: HashMap::new(),
            states: HashMap::from([("lead".to_string(), MemberState::Idle)]),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();

        daemon.maybe_intervene_review_backlog().unwrap();

        assert!(inbox::pending_messages(&root, "lead").unwrap().is_empty());
        assert!(
            daemon
                .owned_task_interventions
                .get("review::lead")
                .is_none()
        );
    }

    #[test]
    fn maybe_intervene_manager_dispatch_gap_queues_for_idle_lead_with_idle_reports() {
        let tmp = tempfile::tempdir().unwrap();
        let architect = MemberInstance {
            name: "architect".to_string(),
            role_name: "architect".to_string(),
            role_type: RoleType::Architect,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let lead = MemberInstance {
            name: "lead".to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let eng_1 = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: Some("lead".to_string()),
            use_worktrees: false,
        };
        let eng_2 = MemberInstance {
            name: "eng-2".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: Some("lead".to_string()),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: vec![architect, lead, eng_1, eng_2],
                pane_map: HashMap::from([("lead".to_string(), "%999".to_string())]),
            },
            watchers: HashMap::new(),
            states: HashMap::from([
                ("lead".to_string(), MemberState::Idle),
                ("eng-1".to_string(), MemberState::Idle),
                ("eng-2".to_string(), MemberState::Idle),
            ]),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        inbox::init_inbox(&root, "eng-1").unwrap();
        inbox::init_inbox(&root, "eng-2").unwrap();
        write_owned_task_file(tmp.path(), 191, "active-task", "in-progress", "eng-1");
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::write(
            tasks_dir.join("192-open-task.md"),
            "---\nid: 192\ntitle: open-task\nstatus: todo\npriority: high\nclass: standard\n---\n\nTask description.\n",
        )
        .unwrap();

        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_manager_dispatch_gap().unwrap();

        let pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].from, "architect");
        assert!(pending[0].body.contains("Dispatch recovery needed"));
        assert!(pending[0].body.contains("eng-1 on #191"));
        assert!(pending[0].body.contains("eng-2"));
        assert!(pending[0].body.contains("batty status"));
        assert!(pending[0].body.contains("batty send eng-1"));
        assert!(pending[0].body.contains("batty assign eng-2"));
        assert!(pending[0].body.contains("batty send architect"));
        assert!(
            daemon
                .owned_task_interventions
                .contains_key("dispatch::lead")
        );
    }

    #[test]
    fn maybe_intervene_architect_utilization_queues_for_underloaded_idle_architect() {
        let tmp = tempfile::tempdir().unwrap();
        let architect = MemberInstance {
            name: "architect".to_string(),
            role_name: "architect".to_string(),
            role_type: RoleType::Architect,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let lead = MemberInstance {
            name: "lead".to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let eng_1 = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: Some("lead".to_string()),
            use_worktrees: false,
        };
        let eng_2 = MemberInstance {
            name: "eng-2".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: Some("lead".to_string()),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: vec![architect, lead, eng_1, eng_2],
                pane_map: HashMap::from([("architect".to_string(), "%999".to_string())]),
            },
            watchers: HashMap::new(),
            states: HashMap::from([
                ("architect".to_string(), MemberState::Idle),
                ("lead".to_string(), MemberState::Idle),
                ("eng-1".to_string(), MemberState::Idle),
                ("eng-2".to_string(), MemberState::Idle),
            ]),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "architect").unwrap();
        write_owned_task_file(tmp.path(), 191, "active-task", "in-progress", "eng-1");
        let tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::write(
            tasks_dir.join("192-open-task.md"),
            "---\nid: 192\ntitle: open-task\nstatus: backlog\npriority: high\nclass: standard\n---\n\nTask description.\n",
        )
        .unwrap();

        backdate_idle_grace(&mut daemon, "architect");
        daemon.maybe_intervene_architect_utilization().unwrap();

        let pending = inbox::pending_messages(&root, "architect").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].from, "daemon");
        assert!(pending[0].body.contains("Utilization recovery needed"));
        assert!(pending[0].body.contains("eng-1 on #191"));
        assert!(pending[0].body.contains("eng-2"));
        assert!(pending[0].body.contains("batty status"));
        assert!(pending[0].body.contains("batty send lead"));
        assert!(pending[0].body.contains("Start Task #192 on eng-2"));
        assert!(
            daemon
                .owned_task_interventions
                .contains_key("utilization::architect")
        );
    }

    #[test]
    fn maybe_intervene_triage_backlog_does_not_refire_while_prior_intervention_remains_pending() {
        let tmp = tempfile::tempdir().unwrap();
        let lead = MemberInstance {
            name: "lead".to_string(),
            role_name: "lead".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: Some("lead".to_string()),
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: vec![lead, engineer],
                pane_map: HashMap::from([("lead".to_string(), "%999".to_string())]),
            },
            watchers: HashMap::new(),
            states: HashMap::from([("lead".to_string(), MemberState::Idle)]),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        inbox::init_inbox(&root, "eng-1").unwrap();
        let mut result = inbox::InboxMessage::new_send("eng-1", "lead", "Task complete.");
        result.timestamp = 42;
        let id = inbox::deliver_to_inbox(&root, &result).unwrap();
        inbox::mark_delivered(&root, "lead", &id).unwrap();

        daemon.update_automation_timers_for_state("lead", MemberState::Working);
        daemon.update_automation_timers_for_state("lead", MemberState::Idle);
        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_triage_backlog().unwrap();

        daemon
            .states
            .insert("lead".to_string(), MemberState::Working);
        daemon.update_automation_timers_for_state("lead", MemberState::Working);
        daemon.states.insert("lead".to_string(), MemberState::Idle);
        daemon.update_automation_timers_for_state("lead", MemberState::Idle);
        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_triage_backlog().unwrap();

        assert_eq!(daemon.triage_interventions.get("lead"), Some(&1));
        let pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending.iter().all(|message| message.from == "architect"));
    }

    #[test]
    fn maybe_fire_nudges_keeps_member_idle_when_delivery_falls_back_to_inbox() {
        let tmp = tempfile::tempdir().unwrap();
        let member = MemberInstance {
            name: "scientist".to_string(),
            role_name: "scientist".to_string(),
            role_type: RoleType::Architect,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: vec![member],
                pane_map: HashMap::from([("scientist".to_string(), "%999".to_string())]),
            },
            watchers: HashMap::new(),
            states: HashMap::from([("scientist".to_string(), MemberState::Idle)]),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::from([(
                "scientist".to_string(),
                NudgeSchedule {
                    text: "Please make progress.".to_string(),
                    interval: Duration::from_secs(1),
                    idle_since: Some(Instant::now() - Duration::from_secs(5)),
                    fired_this_idle: false,
                    paused: false,
                },
            )]),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

        backdate_idle_grace(&mut daemon, "scientist");
        daemon.maybe_fire_nudges().unwrap();

        assert_eq!(daemon.states.get("scientist"), Some(&MemberState::Idle));
        let schedule = daemon.nudges.get("scientist").unwrap();
        assert!(!schedule.paused);
        assert!(schedule.fired_this_idle);

        let messages =
            inbox::pending_messages(&inbox::inboxes_root(tmp.path()), "scientist").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].from, "daemon");
        assert!(messages[0].body.contains("Please make progress."));
    }

    #[test]
    fn maybe_fire_nudges_skips_when_pending_inbox_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let member = MemberInstance {
            name: "scientist".to_string(),
            role_name: "scientist".to_string(),
            role_type: RoleType::Architect,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: None,
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: vec![member],
                pane_map: HashMap::from([("scientist".to_string(), "%999".to_string())]),
            },
            watchers: HashMap::new(),
            states: HashMap::from([("scientist".to_string(), MemberState::Idle)]),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::from([(
                "scientist".to_string(),
                NudgeSchedule {
                    text: "Please make progress.".to_string(),
                    interval: Duration::from_secs(1),
                    idle_since: Some(Instant::now()),
                    fired_this_idle: false,
                    paused: false,
                },
            )]),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "scientist").unwrap();
        let message =
            inbox::InboxMessage::new_send("architect", "scientist", "Process this first.");
        inbox::deliver_to_inbox(&root, &message).unwrap();

        backdate_idle_grace(&mut daemon, "scientist");
        daemon.maybe_fire_nudges().unwrap();

        let messages = inbox::pending_messages(&root, "scientist").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].from, "architect");
        let schedule = daemon.nudges.get("scientist").unwrap();
        assert!(!schedule.fired_this_idle);
        assert_eq!(daemon.states.get("scientist"), Some(&MemberState::Idle));
    }

    #[test]
    fn automation_sender_prefers_direct_manager_and_config_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy::default(),
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    automation: AutomationConfig::default(),
                    automation_sender: Some("human".to_string()),
                    orchestrator_pane: true,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: vec![
                    MemberInstance {
                        name: "architect".to_string(),
                        role_name: "architect".to_string(),
                        role_type: RoleType::Architect,
                        agent: Some("claude".to_string()),
                        prompt: None,
                        reports_to: None,
                        use_worktrees: false,
                    },
                    MemberInstance {
                        name: "lead".to_string(),
                        role_name: "lead".to_string(),
                        role_type: RoleType::Manager,
                        agent: Some("claude".to_string()),
                        prompt: None,
                        reports_to: Some("architect".to_string()),
                        use_worktrees: false,
                    },
                    MemberInstance {
                        name: "eng-1".to_string(),
                        role_name: "eng".to_string(),
                        role_type: RoleType::Engineer,
                        agent: Some("codex".to_string()),
                        prompt: None,
                        reports_to: Some("lead".to_string()),
                        use_worktrees: false,
                    },
                ],
                pane_map: HashMap::new(),
            },
            watchers: HashMap::new(),
            states: HashMap::new(),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            retro_generated: false,
            poll_interval: Duration::from_secs(5),
        };

        assert_eq!(daemon.automation_sender_for("eng-1"), "lead");
        assert_eq!(daemon.automation_sender_for("lead"), "architect");
        assert_eq!(daemon.automation_sender_for("architect"), "human");

        daemon.config.team_config.automation_sender = None;
        assert_eq!(daemon.automation_sender_for("architect"), "daemon");
    }

    #[test]
    fn append_orchestrator_log_line_writes_timestamped_activity() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".batty").join("orchestrator.log");
        append_orchestrator_log_line(&path, "dispatch: assigned task #18").unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("dispatch: assigned task #18"));
        assert!(content.starts_with('['));
    }

    #[test]
    fn record_orchestrator_action_is_noop_when_orchestrator_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                roles: Vec::new(),
            },
            session: "test".to_string(),
            members: Vec::new(),
            pane_map: HashMap::new(),
        })
        .unwrap();

        daemon.record_orchestrator_action("dispatch: no-op");

        assert!(!tmp.path().join(".batty").join("orchestrator.log").exists());
    }

    #[test]
    fn record_orchestrator_action_writes_when_orchestrator_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                workflow_mode: WorkflowMode::Hybrid,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                roles: Vec::new(),
            },
            session: "test".to_string(),
            members: Vec::new(),
            pane_map: HashMap::new(),
        })
        .unwrap();

        daemon.record_orchestrator_action("dispatch: active");

        let content =
            fs::read_to_string(tmp.path().join(".batty").join("orchestrator.log")).unwrap();
        assert!(content.contains("dispatch: active"));
    }

    /// Helper: build a minimal DaemonConfig with the given roles.
    fn daemon_config_with_roles(tmp: &tempfile::TempDir, roles: Vec<RoleDef>) -> DaemonConfig {
        DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                roles,
            },
            session: "test".to_string(),
            members: Vec::new(),
            pane_map: HashMap::new(),
        }
    }

    #[test]
    fn daemon_creates_telegram_bot_when_configured() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = vec![RoleDef {
            name: "user".to_string(),
            role_type: RoleType::User,
            agent: None,
            instances: 1,
            prompt: None,
            talks_to: vec!["architect".to_string()],
            channel: Some("telegram".to_string()),
            channel_config: Some(ChannelConfig {
                target: "12345".to_string(),
                provider: "telegram".to_string(),
                bot_token: Some("test-token-123".to_string()),
                allowed_user_ids: vec![42],
            }),
            nudge_interval_secs: None,
            receives_standup: None,
            standup_interval_secs: None,
            owns: Vec::new(),
            use_worktrees: false,
        }];

        let config = daemon_config_with_roles(&tmp, roles);
        let daemon = TeamDaemon::new(config).unwrap();
        assert!(
            daemon.telegram_bot.is_some(),
            "telegram_bot should be Some when user role has bot_token"
        );
    }

    #[test]
    fn daemon_no_telegram_bot_without_config() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = vec![RoleDef {
            name: "user".to_string(),
            role_type: RoleType::User,
            agent: None,
            instances: 1,
            prompt: None,
            talks_to: vec!["architect".to_string()],
            channel: None,
            channel_config: None,
            nudge_interval_secs: None,
            receives_standup: None,
            standup_interval_secs: None,
            owns: Vec::new(),
            use_worktrees: false,
        }];

        let config = daemon_config_with_roles(&tmp, roles);
        let daemon = TeamDaemon::new(config).unwrap();
        assert!(
            daemon.telegram_bot.is_none(),
            "telegram_bot should be None when no bot_token configured"
        );
    }
}
