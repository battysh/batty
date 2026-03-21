//! Core team daemon: poll loop, lifecycle coordination, and routing.
//!
//! `TeamDaemon` owns the long-running control loop for a Batty team session.
//! It starts and resumes member agents, polls tmux-backed watchers, routes
//! messages across panes, inboxes, and external channels, persists runtime
//! state, and runs periodic automation such as standups and board rotation.
//!
//! Focused subsystems that were extracted from this file stay close to the
//! daemon boundary:
//! - `merge` handles engineer completion, test gating, and merge/escalation
//!   flow once a task is reported done.
//! - `interventions` handles idle nudges and manager/architect intervention
//!   automation without changing the daemon's main control flow.
//!
//! This module remains the integration layer that sequences those subsystems
//! inside each poll iteration.

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
use super::delivery::FailedDelivery;
use super::events::EventSink;
#[cfg(test)]
use super::events::TeamEvent;
use super::failure_patterns::FailureTracker;
use super::hierarchy::MemberInstance;
use super::inbox;
use super::merge;
use super::standup::{self, MemberState};
use super::status;
use super::task_cmd;
#[cfg(test)]
use super::task_loop::next_unclaimed_task;
use super::task_loop::{engineer_base_branch_name, setup_engineer_worktree};
use super::watcher::{SessionTrackerConfig, SessionWatcher, WatcherState};
use super::{AssignmentDeliveryResult, AssignmentResultStatus, now_unix, store_assignment_result};
use crate::agent;
use crate::tmux;

#[path = "dispatch.rs"]
mod dispatch;
#[path = "daemon/interventions.rs"]
mod interventions;
#[path = "launcher.rs"]
mod launcher;
#[path = "daemon/telemetry.rs"]
mod telemetry;

#[cfg(test)]
use self::dispatch::normalized_assignment_dir;
pub(crate) use self::interventions::NudgeSchedule;
use self::interventions::OwnedTaskInterventionState;
use self::launcher::{
    duplicate_claude_session_ids, load_launch_state, member_session_tracker_config,
};
pub(super) use super::delivery::MessageDelivery;

/// Daemon configuration derived from TeamConfig.
pub struct DaemonConfig {
    pub project_root: PathBuf,
    pub team_config: TeamConfig,
    pub session: String,
    pub members: Vec<MemberInstance>,
    pub pane_map: HashMap<String, String>,
}

const CONTEXT_RESTART_COOLDOWN: Duration = Duration::from_secs(30);

const HOT_RELOAD_CHECK_INTERVAL: Duration = Duration::from_secs(30);
const HOT_RELOAD_MIN_INTERVAL: Duration = Duration::from_secs(30);

/// The running team daemon.
pub struct TeamDaemon {
    pub(super) config: DaemonConfig,
    pub(super) watchers: HashMap<String, SessionWatcher>,
    pub(super) states: HashMap<String, MemberState>,
    pub(super) idle_started_at: HashMap<String, Instant>,
    pub(super) active_tasks: HashMap<String, u32>,
    pub(super) retry_counts: HashMap<String, u32>,
    pub(super) triage_idle_epochs: HashMap<String, u64>,
    pub(super) triage_interventions: HashMap<String, u64>,
    pub(super) owned_task_interventions: HashMap<String, OwnedTaskInterventionState>,
    pub(super) intervention_cooldowns: HashMap<String, Instant>,
    pub(super) channels: HashMap<String, Box<dyn Channel>>,
    pub(super) nudges: HashMap<String, NudgeSchedule>,
    pub(super) telegram_bot: Option<super::telegram::TelegramBot>,
    pub(super) failure_tracker: FailureTracker,
    pub(super) event_sink: EventSink,
    pub(super) paused_standups: HashSet<String>,
    pub(super) last_standup: HashMap<String, Instant>,
    pub(super) last_board_rotation: Instant,
    pub(super) last_auto_dispatch: Instant,
    pub(super) pipeline_starvation_fired: bool,
    pub(super) retro_generated: bool,
    pub(super) failed_deliveries: Vec<FailedDelivery>,
    pub(super) poll_interval: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MemberWorktreeContext {
    path: PathBuf,
    branch: Option<String>,
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
            failure_tracker: FailureTracker::new(20),
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
        self.record_daemon_started();
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

            self.run_loop_step("poll_watchers", |daemon| daemon.poll_watchers());
            self.run_loop_step("restart_dead_members", |daemon| {
                daemon.restart_dead_members()
            });
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
                let generated = standup::maybe_generate_standup(
                    &daemon.config.project_root,
                    &daemon.config.team_config,
                    &daemon.config.members,
                    &daemon.watchers,
                    &daemon.states,
                    &daemon.config.pane_map,
                    daemon.telegram_bot.as_ref(),
                    &daemon.paused_standups,
                    &mut daemon.last_standup,
                )?;
                for recipient in generated {
                    daemon.record_standup_generated(&recipient);
                }
                Ok(())
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
            status::update_pane_status_labels(
                &self.config.project_root,
                &self.config.members,
                &self.config.pane_map,
                &self.states,
                &self.nudges,
                &self.last_standup,
                &self.paused_standups,
                |member_name| {
                    standup::standup_interval_for_member_name(
                        &self.config.team_config,
                        &self.config.members,
                        member_name,
                    )
                },
            );

            // Periodic heartbeat
            if last_heartbeat.elapsed() >= heartbeat_interval {
                let uptime = started_at.elapsed().as_secs();
                self.record_daemon_heartbeat(uptime);
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
        self.record_daemon_stopped(shutdown_reason, uptime);
        Ok(())
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
        self.record_daemon_reloading();
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
        self.record_orchestrator_action(format!(
            "restart: respawned pane and relaunched {} after pane death",
            member.name
        ));
        self.emit_event(TeamEvent::pane_respawned(&member.name));
        self.record_member_crashed(&member.name, true);
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
        self.record_task_escalated(&member.name, task.id.to_string());
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
                WatcherState::PaneDead => MemberState::Idle,
                WatcherState::ContextExhausted => MemberState::Working,
            };

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

    fn handle_pane_death(&mut self, member_name: &str) -> Result<()> {
        self.restart_member(member_name)
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

    pub(super) fn increment_retry(&mut self, engineer: &str) -> u32 {
        let count = self.retry_counts.entry(engineer.to_string()).or_insert(0);
        *count += 1;
        *count
    }

    pub(super) fn clear_active_task(&mut self, engineer: &str) {
        self.active_tasks.remove(engineer);
        self.retry_counts.remove(engineer);
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
            self.record_task_unblocked(event_role, task_id.to_string());
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
        standup::update_timer_for_state(
            &self.config.team_config,
            &self.config.members,
            &mut self.paused_standups,
            &mut self.last_standup,
            member_name,
            new_state,
        );
        self.update_triage_intervention_for_state(member_name, new_state);
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
        self.last_standup = standup::restore_timer_state(state.last_standup_elapsed_secs);

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
            last_standup_elapsed_secs: standup::snapshot_timer_state(&self.last_standup),
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
        let Some(stats) = super::retrospective::should_generate_retro(
            &self.config.project_root,
            self.retro_generated,
        )?
        else {
            return Ok(());
        };

        let report_path =
            super::retrospective::generate_retrospective(&self.config.project_root, &stats)?;
        self.retro_generated = true;
        self.record_retro_generated();
        info!(path = %report_path.display(), "retrospective generated");
        Ok(())
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

fn daemon_state_path(project_root: &Path) -> PathBuf {
    super::daemon_state_path(project_root)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::config::AutomationConfig;
    use crate::team::config::{
        BoardConfig, ChannelConfig, RoleDef, StandupConfig, WorkflowMode, WorkflowPolicy,
    };
    use crate::team::events::EventSink;
    use crate::team::test_helpers::{daemon_config_with_roles, make_test_daemon, write_event_log};
    use crate::team::test_support::{
        init_git_repo, setup_fake_claude, write_owned_task_file, write_owned_task_file_with_context,
    };
    use crate::team::watcher::WatcherState;
    use serial_test::serial;
    use std::collections::HashMap;
    use std::path::Path;
    use std::process::Command;

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
            failure_tracker: FailureTracker::new(20),
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
            failure_tracker: FailureTracker::new(20),
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
            failure_tracker: FailureTracker::new(20),
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
            failure_tracker: FailureTracker::new(20),
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
            failure_tracker: FailureTracker::new(20),
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
            failure_tracker: FailureTracker::new(20),
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
            failure_tracker: FailureTracker::new(20),
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
            members: vec![member],
            pane_map: HashMap::from([(member_name.to_string(), pane_id.clone())]),
        })
        .unwrap();

        daemon.spawn_all_agents(false).unwrap();
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

        let current = crate::tmux::pane_current_path(&pane_id).unwrap();
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
            members: vec![member],
            pane_map: HashMap::from([(member_name.to_string(), pane_id.clone())]),
        })
        .unwrap();

        crate::tmux::send_keys(&pane_id, "exit", true).unwrap();
        for _ in 0..5 {
            if crate::tmux::pane_dead(&pane_id).unwrap_or(false) {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        assert!(crate::tmux::pane_dead(&pane_id).unwrap());

        daemon.restart_member(member_name).unwrap();
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

        let current = crate::tmux::pane_current_path(&pane_id).unwrap();
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
        write_owned_task_file(tmp.path(), 191, "active-task", "in-progress", member_name);

        daemon.handle_context_exhaustion(member_name).unwrap();
        std::thread::sleep(Duration::from_millis(700));

        let current = crate::tmux::pane_current_path(&pane_id).unwrap();
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
            failure_tracker: FailureTracker::new(20),
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
                failure_tracker: FailureTracker::new(20),
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
            failure_tracker: FailureTracker::new(20),
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
            failure_tracker: FailureTracker::new(20),
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
            failure_tracker: FailureTracker::new(20),
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
            failure_tracker: FailureTracker::new(20),
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
            failure_tracker: FailureTracker::new(20),
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
            failure_tracker: FailureTracker::new(20),
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
            failure_tracker: FailureTracker::new(20),
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
            failure_tracker: FailureTracker::new(20),
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
            failure_tracker: FailureTracker::new(20),
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
            failure_tracker: FailureTracker::new(20),
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
            failure_tracker: FailureTracker::new(20),
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
            failure_tracker: FailureTracker::new(20),
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
            failure_tracker: FailureTracker::new(20),
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
            failure_tracker: FailureTracker::new(20),
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
            failure_tracker: FailureTracker::new(20),
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
            failure_tracker: FailureTracker::new(20),
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
            failure_tracker: FailureTracker::new(20),
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
            failure_tracker: FailureTracker::new(20),
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
            failure_tracker: FailureTracker::new(20),
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
            failure_tracker: FailureTracker::new(20),
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
            failure_tracker: FailureTracker::new(20),
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
            failure_tracker: FailureTracker::new(20),
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
            failure_tracker: FailureTracker::new(20),
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

        let config = daemon_config_with_roles(tmp.path(), roles);
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

        let config = daemon_config_with_roles(tmp.path(), roles);
        let daemon = TeamDaemon::new(config).unwrap();
        assert!(
            daemon.telegram_bot.is_none(),
            "telegram_bot should be None when no bot_token configured"
        );
    }
}
