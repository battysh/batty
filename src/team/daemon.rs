//! Core team daemon — polling loop, agent lifecycle, message routing.
//!
//! The daemon ties together all team subsystems: it spawns agents in tmux
//! panes, monitors their output via `SessionWatcher`, routes messages between
//! roles, generates periodic standups, and emits structured events.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

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
use super::merge;
use super::message;
use super::standup::MemberState;
use super::status;
use super::task_cmd;
use super::task_loop::{
    engineer_base_branch_name, next_unclaimed_task, prepare_engineer_assignment_worktree,
    setup_engineer_worktree,
};
use super::watcher::{SessionTrackerConfig, SessionWatcher, WatcherState};
use super::{AssignmentDeliveryResult, AssignmentResultStatus, now_unix, store_assignment_result};
use crate::agent;
use crate::tmux;

#[path = "daemon/interventions.rs"]
mod interventions;

pub(crate) use self::interventions::NudgeSchedule;
use self::interventions::OwnedTaskInterventionState;

/// Daemon configuration derived from TeamConfig.
pub struct DaemonConfig {
    pub project_root: PathBuf,
    pub team_config: TeamConfig,
    pub session: String,
    pub members: Vec<MemberInstance>,
    pub pane_map: HashMap<String, String>,
}

const DELIVERY_VERIFICATION_CAPTURE_LINES: u32 = 50;
const FAILED_DELIVERY_RETRY_DELAY: Duration = Duration::from_secs(30);
const FAILED_DELIVERY_MAX_ATTEMPTS: u32 = 3;
const CONTEXT_RESTART_COOLDOWN: Duration = Duration::from_secs(30);

const HOT_RELOAD_CHECK_INTERVAL: Duration = Duration::from_secs(30);
const HOT_RELOAD_MIN_INTERVAL: Duration = Duration::from_secs(30);

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
    intervention_cooldowns: HashMap<String, Instant>,
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
    pipeline_starvation_fired: bool,
    retro_generated: bool,
    failed_deliveries: Vec<FailedDelivery>,
    poll_interval: Duration,
}

#[derive(Debug, Clone)]
struct FailedDelivery {
    recipient: String,
    from: String,
    body: String,
    attempts: u32,
    last_attempt: Instant,
}

impl FailedDelivery {
    fn new(recipient: &str, from: &str, body: &str) -> Self {
        Self {
            recipient: recipient.to_string(),
            from: from.to_string(),
            body: body.to_string(),
            attempts: 1,
            last_attempt: Instant::now(),
        }
    }

    fn message_marker(&self) -> String {
        message_delivery_marker(&self.from)
    }

    fn is_ready_for_retry(&self, now: Instant) -> bool {
        now.duration_since(self.last_attempt) >= FAILED_DELIVERY_RETRY_DELAY
    }

    fn has_attempts_remaining(&self) -> bool {
        self.attempts < FAILED_DELIVERY_MAX_ATTEMPTS
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MemberWorktreeContext {
    path: PathBuf,
    branch: Option<String>,
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
    pipeline_starvation_fired: bool,
}

#[derive(Debug, Clone)]
struct MemberLaunchPlan {
    short_cmd: String,
    identity: LaunchIdentity,
    initial_state: MemberState,
    activate_watcher: bool,
    resume_summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageDelivery {
    Channel,
    LivePane,
    InboxQueued,
    SkippedUnknownRecipient,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BinaryFingerprint {
    path: PathBuf,
    modified: SystemTime,
    len: u64,
    #[cfg(unix)]
    inode: u64,
}

impl BinaryFingerprint {
    fn capture(path: &Path) -> Result<Self> {
        let metadata =
            fs::metadata(path).with_context(|| format!("failed to stat {}", path.display()))?;
        let modified = metadata
            .modified()
            .with_context(|| format!("failed to read mtime for {}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            modified,
            len: metadata.len(),
            #[cfg(unix)]
            inode: std::os::unix::fs::MetadataExt::ino(&metadata),
        })
    }

    fn changed_from(&self, previous: &Self) -> bool {
        self.modified != previous.modified || self.len != previous.len || {
            #[cfg(unix)]
            {
                self.inode != previous.inode
            }
            #[cfg(not(unix))]
            {
                false
            }
        }
    }
}

#[derive(Debug, Clone)]
struct HotReloadMonitor {
    binary: BinaryFingerprint,
    last_checked: Instant,
    last_reload_attempt: Option<Instant>,
}

impl HotReloadMonitor {
    fn new(binary: BinaryFingerprint) -> Self {
        Self {
            binary,
            last_checked: Instant::now(),
            last_reload_attempt: None,
        }
    }

    fn for_current_exe() -> Result<Self> {
        let path = std::env::current_exe().context("failed to resolve current executable")?;
        Ok(Self::new(BinaryFingerprint::capture(&path)?))
    }

    fn should_check(&self) -> bool {
        self.last_checked.elapsed() >= HOT_RELOAD_CHECK_INTERVAL
    }

    fn changed_binary(&mut self) -> Result<Option<BinaryFingerprint>> {
        self.last_checked = Instant::now();
        let current = BinaryFingerprint::capture(&self.binary.path)?;
        Ok(current.changed_from(&self.binary).then_some(current))
    }

    fn can_attempt_reload(&self) -> bool {
        self.last_reload_attempt
            .map(|instant| instant.elapsed() >= HOT_RELOAD_MIN_INTERVAL)
            .unwrap_or(true)
    }

    fn mark_reload_attempt(&mut self) {
        self.last_reload_attempt = Some(Instant::now());
    }
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
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
            poll_interval: Duration::from_secs(5),
        })
    }

    /// Run the daemon loop. Blocks until the session is killed or an error occurs.
    ///
    /// If `resume` is true, agents are launched with session-resume flags
    /// (`claude --resume <session-id>` / `codex resume --last`) instead of fresh starts.
    pub fn run(&mut self, resume: bool) -> Result<()> {
        self.emit_event(TeamEvent::daemon_started());
        self.acknowledge_hot_reload_marker();
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
        let mut hot_reload = match HotReloadMonitor::for_current_exe() {
            Ok(monitor) => Some(monitor),
            Err(error) => {
                warn!(error = %error, "failed to initialize daemon hot-reload monitor");
                None
            }
        };

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
            self.run_loop_step("retry_failed_deliveries", |daemon| {
                daemon.retry_failed_deliveries()
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
            self.run_loop_step("maybe_auto_unblock_blocked_tasks", |daemon| {
                daemon.maybe_auto_unblock_blocked_tasks()
            });
            self.run_loop_step("maybe_auto_dispatch", |daemon| daemon.maybe_auto_dispatch());
            self.run_loop_step("maybe_intervene_manager_dispatch_gap", |daemon| {
                daemon.maybe_intervene_manager_dispatch_gap()
            });
            self.run_loop_step("maybe_intervene_architect_utilization", |daemon| {
                daemon.maybe_intervene_architect_utilization()
            });
            self.run_loop_step("maybe_detect_pipeline_starvation", |daemon| {
                daemon.maybe_detect_pipeline_starvation()
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
            self.run_loop_step("maybe_reload_binary", |daemon| {
                daemon.maybe_hot_reload_binary(hot_reload.as_mut())
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

    pub(super) fn emit_event(&mut self, event: TeamEvent) {
        self.failure_window.push(&event);
        if let Err(error) = self.event_sink.emit(event) {
            warn!(error = %error, "failed to write daemon event; continuing");
        }
    }

    fn acknowledge_hot_reload_marker(&mut self) {
        if !consume_hot_reload_marker(&self.config.project_root) {
            return;
        }

        self.emit_event(TeamEvent::daemon_reloaded());
        self.record_orchestrator_action("runtime: daemon hot-reloaded");
        info!("daemon restarted via hot reload");
    }

    fn maybe_hot_reload_binary(&mut self, monitor: Option<&mut HotReloadMonitor>) -> Result<()> {
        let Some(monitor) = monitor else {
            return Ok(());
        };
        if !monitor.should_check() {
            return Ok(());
        }

        let Some(updated_binary) = monitor.changed_binary()? else {
            return Ok(());
        };

        if !monitor.can_attempt_reload() {
            warn!(
                path = %updated_binary.path.display(),
                "binary changed again but reload attempt is rate-limited"
            );
            return Ok(());
        }

        if !binary_is_reloadable(&updated_binary.path) {
            warn!(
                path = %updated_binary.path.display(),
                "binary changed but is not safe to hot-reload yet"
            );
            return Ok(());
        }

        monitor.mark_reload_attempt();
        self.persist_runtime_state(false)?;
        self.emit_event(TeamEvent::daemon_reloading());
        self.record_orchestrator_action(format!(
            "runtime: daemon reloading after binary change ({})",
            updated_binary.path.display()
        ));
        write_hot_reload_marker(&self.config.project_root)?;

        if let Err(error) = exec_reloaded_daemon(&updated_binary.path, &self.config.project_root) {
            let _ = clear_hot_reload_marker(&self.config.project_root);
            warn!(
                path = %updated_binary.path.display(),
                error = %error,
                "failed to exec updated daemon binary; continuing on existing process"
            );
            self.record_orchestrator_action(format!("runtime: daemon reload failed ({error})"));
        }

        Ok(())
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
        let resume_summary = format_resume_decision_summary(
            &member.name,
            &normalized_agent,
            previous_identity,
            resume,
            &prompt_text,
            claude_session_available,
            previous_identity
                .and_then(|identity| identity.session_id.as_deref())
                .is_some_and(|existing| duplicate_claude_session_ids.contains(existing)),
            member_resume,
            session_id.as_deref(),
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
            resume_summary,
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

    fn handle_context_exhaustion(&mut self, member_name: &str) -> Result<()> {
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
        if let Err(error) = self.queue_message("daemon", member_name, &restart_notice) {
            warn!(member = %member_name, error = %error, "failed to inject restart notice");
        }
        self.record_orchestrator_action(format!(
            "restart: relaunched {} on task #{} after context exhaustion",
            member_name, task.id
        ));
        self.intervention_cooldowns
            .insert(restart_cooldown_key, Instant::now());
        self.emit_event(TeamEvent::agent_restarted(
            member_name,
            &task.id.to_string(),
            "context_exhausted",
            prior_restarts + 1,
        ));
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
        self.emit_event(TeamEvent::task_escalated(
            &member.name,
            &task.id.to_string(),
        ));
        Ok(())
    }

    /// Spawn the correct agent in each member's pane.
    fn spawn_all_agents(&mut self, resume: bool) -> Result<()> {
        let previous_launch_state = load_launch_state(&self.config.project_root);
        let duplicate_claude_session_ids = duplicate_claude_session_ids(&previous_launch_state);
        let mut next_launch_state = HashMap::new();
        let mut resume_summaries = Vec::new();

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
                    resume_summaries.push(plan.resume_summary.clone());
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

        if !resume_summaries.is_empty() {
            self.record_orchestrator_action(format!("resume: {}", resume_summaries.join(", ")));
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
            let prev_watcher_state = self
                .watchers
                .get(name)
                .map(|watcher| watcher.state)
                .unwrap_or(WatcherState::Idle);
            let (new_state, completion_observed, session_size_bytes) = {
                let watcher = self.watchers.get_mut(name).unwrap();
                match watcher.poll() {
                    Ok(new_state) => (
                        new_state,
                        watcher.take_completion_event(),
                        watcher.current_session_size_bytes(),
                    ),
                    Err(e) => {
                        warn!(member = %name, error = %e, "watcher poll failed");
                        continue;
                    }
                }
            };

            let member_state = match new_state {
                WatcherState::Active => MemberState::Working,
                WatcherState::Idle => MemberState::Idle,
                WatcherState::ContextExhausted => MemberState::Working,
            };

            if prev_state != Some(member_state) {
                self.states.insert(name.clone(), member_state);

                // Update automation countdowns on state transitions.
                self.update_automation_timers_for_state(name, member_state);
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
                    self.emit_event(TeamEvent::context_exhausted(
                        name,
                        task_id,
                        session_size_bytes,
                    ));
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

            if completion_observed && self.active_task_id(name).is_some() {
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

    pub(super) fn mark_member_working(&mut self, member_name: &str) {
        self.states
            .insert(member_name.to_string(), MemberState::Working);
        if let Some(watcher) = self.watchers.get_mut(member_name) {
            watcher.activate();
        }
        self.update_automation_timers_for_state(member_name, MemberState::Working);
    }

    pub(super) fn set_member_idle(&mut self, member_name: &str) {
        self.states
            .insert(member_name.to_string(), MemberState::Idle);
        if let Some(watcher) = self.watchers.get_mut(member_name) {
            watcher.deactivate();
        }
        self.update_automation_timers_for_state(member_name, MemberState::Idle);
    }

    fn verify_message_content_in_pane(&self, pane_id: &str, message_marker: &str) -> bool {
        match tmux::capture_pane_recent(pane_id, DELIVERY_VERIFICATION_CAPTURE_LINES) {
            Ok(capture) => capture_contains_message_marker(&capture, message_marker),
            Err(error) => {
                warn!(
                    pane_id,
                    error = %error,
                    "failed to capture pane for content-based delivery verification"
                );
                false
            }
        }
    }

    fn record_failed_delivery(&mut self, recipient: &str, from: &str, body: &str) {
        if let Some(existing) = self.failed_deliveries.iter_mut().find(|delivery| {
            delivery.recipient == recipient && delivery.from == from && delivery.body == body
        }) {
            existing.last_attempt = Instant::now();
            return;
        }

        self.failed_deliveries
            .push(FailedDelivery::new(recipient, from, body));
    }

    fn clear_failed_delivery(&mut self, recipient: &str, from: &str, body: &str) {
        self.failed_deliveries.retain(|delivery| {
            delivery.recipient != recipient || delivery.from != from || delivery.body != body
        });
    }

    fn failed_delivery_escalation_recipient(&self, recipient: &str) -> Option<String> {
        self.config
            .members
            .iter()
            .find(|member| member.name == recipient)
            .and_then(|member| member.reports_to.clone())
            .or_else(|| {
                self.config
                    .members
                    .iter()
                    .find(|member| {
                        member.role_type == RoleType::Manager && member.name != recipient
                    })
                    .map(|member| member.name.clone())
            })
            .or_else(|| {
                let sender = self.automation_sender_for(recipient);
                (sender != recipient
                    && self
                        .config
                        .members
                        .iter()
                        .any(|member| member.name == sender))
                .then_some(sender)
            })
    }

    fn escalate_failed_delivery(&mut self, delivery: &FailedDelivery) -> Result<()> {
        let Some(manager) = self.failed_delivery_escalation_recipient(&delivery.recipient) else {
            warn!(
                recipient = %delivery.recipient,
                from = %delivery.from,
                "failed delivery exhausted retries without escalation target"
            );
            return Ok(());
        };

        let body = format!(
            "Live message delivery failed after {} attempts.\nRecipient: {}\nFrom: {}\nMarker: {}\nMessage body:\n{}",
            delivery.attempts,
            delivery.recipient,
            delivery.from,
            delivery.message_marker(),
            delivery.body
        );
        let root = inbox::inboxes_root(&self.config.project_root);
        let msg = inbox::InboxMessage::new_send("daemon", &manager, &body);
        inbox::deliver_to_inbox(&root, &msg)?;
        self.emit_event(TeamEvent::message_routed("daemon", &manager));
        warn!(
            recipient = %delivery.recipient,
            from = %delivery.from,
            escalation_target = %manager,
            attempts = delivery.attempts,
            "failed delivery escalated to manager inbox"
        );
        Ok(())
    }

    fn retry_failed_deliveries(&mut self) -> Result<()> {
        if self.failed_deliveries.is_empty() {
            return Ok(());
        }

        let now = Instant::now();
        let pending = std::mem::take(&mut self.failed_deliveries);
        for mut delivery in pending {
            if !delivery.is_ready_for_retry(now) {
                self.failed_deliveries.push(delivery);
                continue;
            }

            let is_ready = self
                .watchers
                .get(&delivery.recipient)
                .map(|watcher| matches!(watcher.state, WatcherState::Idle))
                .unwrap_or(true);
            if !is_ready {
                self.failed_deliveries.push(delivery);
                continue;
            }

            let Some(pane_id) = self.config.pane_map.get(&delivery.recipient).cloned() else {
                self.escalate_failed_delivery(&delivery)?;
                continue;
            };

            delivery.attempts += 1;
            delivery.last_attempt = now;
            info!(
                recipient = %delivery.recipient,
                from = %delivery.from,
                attempts = delivery.attempts,
                "retrying failed live delivery"
            );

            let injected = match message::inject_message(&pane_id, &delivery.from, &delivery.body) {
                Ok(()) => true,
                Err(error) => {
                    warn!(
                        recipient = %delivery.recipient,
                        from = %delivery.from,
                        attempts = delivery.attempts,
                        error = %error,
                        "failed to re-inject message during delivery retry"
                    );
                    false
                }
            };

            if injected
                && self.verify_message_delivered(
                    &delivery.from,
                    &delivery.recipient,
                    &delivery.body,
                    3,
                    false,
                )
            {
                continue;
            }

            if delivery.has_attempts_remaining() {
                self.failed_deliveries.push(delivery);
            } else {
                self.escalate_failed_delivery(&delivery)?;
            }
        }

        Ok(())
    }

    /// After injecting a message, verify the injected marker appears in the pane.
    ///
    /// Polls the pane for up to `max_attempts` rounds. If the marker still
    /// does not appear, resends Enter to unstick the submission.
    fn verify_message_delivered(
        &mut self,
        from: &str,
        recipient: &str,
        body: &str,
        max_attempts: u32,
        record_failure: bool,
    ) -> bool {
        let Some(pane_id) = self.config.pane_map.get(recipient).cloned() else {
            return true; // No pane to verify
        };
        let message_marker = message_delivery_marker(from);

        for attempt in 1..=max_attempts {
            std::thread::sleep(Duration::from_secs(2));

            if self.verify_message_content_in_pane(&pane_id, &message_marker) {
                self.clear_failed_delivery(recipient, from, body);
                debug!(
                    recipient,
                    attempt,
                    marker = %message_marker,
                    "message delivery verified: marker found in pane"
                );
                return true;
            }

            warn!(
                recipient,
                attempt,
                marker = %message_marker,
                "message marker missing after injection; resending Enter"
            );
            if let Err(e) = tmux::send_keys(&pane_id, "", true) {
                warn!(recipient, error = %e, "failed to resend Enter");
            }
        }

        if record_failure {
            self.record_failed_delivery(recipient, from, body);
            warn!(
                recipient,
                max_attempts,
                marker = %message_marker,
                "message delivery failed after retries; queued for daemon retry"
            );
        }

        false
    }

    pub(super) fn active_task_id(&self, engineer: &str) -> Option<u32> {
        self.active_tasks.get(engineer).copied()
    }

    pub(super) fn project_root(&self) -> &Path {
        &self.config.project_root
    }

    pub(super) fn worktree_dir(&self, engineer: &str) -> PathBuf {
        self.config
            .project_root
            .join(".batty")
            .join("worktrees")
            .join(engineer)
    }

    pub(super) fn board_dir(&self) -> PathBuf {
        self.config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board")
    }

    pub(super) fn member_uses_worktrees(&self, engineer: &str) -> bool {
        self.config
            .members
            .iter()
            .find(|member| member.name == engineer)
            .map(|member| member.use_worktrees)
            .unwrap_or(false)
    }

    pub(super) fn manager_name(&self, engineer: &str) -> Option<String> {
        self.config
            .members
            .iter()
            .find(|member| member.name == engineer)
            .and_then(|member| member.reports_to.clone())
    }

    #[cfg(test)]
    pub(super) fn set_active_task_for_test(&mut self, engineer: &str, task_id: u32) {
        self.active_tasks.insert(engineer.to_string(), task_id);
    }

    #[cfg(test)]
    pub(super) fn retry_count_for_test(&self, engineer: &str) -> Option<u32> {
        self.retry_counts.get(engineer).copied()
    }

    #[cfg(test)]
    pub(super) fn member_state_for_test(&self, engineer: &str) -> Option<MemberState> {
        self.states.get(engineer).copied()
    }

    #[cfg(test)]
    pub(super) fn set_member_state_for_test(&mut self, engineer: &str, state: MemberState) {
        self.states.insert(engineer.to_string(), state);
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

    fn context_restart_cooldown_key(member_name: &str) -> String {
        format!("context-restart::{member_name}")
    }

    fn context_escalation_cooldown_key(member_name: &str) -> String {
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
        let count = super::events::read_events(&events_path)?
            .into_iter()
            .filter(|event| event.event == "agent_restarted")
            .filter(|event| event.task.as_deref() == Some(task_id.as_str()))
            .count() as u32;
        Ok(count)
    }

    fn restart_assignment_message(task: &crate::task::Task) -> String {
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

    fn launch_task_assignment(
        &mut self,
        engineer: &str,
        task: &str,
        task_id: Option<u32>,
        reset_context: bool,
        emit_task_assigned: bool,
    ) -> Result<AssignmentLaunch> {
        info!(engineer, task, "assigning task");

        let Some(pane_id) = self.config.pane_map.get(engineer).cloned() else {
            bail!("no pane found for engineer '{engineer}'");
        };

        let member = self
            .config
            .members
            .iter()
            .find(|m| m.name == engineer)
            .cloned();
        let agent_name = member
            .as_ref()
            .and_then(|m| m.agent.as_deref())
            .unwrap_or("claude");

        let team_config_dir = self.config.project_root.join(".batty").join("team_config");
        let use_worktrees = member.as_ref().map(|m| m.use_worktrees).unwrap_or(false);
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

        if reset_context {
            let adapter = agent::adapter_from_name(agent_name);
            if let Some(adapter) = &adapter {
                for (keys, enter) in adapter.reset_context_keys() {
                    tmux::send_keys(&pane_id, &keys, enter)?;
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }

        self.ensure_assignment_pane_cwd(engineer, &pane_id, &work_dir)?;

        let role_context = member
            .as_ref()
            .map(|m| strip_nudge_section(&self.load_prompt(m, &team_config_dir)));
        let normalized_agent = canonical_agent_name(agent_name);
        let session_id = new_member_session_id(&normalized_agent);

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

        self.mark_member_working(engineer);

        if emit_task_assigned {
            self.emit_event(TeamEvent::task_assigned(engineer, task));
        }

        Ok(AssignmentLaunch {
            branch: task_branch,
            work_dir,
        })
    }

    fn ensure_assignment_pane_cwd(
        &mut self,
        member_name: &str,
        pane_id: &str,
        expected_dir: &Path,
    ) -> Result<()> {
        let current_path = PathBuf::from(tmux::pane_current_path(pane_id)?);
        let normalized_expected = normalized_assignment_dir(expected_dir);
        if normalized_assignment_dir(&current_path) == normalized_expected {
            return Ok(());
        }

        warn!(
            member = %member_name,
            pane = %pane_id,
            current = %current_path.display(),
            expected = %expected_dir.display(),
            "correcting pane cwd before assignment"
        );

        let command = format!(
            "cd '{}'",
            shell_single_quote(expected_dir.to_string_lossy().as_ref())
        );
        tmux::send_keys(pane_id, &command, true)?;
        std::thread::sleep(Duration::from_millis(200));

        let corrected_path = PathBuf::from(tmux::pane_current_path(pane_id)?);
        if normalized_assignment_dir(&corrected_path) != normalized_expected {
            bail!(
                "failed to correct pane cwd for '{member_name}': expected {}, got {}",
                expected_dir.display(),
                corrected_path.display()
            );
        }

        self.emit_event(TeamEvent::cwd_corrected(
            member_name,
            &expected_dir.display().to_string(),
        ));
        Ok(())
    }

    pub(super) fn increment_retry(&mut self, engineer: &str) -> u32 {
        let count = self.retry_counts.entry(engineer.to_string()).or_insert(0);
        *count += 1;
        *count
    }

    pub(super) fn clear_active_task(&mut self, engineer: &str) {
        self.active_tasks.remove(engineer);
        self.retry_counts.remove(engineer);
    }

    pub(super) fn run_kanban_md_nonfatal<'a, I>(
        &mut self,
        args: &[&str],
        action: &str,
        recipients: I,
    ) -> bool
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

    pub(super) fn queue_message(&mut self, from: &str, recipient: &str, body: &str) -> Result<()> {
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
                    self.verify_message_delivered(from, recipient, body, 3, true);
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

    pub(super) fn notify_reports_to(&mut self, from_role: &str, msg: &str) -> Result<()> {
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
                            self.verify_message_delivered(&msg.from, name, &msg.body, 3, true);
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
        self.launch_task_assignment(engineer, task, task_id, true, true)
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

    fn maybe_auto_unblock_blocked_tasks(&mut self) -> Result<()> {
        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks"))?;
        let done_task_ids: HashSet<u32> = tasks
            .iter()
            .filter(|task| task.status == "done")
            .map(|task| task.id)
            .collect();
        let unblocked_tasks = tasks
            .iter()
            .filter(|task| task.status == "blocked")
            .filter(|task| !task.depends_on.is_empty())
            .filter(|task| {
                task.depends_on
                    .iter()
                    .all(|dependency| done_task_ids.contains(dependency))
            })
            .map(|task| {
                (
                    task.id,
                    task.title.clone(),
                    task.depends_on.clone(),
                    self.auto_unblock_notification_recipient(task),
                )
            })
            .collect::<Vec<_>>();

        for (task_id, title, dependencies, recipient) in unblocked_tasks {
            task_cmd::cmd_transition(&board_dir, task_id, "todo")
                .with_context(|| format!("failed to auto-unblock task #{task_id}"))?;

            let dependency_list = dependencies
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            let event_role = recipient.as_deref().unwrap_or("daemon");
            self.emit_event(TeamEvent::task_unblocked(event_role, &task_id.to_string()));
            self.record_orchestrator_action(format!(
                "dependency resolution: auto-unblocked task #{} ({}) after dependencies [{}] completed",
                task_id, title, dependency_list
            ));
            info!(
                task_id,
                task_title = %title,
                dependencies = %dependency_list,
                recipient = recipient.as_deref().unwrap_or("none"),
                "auto-unblocked blocked task"
            );

            let Some(recipient) = recipient else {
                continue;
            };
            let body = format!(
                "Task #{task_id} ({title}) was automatically moved from `blocked` to `todo` because dependencies [{dependency_list}] are done."
            );
            if let Err(error) = self.queue_daemon_message(&recipient, &body) {
                warn!(
                    task_id,
                    to = %recipient,
                    error = %error,
                    "failed to notify auto-unblocked task recipient"
                );
            }
        }

        Ok(())
    }

    /// Update `@batty_status` on each pane border with state + timer countdowns.
    fn update_pane_status_labels(&self) {
        status::update_pane_status_labels(
            &self.config.project_root,
            &self.config.members,
            &self.config.pane_map,
            &self.states,
            &self.nudges,
            &self.last_standup,
            &self.paused_standups,
            |member_name| self.standup_interval_for_member_name(member_name),
        );
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

    fn manager_for_member_name(&self, member_name: &str) -> Option<&str> {
        self.config
            .members
            .iter()
            .find(|member| member.name == member_name)
            .and_then(|member| member.reports_to.as_deref())
    }

    fn auto_unblock_notification_recipient(&self, task: &crate::task::Task) -> Option<String> {
        task.claimed_by
            .as_deref()
            .filter(|owner| {
                self.config
                    .members
                    .iter()
                    .any(|member| member.name == *owner)
            })
            .map(str::to_string)
            .or_else(|| {
                self.config
                    .members
                    .iter()
                    .find(|member| member.role_type == RoleType::Manager)
                    .map(|member| member.name.clone())
            })
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
        self.pipeline_starvation_fired = state.pipeline_starvation_fired;
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
            pipeline_starvation_fired: self.pipeline_starvation_fired,
        };
        save_daemon_state(&self.config.project_root, &state)
    }

    fn maybe_detect_pipeline_starvation(&mut self) -> Result<()> {
        let Some(threshold) = self
            .config
            .team_config
            .workflow_policy
            .pipeline_starvation_threshold
        else {
            self.pipeline_starvation_fired = false;
            return Ok(());
        };

        let idle_engineers = self.idle_engineer_names();
        let idle_count = idle_engineers.len();
        if idle_count == 0 {
            self.pipeline_starvation_fired = false;
            return Ok(());
        }

        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let todo_count = crate::task::load_tasks_from_dir(&board_dir.join("tasks"))?
            .into_iter()
            .filter(|task| matches!(task.status.as_str(), "todo" | "backlog"))
            .count();

        let deficit = idle_count.saturating_sub(todo_count);
        if todo_count >= idle_count || deficit < threshold {
            self.pipeline_starvation_fired = false;
            return Ok(());
        }
        if self.pipeline_starvation_fired {
            return Ok(());
        }

        let inbox_root = inbox::inboxes_root(&self.config.project_root);
        let architects: Vec<String> = self
            .config
            .members
            .iter()
            .filter(|member| member.role_type == RoleType::Architect)
            .map(|member| member.name.clone())
            .collect();
        if architects.is_empty() {
            return Ok(());
        }

        let message =
            format!("Pipeline running dry: {idle_count} idle engineers, {todo_count} todo tasks.");
        for architect in &architects {
            let visible_sender = self.automation_sender_for(architect);
            let inbox_msg = inbox::InboxMessage::new_send(&visible_sender, architect, &message);
            inbox::deliver_to_inbox(&inbox_root, &inbox_msg)?;
        }
        self.pipeline_starvation_fired = true;
        Ok(())
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
        let generated = status::maybe_generate_standup(
            &self.config.project_root,
            &self.config.team_config,
            &self.config.members,
            &self.watchers,
            &self.states,
            &self.config.pane_map,
            self.telegram_bot.as_ref(),
            &self.paused_standups,
            &mut self.last_standup,
        )?;

        for recipient in generated {
            self.emit_event(TeamEvent::standup_generated(&recipient));
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

fn message_delivery_marker(sender: &str) -> String {
    format!("--- Message from {sender} ---")
}

fn capture_contains_message_marker(capture: &str, message_marker: &str) -> bool {
    capture.contains(message_marker)
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

fn format_stuck_duration(stuck_age_secs: u64) -> String {
    if stuck_age_secs >= 3600 {
        let hours = stuck_age_secs / 3600;
        let mins = (stuck_age_secs % 3600) / 60;
        format!("{hours}h {mins}m")
    } else if stuck_age_secs >= 60 {
        let mins = stuck_age_secs / 60;
        let secs = stuck_age_secs % 60;
        format!("{mins}m {secs}s")
    } else {
        format!("{stuck_age_secs}s")
    }
}
fn append_orchestrator_log_line(path: &Path, message: &str) -> Result<()> {
    use std::io::Write;
    let mut file = super::open_log_for_append(path)?;
    writeln!(file, "[{}] {}", now_unix(), message)?;
    file.flush()?;
    Ok(())
}

fn hot_reload_marker_path(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("reload")
}

fn write_hot_reload_marker(project_root: &Path) -> Result<()> {
    let path = hot_reload_marker_path(project_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(&path, now_unix().to_string())
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn clear_hot_reload_marker(project_root: &Path) -> Result<()> {
    let path = hot_reload_marker_path(project_root);
    if !path.exists() {
        return Ok(());
    }
    fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
    Ok(())
}

fn consume_hot_reload_marker(project_root: &Path) -> bool {
    let path = hot_reload_marker_path(project_root);
    if !path.exists() {
        return false;
    }
    clear_hot_reload_marker(project_root).is_ok()
}

fn hot_reload_daemon_args(project_root: &Path) -> Vec<String> {
    let root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf())
        .to_string_lossy()
        .to_string();
    vec![
        "-v".to_string(),
        "daemon".to_string(),
        "--project-root".to_string(),
        root,
        "--resume".to_string(),
    ]
}

fn binary_is_reloadable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return false;
        }
    }

    #[cfg(target_os = "macos")]
    {
        let Ok(status) = std::process::Command::new("codesign")
            .args(["--verify", path.to_string_lossy().as_ref()])
            .status()
        else {
            return false;
        };
        if !status.success() {
            return false;
        }
    }

    true
}

#[cfg(unix)]
fn exec_reloaded_daemon(executable: &Path, project_root: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let error = std::process::Command::new(executable)
        .args(hot_reload_daemon_args(project_root))
        .exec();
    Err(anyhow::Error::new(error).context(format!("failed to exec {}", executable.display())))
}

#[cfg(not(unix))]
fn exec_reloaded_daemon(_executable: &Path, _project_root: &Path) -> Result<()> {
    bail!("daemon hot reload via exec is only supported on unix")
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
    // Namespace temp paths by project to avoid collisions when multiple batty
    // instances run concurrently (e.g. batty + mafia_solver both have "architect").
    let project_slug = project_root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "default".to_string());
    let script_path =
        std::env::temp_dir().join(format!("batty-launch-{project_slug}-{member_name}.sh"));
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
    let wrapper_dir = std::env::temp_dir().join(format!("batty-bin-{project_slug}-{member_name}"));
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

fn normalized_assignment_dir(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn shell_single_quote(value: &str) -> String {
    value.replace('\'', "'\"'\"'")
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

fn format_resume_decision_summary(
    member_name: &str,
    current_agent: &str,
    previous_identity: Option<&LaunchIdentity>,
    resume_requested: bool,
    current_prompt: &str,
    claude_session_available: bool,
    duplicate_session_id: bool,
    member_resume: bool,
    session_id: Option<&str>,
) -> String {
    let decision = if member_resume { "yes" } else { "no" };
    let reason = if !resume_requested {
        "resume disabled".to_string()
    } else if let Some(previous) = previous_identity {
        if previous.agent != current_agent {
            "agent changed".to_string()
        } else if previous.prompt != current_prompt {
            "prompt changed".to_string()
        } else if duplicate_session_id {
            "session duplicated".to_string()
        } else if previous.session_id.is_some() && !claude_session_available {
            "session missing".to_string()
        } else if member_resume {
            session_id
                .map(short_session_summary)
                .unwrap_or_else(|| "identity matched".to_string())
        } else if previous.session_id.is_none() {
            "session unavailable".to_string()
        } else {
            "starting fresh".to_string()
        }
    } else {
        "no prior launch identity".to_string()
    };

    format!("{member_name}={decision} ({reason})")
}

fn short_session_summary(session_id: &str) -> String {
    let short = session_id.chars().take(8).collect::<String>();
    if session_id.chars().count() > 8 {
        format!("session {short}...")
    } else {
        format!("session {short}")
    }
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
    use std::path::Path;
    use std::process::Command;
    use std::sync::{Arc, Mutex};

    use crate::team::comms::Channel;
    use crate::team::config::{
        BoardConfig, ChannelConfig, RoleDef, StandupConfig, WorkflowMode, WorkflowPolicy,
    };
    use crate::team::events::{EventSink, read_events};
    use crate::team::test_support::{
        init_git_repo, setup_fake_claude, write_owned_task_file, write_owned_task_file_with_context,
    };
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

    fn write_open_task_file(project_root: &Path, id: u32, title: &str, status: &str) {
        let tasks_dir = project_root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join(format!("{id:03}-{title}.md")),
            format!(
                "---\nid: {id}\ntitle: {title}\nstatus: {status}\npriority: high\nclass: standard\n---\n\nTask description.\n"
            ),
        )
        .unwrap();
    }

    fn write_board_task_file(
        project_root: &Path,
        id: u32,
        title: &str,
        status: &str,
        claimed_by: Option<&str>,
        depends_on: &[u32],
        blocked_on: Option<&str>,
    ) {
        let tasks_dir = project_root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();

        let mut content =
            format!("---\nid: {id}\ntitle: {title}\nstatus: {status}\npriority: high\n");
        if let Some(claimed_by) = claimed_by {
            content.push_str(&format!("claimed_by: {claimed_by}\n"));
        }
        if !depends_on.is_empty() {
            content.push_str("depends_on:\n");
            for dependency in depends_on {
                content.push_str(&format!("  - {dependency}\n"));
            }
        }
        if let Some(blocked_on) = blocked_on {
            content.push_str(&format!("blocked_on: {blocked_on}\n"));
            content.push_str(&format!("blocked: {blocked_on}\n"));
        }
        content.push_str("class: standard\n---\n\nTask description.\n");

        std::fs::write(tasks_dir.join(format!("{id:03}-{title}.md")), content).unwrap();
    }

    fn starvation_test_daemon(tmp: &tempfile::TempDir, threshold: Option<usize>) -> TeamDaemon {
        let architect = MemberInstance {
            name: "architect".to_string(),
            role_name: "architect".to_string(),
            role_type: RoleType::Architect,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let eng_1 = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };
        let eng_2 = MemberInstance {
            name: "eng-2".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: Some("architect".to_string()),
            use_worktrees: false,
        };

        TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy {
                        pipeline_starvation_threshold: threshold,
                        ..WorkflowPolicy::default()
                    },
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
                members: vec![architect, eng_1, eng_2],
                pane_map: HashMap::new(),
            },
            watchers: HashMap::new(),
            states: HashMap::from([
                ("eng-1".to_string(), MemberState::Idle),
                ("eng-2".to_string(), MemberState::Idle),
            ]),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
            poll_interval: Duration::from_secs(5),
        }
    }

    fn failed_delivery_test_daemon(tmp: &tempfile::TempDir) -> TeamDaemon {
        let manager = MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
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
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
        };
        let architect = MemberInstance {
            name: "architect".to_string(),
            role_name: "architect".to_string(),
            role_type: RoleType::Architect,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };

        TeamDaemon {
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
                members: vec![architect, manager, engineer],
                pane_map: HashMap::from([("eng-1".to_string(), "%9999999".to_string())]),
            },
            watchers: HashMap::new(),
            states: HashMap::new(),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
            poll_interval: Duration::from_secs(5),
        }
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
        assert!(cmd.contains("batty-launch-project-arch-1.sh"));
        let script_path = std::env::temp_dir().join("batty-launch-project-arch-1.sh");
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
        assert!(cmd.contains("batty-launch-project-mgr-1.sh"));
        let script_path = std::env::temp_dir().join("batty-launch-project-mgr-1.sh");
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
        let project_slug = tmp.path().file_name().unwrap().to_string_lossy();
        let script_path =
            std::env::temp_dir().join(format!("batty-launch-{project_slug}-eng-1.sh"));
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
        let project_slug = tmp.path().file_name().unwrap().to_string_lossy();
        assert!(cmd.contains(&format!("batty-launch-{project_slug}-codex-active-test.sh")));
        let script_path =
            std::env::temp_dir().join(format!("batty-launch-{project_slug}-codex-active-test.sh"));
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
        let script_path = std::env::temp_dir().join("batty-launch-project-architect.sh");
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
            status::format_nudge_status(Some(&schedule)),
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
            status::format_nudge_status(Some(&schedule)),
            " #[fg=244]nudge paused#[default]"
        );
    }

    #[test]
    fn format_standup_status_marks_paused_while_member_is_working() {
        assert_eq!(
            status::format_standup_status(Some(Instant::now()), Duration::from_secs(600), true),
            " #[fg=244]standup paused#[default]"
        );
    }

    #[test]
    fn compose_pane_status_label_shows_pending_inbox_count() {
        let label = status::compose_pane_status_label(
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
        let label =
            status::compose_pane_status_label(MemberState::Working, 0, 0, &[], &[], true, "", "");
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
            pipeline_starvation_fired: true,
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
    fn resume_reason_includes_session_info() {
        let previous = LaunchIdentity {
            agent: "claude-code".to_string(),
            prompt: "same prompt".to_string(),
            session_id: Some("e303fefd1234".to_string()),
        };

        let summary = format_resume_decision_summary(
            "architect",
            "claude-code",
            Some(&previous),
            true,
            "same prompt",
            true,
            false,
            true,
            Some("e303fefd1234"),
        );

        assert!(summary.contains("architect=yes"));
        assert!(summary.contains("session e303fefd"));
    }

    #[test]
    fn fresh_start_logged_differently() {
        let previous = LaunchIdentity {
            agent: "claude-code".to_string(),
            prompt: "old prompt".to_string(),
            session_id: Some("e303fefd1234".to_string()),
        };

        let summary = format_resume_decision_summary(
            "architect",
            "claude-code",
            Some(&previous),
            true,
            "new prompt",
            true,
            false,
            false,
            Some("new-session"),
        );

        assert!(summary.contains("architect=no"));
        assert!(summary.contains("prompt changed"));
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
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
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
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
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
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
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
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
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
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
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
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
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
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
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
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
            poll_interval: Duration::from_secs(5),
        };

        assert_eq!(daemon.active_task_id("eng-1"), None);
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
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
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
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
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
    fn delivery_confirm_marker_detection_matches_captured_text() {
        let marker = message_delivery_marker("manager");
        let capture = format!("prompt\n{marker}\nbody\n");
        assert!(capture_contains_message_marker(&capture, &marker));
        assert!(!capture_contains_message_marker("prompt only", &marker));
    }

    #[test]
    fn delivery_confirm_marker_generation_uses_sender_header() {
        assert_eq!(
            message_delivery_marker("eng-1-4"),
            "--- Message from eng-1-4 ---"
        );
    }

    #[test]
    fn failed_delivery_new_sets_expected_fields() {
        let delivery = FailedDelivery::new("eng-1", "manager", "Please retry this.");
        assert_eq!(delivery.recipient, "eng-1");
        assert_eq!(delivery.from, "manager");
        assert_eq!(delivery.body, "Please retry this.");
        assert_eq!(delivery.attempts, 1);
        assert_eq!(delivery.message_marker(), "--- Message from manager ---");
        assert!(delivery.has_attempts_remaining());
    }

    #[test]
    fn failed_delivery_retry_requeues_before_attempt_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        let mut delivery = FailedDelivery::new("eng-1", "manager", "Please retry this.");
        delivery.attempts = 1;
        delivery.last_attempt = Instant::now() - FAILED_DELIVERY_RETRY_DELAY;
        daemon.failed_deliveries.push(delivery);

        daemon.retry_failed_deliveries().unwrap();

        assert_eq!(daemon.failed_deliveries.len(), 1);
        assert_eq!(daemon.failed_deliveries[0].attempts, 2);
        let messages =
            inbox::pending_messages(&inbox::inboxes_root(tmp.path()), "manager").unwrap();
        assert!(messages.is_empty());
    }

    #[test]
    fn failed_delivery_retry_respects_attempt_cap_and_escalates() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        let mut delivery = FailedDelivery::new("eng-1", "manager", "Please retry this.");
        delivery.attempts = FAILED_DELIVERY_MAX_ATTEMPTS - 1;
        delivery.last_attempt = Instant::now() - FAILED_DELIVERY_RETRY_DELAY;
        daemon.failed_deliveries.push(delivery);

        daemon.retry_failed_deliveries().unwrap();

        assert!(daemon.failed_deliveries.is_empty());
        let messages =
            inbox::pending_messages(&inbox::inboxes_root(tmp.path()), "manager").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].from, "daemon");
        assert!(
            messages[0]
                .body
                .contains("Live message delivery failed after 3 attempts.")
        );
        assert!(messages[0].body.contains("Recipient: eng-1"));
    }

    #[test]
    #[serial]
    fn poll_watchers_emits_context_exhausted_event() {
        let session = format!("batty-test-context-exhausted-{}", std::process::id());
        let _ = crate::tmux::kill_session(&session);

        crate::tmux::create_session(&session, "cat", &[], "/tmp").unwrap();
        let pane_id = crate::tmux::pane_id(&session).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        tmux::send_keys(&pane_id, "Conversation is too long to continue.", true).unwrap();
        tmux::send_keys(&pane_id, "prompt is too long", true).unwrap();
        std::thread::sleep(Duration::from_millis(150));

        let tmp = tempfile::tempdir().unwrap();
        let events_path = tmp.path().join("events.jsonl");
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
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
                    orchestrator_pane: false,
                    orchestrator_position: OrchestratorPosition::Bottom,
                    layout: None,
                    roles: Vec::new(),
                },
                session: session.clone(),
                members: vec![engineer],
                pane_map: HashMap::from([("eng-1".to_string(), pane_id.clone())]),
            },
            watchers: HashMap::from([(
                "eng-1".to_string(),
                SessionWatcher::new(&pane_id, "eng-1", 300, None),
            )]),
            states: HashMap::from([("eng-1".to_string(), MemberState::Idle)]),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::from([("eng-1".to_string(), 42)]),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            intervention_cooldowns: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&events_path).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
            poll_interval: Duration::from_secs(5),
        };

        daemon.poll_watchers().unwrap();

        assert_eq!(daemon.states.get("eng-1"), Some(&MemberState::Working));
        let events = read_events(&events_path).unwrap();
        let event = events
            .iter()
            .find(|event| event.event == "context_exhausted")
            .unwrap();
        assert_eq!(event.role.as_deref(), Some("eng-1"));
        assert_eq!(event.task.as_deref(), Some("42"));

        let _ = crate::tmux::kill_session(&session);
    }

    fn make_test_daemon(project_root: &Path, members: Vec<MemberInstance>) -> TeamDaemon {
        TeamDaemon::new(DaemonConfig {
            project_root: project_root.to_path_buf(),
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
        })
        .unwrap()
    }

    #[test]
    #[serial]
    fn restart_dead_members_respawns_member_and_records_event() {
        let session = "batty-test-restart-dead-member";
        let _ = crate::tmux::kill_session(session);

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
        for _ in 0..5 {
            if crate::tmux::pane_dead(&pane_id).unwrap_or(false) {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
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
    #[serial]
    fn agent_restart_relaunches_context_exhausted_member_with_task_context() {
        let session = "batty-test-agent-restart-context";
        let _ = crate::tmux::kill_session(session);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member_name = "eng-restart";
        let lead_name = "lead";
        let (fake_bin, fake_log) = setup_fake_claude(&tmp, member_name);
        let worktree_path = tmp.path().join("worktree");
        std::fs::create_dir_all(&worktree_path).unwrap();

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
            "eng-1-2/task-191",
            &worktree_path.display().to_string(),
        );

        daemon.handle_context_exhaustion(member_name).unwrap();
        std::thread::sleep(Duration::from_millis(700));

        let log = (0..20)
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
        assert!(log.contains("Branch: eng-1-2/task-191"));
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
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
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
            session: session.clone(),
            members: vec![engineer],
            pane_map: HashMap::from([("eng-1".to_string(), pane_id.clone())]),
        })
        .unwrap();

        daemon
            .ensure_assignment_pane_cwd("eng-1", &pane_id, &expected_dir)
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
            session: session.clone(),
            members: vec![engineer],
            pane_map: HashMap::from([("eng-1".to_string(), pane_id.clone())]),
        })
        .unwrap();

        daemon
            .ensure_assignment_pane_cwd("eng-1", &pane_id, &expected_dir)
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
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
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
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
            poll_interval: Duration::from_secs(5),
        };

        daemon.maybe_generate_standup().unwrap();

        assert!(daemon.last_standup.is_empty());
    }

    #[test]
    #[serial]
    fn maybe_fire_nudges_marks_member_working_after_live_delivery() {
        let session = "batty-test-nudge-live-delivery";
        let mut delivered_live = false;

        // A freshly created tmux pane can occasionally reject the first live
        // injection under heavy suite load. Retry the full setup a few times so
        // this test only fails on a real regression in the live-delivery path.
        for _attempt in 0..3 {
            let _ = crate::tmux::kill_session(session);

            crate::tmux::create_session(session, "cat", &[], "/tmp").unwrap();
            let pane_id = crate::tmux::pane_id(session).unwrap();
            std::thread::sleep(Duration::from_millis(150));

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
                intervention_cooldowns: HashMap::new(),
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
                pipeline_starvation_fired: false,
                retro_generated: false,
                failed_deliveries: Vec::new(),
                poll_interval: Duration::from_secs(5),
            };

            backdate_idle_grace(&mut daemon, "scientist");
            daemon.maybe_fire_nudges().unwrap();

            if daemon.states.get("scientist") == Some(&MemberState::Working) {
                let schedule = daemon.nudges.get("scientist").unwrap();
                assert!(schedule.paused);
                assert!(schedule.idle_since.is_none());
                assert!(!schedule.fired_this_idle);
                delivered_live = true;
                crate::tmux::kill_session(session).unwrap();
                break;
            }

            crate::tmux::kill_session(session).unwrap();
            std::thread::sleep(Duration::from_millis(100));
        }

        assert!(
            delivered_live,
            "expected at least one successful live nudge delivery"
        );
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
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
            poll_interval: Duration::from_secs(5),
        };

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        inbox::init_inbox(&root, "eng-1").unwrap();
        let mut result = inbox::InboxMessage::new_send("eng-1", "lead", "Task complete.");
        result.timestamp = super::now_unix();
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
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
            poll_interval: Duration::from_secs(5),
        };

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        inbox::init_inbox(&root, "eng-1").unwrap();
        let mut result = inbox::InboxMessage::new_send("eng-1", "lead", "Task complete.");
        result.timestamp = super::now_unix();
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
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
            poll_interval: Duration::from_secs(5),
        };

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        inbox::init_inbox(&root, "eng-1").unwrap();
        let mut result = inbox::InboxMessage::new_send("eng-1", "lead", "Task complete.");
        result.timestamp = super::now_unix();
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
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
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
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
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
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
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
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
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
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
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
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
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

    fn backdate_intervention_cooldown(daemon: &mut TeamDaemon, key: &str) {
        let cooldown = Duration::from_secs(
            daemon
                .config
                .team_config
                .automation
                .intervention_cooldown_secs,
        ) + Duration::from_secs(1);
        daemon
            .intervention_cooldowns
            .insert(key.to_string(), Instant::now() - cooldown);
    }

    #[test]
    fn owned_task_intervention_respects_cooldown() {
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
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
            poll_interval: Duration::from_secs(5),
        };

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        write_owned_task_file(tmp.path(), 191, "first-task", "in-progress", "lead");

        // First fire: should deliver intervention.
        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_owned_tasks().unwrap();
        let pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(pending.len(), 1, "first intervention should fire");

        // Acknowledge the message so inbox is clear for next check.
        for msg in pending {
            inbox::mark_delivered(&root, "lead", &msg.id).unwrap();
        }

        // Change signature (add another task) — should still be blocked by cooldown.
        write_owned_task_file(tmp.path(), 192, "second-task", "in-progress", "lead");
        daemon.maybe_intervene_owned_tasks().unwrap();
        let pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(pending.len(), 0, "cooldown should prevent refire");

        // Expire the cooldown — should fire again.
        backdate_intervention_cooldown(&mut daemon, "lead");
        daemon.maybe_intervene_owned_tasks().unwrap();
        let pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(pending.len(), 1, "should fire after cooldown expires");
    }

    #[test]
    fn triage_intervention_respects_cooldown() {
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
        let eng = MemberInstance {
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
                members: vec![lead, eng],
                pane_map: HashMap::from([
                    ("lead".to_string(), "%999".to_string()),
                    ("eng-1".to_string(), "%998".to_string()),
                ]),
            },
            watchers: HashMap::new(),
            states: HashMap::from([("lead".to_string(), MemberState::Idle)]),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::from([("lead".to_string(), 1)]),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
            poll_interval: Duration::from_secs(5),
        };

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        inbox::init_inbox(&root, "eng-1").unwrap();

        // Deliver a message from eng-1 to lead's inbox so triage finds something.
        let msg = inbox::InboxMessage::new_send("eng-1", "lead", "done with task 42");
        let msg_id = inbox::deliver_to_inbox(&root, &msg).unwrap();
        inbox::mark_delivered(&root, "lead", &msg_id).unwrap();

        // First fire: should work.
        backdate_idle_grace(&mut daemon, "lead");
        daemon.maybe_intervene_triage_backlog().unwrap();
        let pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(pending.len(), 1, "first triage intervention should fire");

        // Acknowledge so inbox is clear.
        for p in pending {
            inbox::mark_delivered(&root, "lead", &p.id).unwrap();
        }

        // Advance epoch (Working → Idle transition).
        daemon.update_automation_timers_for_state("lead", MemberState::Working);
        daemon.update_automation_timers_for_state("lead", MemberState::Idle);
        backdate_idle_grace(&mut daemon, "lead");

        // New epoch should normally allow refire, but cooldown blocks it.
        daemon.maybe_intervene_triage_backlog().unwrap();
        let pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(pending.len(), 0, "cooldown should prevent triage refire");

        // Expire cooldown — should fire.
        backdate_intervention_cooldown(&mut daemon, "triage::lead");
        daemon.maybe_intervene_triage_backlog().unwrap();
        let pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(
            pending.len(),
            1,
            "triage should fire after cooldown expires"
        );
    }

    #[test]
    fn maybe_intervene_owned_tasks_escalates_stuck_signature_to_parent() {
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
        let events_path = tmp.path().join("events.jsonl");
        let mut daemon = TeamDaemon {
            config: DaemonConfig {
                project_root: tmp.path().to_path_buf(),
                team_config: TeamConfig {
                    name: "test".to_string(),
                    workflow_mode: WorkflowMode::Legacy,
                    workflow_policy: WorkflowPolicy {
                        escalation_threshold_secs: 120,
                        ..WorkflowPolicy::default()
                    },
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
                pane_map: HashMap::from([("eng-1".to_string(), "%999".to_string())]),
            },
            watchers: HashMap::new(),
            states: HashMap::from([("eng-1".to_string(), MemberState::Idle)]),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            intervention_cooldowns: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            failure_window: FailureWindow::new(20),
            last_pattern_notifications: HashMap::new(),
            event_sink: EventSink::new(&events_path).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
            poll_interval: Duration::from_secs(5),
        };

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "eng-1").unwrap();
        inbox::init_inbox(&root, "lead").unwrap();
        write_owned_task_file(tmp.path(), 191, "owned-task", "in-progress", "eng-1");

        daemon.update_automation_timers_for_state("eng-1", MemberState::Working);
        daemon.update_automation_timers_for_state("eng-1", MemberState::Idle);
        backdate_idle_grace(&mut daemon, "eng-1");
        daemon.maybe_intervene_owned_tasks().unwrap();

        let state = daemon.owned_task_interventions.get_mut("eng-1").unwrap();
        state.detected_at = Instant::now() - Duration::from_secs(121);

        daemon.maybe_intervene_owned_tasks().unwrap();

        let engineer_pending = inbox::pending_messages(&root, "eng-1").unwrap();
        assert_eq!(engineer_pending.len(), 1);
        let lead_pending = inbox::pending_messages(&root, "lead").unwrap();
        assert_eq!(lead_pending.len(), 1);
        assert_eq!(lead_pending[0].from, "daemon");
        assert!(lead_pending[0].body.contains("Stuck task escalation"));
        assert!(lead_pending[0].body.contains("eng-1"));
        assert!(lead_pending[0].body.contains("Task #191"));
        assert!(lead_pending[0].body.contains("kanban-md edit --dir"));
        assert!(lead_pending[0].body.contains("batty assign eng-1"));
        assert!(
            daemon
                .owned_task_interventions
                .get("eng-1")
                .is_some_and(|state| state.escalation_sent)
        );

        let events = super::super::events::read_events(&events_path).unwrap();
        assert!(
            events.iter().any(|event| {
                event.event == "task_escalated"
                    && event.role.as_deref() == Some("eng-1")
                    && event.task.as_deref() == Some("191")
            }),
            "expected task_escalated event for stuck owned task"
        );
    }

    #[test]
    fn maybe_auto_unblock_moves_blocked_task_to_todo_and_notifies_owner() {
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
        let engineer = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![manager, engineer]);
        let board_tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        let events_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, "eng-1").unwrap();

        write_board_task_file(tmp.path(), 11, "dep-a", "done", None, &[], None);
        write_board_task_file(tmp.path(), 12, "dep-b", "done", None, &[], None);
        write_board_task_file(
            tmp.path(),
            13,
            "blocked-task",
            "blocked",
            Some("eng-1"),
            &[11, 12],
            Some("waiting on dependencies"),
        );

        daemon.maybe_auto_unblock_blocked_tasks().unwrap();

        let tasks = crate::task::load_tasks_from_dir(&board_tasks_dir).unwrap();
        let task = tasks.iter().find(|task| task.id == 13).unwrap();
        assert_eq!(task.status, "todo");
        assert!(task.blocked_on.is_none());
        assert!(task.blocked.is_none());

        let pending = inbox::pending_messages(&inbox_root, "eng-1").unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].body.contains("Task #13 (blocked-task)"));
        assert!(
            pending[0]
                .body
                .contains("automatically moved from `blocked` to `todo`")
        );
        assert!(pending[0].body.contains("[11, 12]"));

        let events = super::super::events::read_events(&events_path).unwrap();
        assert!(events.iter().any(|event| {
            event.event == "task_unblocked"
                && event.role.as_deref() == Some("eng-1")
                && event.task.as_deref() == Some("13")
        }));
    }

    #[test]
    fn maybe_auto_unblock_notifies_manager_when_task_is_unowned() {
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
        let mut daemon = make_test_daemon(tmp.path(), vec![manager]);
        let inbox_root = inbox::inboxes_root(tmp.path());
        let events_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        inbox::init_inbox(&inbox_root, "manager").unwrap();

        write_board_task_file(tmp.path(), 21, "dep-a", "done", None, &[], None);
        write_board_task_file(
            tmp.path(),
            22,
            "blocked-task",
            "blocked",
            None,
            &[21],
            Some("waiting on dependencies"),
        );

        daemon.maybe_auto_unblock_blocked_tasks().unwrap();

        let pending = inbox::pending_messages(&inbox_root, "manager").unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].body.contains("Task #22 (blocked-task)"));

        let events = super::super::events::read_events(&events_path).unwrap();
        assert!(events.iter().any(|event| {
            event.event == "task_unblocked"
                && event.role.as_deref() == Some("manager")
                && event.task.as_deref() == Some("22")
        }));
    }

    #[test]
    fn maybe_auto_unblock_leaves_unresolved_or_dependency_free_tasks_blocked() {
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
        let mut daemon = make_test_daemon(tmp.path(), vec![manager]);
        let board_tasks_dir = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        let events_path = tmp
            .path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl");
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, "manager").unwrap();

        write_board_task_file(tmp.path(), 31, "dep-a", "done", None, &[], None);
        write_board_task_file(tmp.path(), 32, "dep-b", "review", None, &[], None);
        write_board_task_file(
            tmp.path(),
            33,
            "blocked-partial",
            "blocked",
            None,
            &[31, 32],
            Some("waiting on dependencies"),
        );
        write_board_task_file(
            tmp.path(),
            34,
            "blocked-no-deps",
            "blocked",
            None,
            &[],
            Some("manual hold"),
        );

        daemon.maybe_auto_unblock_blocked_tasks().unwrap();

        let tasks = crate::task::load_tasks_from_dir(&board_tasks_dir).unwrap();
        let partial = tasks.iter().find(|task| task.id == 33).unwrap();
        assert_eq!(partial.status, "blocked");
        assert_eq!(
            partial.blocked_on.as_deref(),
            Some("waiting on dependencies")
        );

        let no_deps = tasks.iter().find(|task| task.id == 34).unwrap();
        assert_eq!(no_deps.status, "blocked");
        assert_eq!(no_deps.blocked_on.as_deref(), Some("manual hold"));

        let pending = inbox::pending_messages(&inbox_root, "manager").unwrap();
        assert!(pending.is_empty());

        let events = super::super::events::read_events(&events_path).unwrap();
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.task.as_deref(), Some("33" | "34")))
        );
    }

    #[test]
    fn maybe_intervene_owned_tasks_waits_for_escalation_threshold() {
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
                    workflow_policy: WorkflowPolicy {
                        escalation_threshold_secs: 120,
                        ..WorkflowPolicy::default()
                    },
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
                pane_map: HashMap::from([("eng-1".to_string(), "%999".to_string())]),
            },
            watchers: HashMap::new(),
            states: HashMap::from([("eng-1".to_string(), MemberState::Idle)]),
            idle_started_at: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            triage_idle_epochs: HashMap::new(),
            triage_interventions: HashMap::new(),
            owned_task_interventions: HashMap::new(),
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
            poll_interval: Duration::from_secs(5),
        };

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "eng-1").unwrap();
        inbox::init_inbox(&root, "lead").unwrap();
        write_owned_task_file(tmp.path(), 191, "owned-task", "in-progress", "eng-1");

        daemon.update_automation_timers_for_state("eng-1", MemberState::Working);
        daemon.update_automation_timers_for_state("eng-1", MemberState::Idle);
        backdate_idle_grace(&mut daemon, "eng-1");
        daemon.maybe_intervene_owned_tasks().unwrap();

        let state = daemon.owned_task_interventions.get_mut("eng-1").unwrap();
        state.detected_at = Instant::now() - Duration::from_secs(119);

        daemon.maybe_intervene_owned_tasks().unwrap();

        assert!(inbox::pending_messages(&root, "lead").unwrap().is_empty());
        assert!(
            daemon
                .owned_task_interventions
                .get("eng-1")
                .is_some_and(|state| !state.escalation_sent)
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
            intervention_cooldowns: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            event_sink: EventSink::new(&events_path).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
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
            intervention_cooldowns: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            event_sink: EventSink::new(&events_path).unwrap(),
            paused_standups: HashSet::new(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
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
        let repo = init_git_repo(&tmp, "batty-daemon-test");
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
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
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
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
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
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
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
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
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
    fn test_starvation_detected() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = starvation_test_daemon(&tmp, Some(1));
        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "architect").unwrap();
        write_open_task_file(tmp.path(), 101, "queued-task", "todo");

        daemon.maybe_detect_pipeline_starvation().unwrap();

        let pending = inbox::pending_messages(&root, "architect").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].from, "daemon");
        assert_eq!(
            pending[0].body,
            "Pipeline running dry: 2 idle engineers, 1 todo tasks."
        );
        assert!(daemon.pipeline_starvation_fired);
    }

    #[test]
    fn test_debounce() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = starvation_test_daemon(&tmp, Some(1));
        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "architect").unwrap();
        write_open_task_file(tmp.path(), 101, "queued-task", "todo");

        daemon.maybe_detect_pipeline_starvation().unwrap();
        daemon.maybe_detect_pipeline_starvation().unwrap();

        let pending = inbox::pending_messages(&root, "architect").unwrap();
        assert_eq!(pending.len(), 1);
        assert!(daemon.pipeline_starvation_fired);
    }

    #[test]
    fn test_threshold_config() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = starvation_test_daemon(&tmp, Some(2));
        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "architect").unwrap();
        write_open_task_file(tmp.path(), 101, "queued-task", "todo");

        daemon.maybe_detect_pipeline_starvation().unwrap();
        assert!(
            inbox::pending_messages(&root, "architect")
                .unwrap()
                .is_empty()
        );
        assert!(!daemon.pipeline_starvation_fired);

        let disabled_tmp = tempfile::tempdir().unwrap();
        let mut disabled_daemon = starvation_test_daemon(&disabled_tmp, None);
        let disabled_root = inbox::inboxes_root(disabled_tmp.path());
        inbox::init_inbox(&disabled_root, "architect").unwrap();
        write_open_task_file(disabled_tmp.path(), 101, "queued-task", "todo");

        disabled_daemon.maybe_detect_pipeline_starvation().unwrap();
        assert!(
            inbox::pending_messages(&disabled_root, "architect")
                .unwrap()
                .is_empty()
        );
        assert!(!disabled_daemon.pipeline_starvation_fired);
    }

    #[test]
    fn test_reset_when_work_added() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = starvation_test_daemon(&tmp, Some(1));
        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "architect").unwrap();
        write_open_task_file(tmp.path(), 101, "queued-task", "todo");

        daemon.maybe_detect_pipeline_starvation().unwrap();
        assert!(daemon.pipeline_starvation_fired);

        write_open_task_file(tmp.path(), 102, "queued-task-2", "backlog");
        daemon.maybe_detect_pipeline_starvation().unwrap();
        assert!(!daemon.pipeline_starvation_fired);

        std::fs::remove_file(
            tmp.path()
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks")
                .join("102-queued-task-2.md"),
        )
        .unwrap();
        daemon.maybe_detect_pipeline_starvation().unwrap();

        let pending = inbox::pending_messages(&root, "architect").unwrap();
        assert_eq!(pending.len(), 2);
        assert!(daemon.pipeline_starvation_fired);
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
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
            poll_interval: Duration::from_secs(5),
        };

        let root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&root, "lead").unwrap();
        inbox::init_inbox(&root, "eng-1").unwrap();
        let mut result = inbox::InboxMessage::new_send("eng-1", "lead", "Task complete.");
        result.timestamp = super::now_unix();
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
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
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
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
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
            intervention_cooldowns: HashMap::new(),
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
            pipeline_starvation_fired: false,
            retro_generated: false,
            failed_deliveries: Vec::new(),
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

    #[test]
    fn hot_reload_marker_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = hot_reload_marker_path(tmp.path());

        write_hot_reload_marker(tmp.path()).unwrap();
        assert!(marker.exists());
        assert!(consume_hot_reload_marker(tmp.path()));
        assert!(!marker.exists());
        assert!(!consume_hot_reload_marker(tmp.path()));
    }

    #[test]
    fn hot_reload_resume_args_include_resume_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let args = hot_reload_daemon_args(tmp.path());
        let canonical_root = tmp.path().canonicalize().unwrap();
        assert_eq!(
            args,
            vec![
                "-v".to_string(),
                "daemon".to_string(),
                "--project-root".to_string(),
                canonical_root.to_string_lossy().to_string(),
                "--resume".to_string(),
            ]
        );
    }

    #[test]
    fn hot_reload_fingerprint_detects_binary_change() {
        let tmp = tempfile::tempdir().unwrap();
        let binary = tmp.path().join("batty");
        fs::write(&binary, "old-binary").unwrap();
        let before = BinaryFingerprint::capture(&binary).unwrap();

        std::thread::sleep(Duration::from_millis(1100));
        fs::write(&binary, "new-binary-build").unwrap();
        let after = BinaryFingerprint::capture(&binary).unwrap();

        assert!(after.changed_from(&before));
    }

    #[test]
    fn hot_reload_acknowledgement_emits_event_and_log() {
        let tmp = tempfile::tempdir().unwrap();
        write_hot_reload_marker(tmp.path()).unwrap();

        let mut daemon = TeamDaemon::new(DaemonConfig {
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

        daemon.acknowledge_hot_reload_marker();

        let events = super::super::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap();
        assert!(events.iter().any(|event| event.event == "daemon_reloaded"));

        let content =
            fs::read_to_string(tmp.path().join(".batty").join("orchestrator.log")).unwrap();
        assert!(content.contains("daemon hot-reloaded"));
        assert!(!hot_reload_marker_path(tmp.path()).exists());
    }

    #[test]
    fn resume_decision_logged_to_orchestrator() {
        let tmp = tempfile::tempdir().unwrap();
        let member = MemberInstance {
            name: "architect".to_string(),
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
            members: vec![member],
            pane_map: HashMap::from([("architect".to_string(), "%999".to_string())]),
        })
        .unwrap();

        daemon.spawn_all_agents(false).unwrap();

        let content =
            fs::read_to_string(tmp.path().join(".batty").join("orchestrator.log")).unwrap();
        assert!(content.contains("resume: architect=no (resume disabled)"));
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
