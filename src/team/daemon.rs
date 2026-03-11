//! Core team daemon — polling loop, agent lifecycle, message routing.
//!
//! The daemon ties together all team subsystems: it spawns agents in tmux
//! panes, monitors their output via `SessionWatcher`, routes messages between
//! roles, generates periodic standups, and emits structured events.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tracing::{debug, info, warn};

use super::board;
use super::comms::{self, Channel};
use super::config::{RoleType, TeamConfig};
use super::events::{EventSink, TeamEvent};
use super::hierarchy::MemberInstance;
use super::inbox;
use super::message;
use super::standup::{self, MemberState};
use super::watcher::{SessionWatcher, WatcherState};
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
    /// When the member last became idle (`None` if currently working).
    idle_since: Option<Instant>,
    /// Whether the nudge has already fired for the current idle period.
    fired_this_idle: bool,
}

/// The running team daemon.
pub struct TeamDaemon {
    config: DaemonConfig,
    watchers: HashMap<String, SessionWatcher>,
    states: HashMap<String, MemberState>,
    active_tasks: HashMap<String, u32>,
    retry_counts: HashMap<String, u32>,
    channels: HashMap<String, Box<dyn Channel>>,
    nudges: HashMap<String, NudgeSchedule>,
    telegram_bot: Option<super::telegram::TelegramBot>,
    event_sink: EventSink,
    last_standup: HashMap<String, Instant>,
    last_board_rotation: Instant,
    last_auto_dispatch: Instant,
    poll_interval: Duration,
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
            let codex_cwd = config
                .members
                .iter()
                .find(|member| member.name == *name)
                .and_then(|member| member_codex_cwd(&config.project_root, member));
            watchers.insert(
                name.clone(),
                SessionWatcher::new(pane_id, name, stale_secs, codex_cwd),
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
                let prompt_file = role
                    .prompt
                    .as_deref()
                    .unwrap_or_else(|| match role.role_type {
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
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            channels,
            nudges,
            telegram_bot,
            event_sink,
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            poll_interval: Duration::from_secs(5),
        })
    }

    /// Run the daemon loop. Blocks until the session is killed or an error occurs.
    ///
    /// If `resume` is true, agents are launched with session-resume flags
    /// (`claude --continue` / `codex resume --last`) instead of fresh starts.
    pub fn run(&mut self, resume: bool) -> Result<()> {
        self.event_sink.emit(TeamEvent::daemon_started())?;
        info!(session = %self.config.session, resume, "daemon started");

        // Spawn agents in all panes
        self.spawn_all_agents(resume)?;

        // Main polling loop
        loop {
            if !tmux::session_exists(&self.config.session) {
                info!("tmux session gone, shutting down");
                break;
            }

            self.poll_watchers()?;
            self.drain_legacy_command_queue()?;
            self.deliver_inbox_messages()?;
            self.maybe_auto_dispatch()?;
            self.poll_telegram()?;
            self.deliver_user_inbox()?;
            self.maybe_fire_nudges()?;
            self.maybe_generate_standup()?;
            self.maybe_rotate_board()?;
            self.update_pane_status_labels();

            std::thread::sleep(self.poll_interval);
        }

        self.event_sink.emit(TeamEvent::daemon_stopped())?;
        info!("daemon stopped");
        Ok(())
    }

    /// Spawn the correct agent in each member's pane.
    fn spawn_all_agents(&mut self, resume: bool) -> Result<()> {
        let team_config_dir = self.config.project_root.join(".batty").join("team_config");

        // Ensure inboxes exist for all members
        let inboxes = inbox::inboxes_root(&self.config.project_root);
        for member in &self.config.members {
            if let Err(e) = inbox::init_inbox(&inboxes, &member.name) {
                warn!(member = %member.name, error = %e, "failed to init inbox");
            }
        }

        for member in &self.config.members {
            if member.role_type == RoleType::User {
                continue;
            }

            let Some(pane_id) = self.config.pane_map.get(&member.name) else {
                warn!(member = %member.name, "no pane found for member");
                continue;
            };

            // Set up worktree if needed
            let work_dir = if member.use_worktrees {
                let wt_dir = self
                    .config
                    .project_root
                    .join(".batty")
                    .join("worktrees")
                    .join(&member.name);
                match setup_engineer_worktree(
                    &self.config.project_root,
                    &wt_dir,
                    &member.name,
                    &team_config_dir,
                ) {
                    Ok(path) => path,
                    Err(e) => {
                        warn!(member = %member.name, error = %e, "worktree setup failed, using project root");
                        self.config.project_root.clone()
                    }
                }
            } else {
                self.config.project_root.clone()
            };

            // Build launch script (strip ## Nudge section — that's daemon-only).
            // All roles start idle so their role prompt is loaded as persistent
            // context instead of being interpreted as an immediate shell task.
            let agent_name = member.agent.as_deref().unwrap_or("claude");
            let prompt_text = strip_nudge_section(&self.load_prompt(member, &team_config_dir));
            let idle = role_starts_idle();

            let short_cmd = write_launch_script(
                &member.name,
                agent_name,
                &prompt_text,
                Some(&prompt_text),
                &work_dir,
                &self.config.project_root,
                idle,
                resume,
            )?;

            debug!(member = %member.name, agent = agent_name, idle, resume, "spawning agent");
            tmux::send_keys(pane_id, &short_cmd, true)?;

            let initial_state = if idle {
                MemberState::Idle
            } else {
                MemberState::Working
            };
            self.states.insert(member.name.clone(), initial_state);
            if !idle {
                if let Some(watcher) = self.watchers.get_mut(&member.name) {
                    watcher.activate();
                }
            }

            self.event_sink
                .emit(TeamEvent::agent_spawned(&member.name))?;
        }

        Ok(())
    }

    /// Load the prompt template for a member, substituting role-specific info.
    fn load_prompt(&self, member: &MemberInstance, config_dir: &Path) -> String {
        let prompt_file = member
            .prompt
            .as_deref()
            .unwrap_or_else(|| match member.role_type {
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
            let watcher = self.watchers.get_mut(name).unwrap();

            match watcher.poll() {
                Ok(new_state) => {
                    let member_state = match new_state {
                        WatcherState::Active => MemberState::Working,
                        WatcherState::Completed => MemberState::Completed,
                        WatcherState::Idle => MemberState::Idle,
                        WatcherState::Stale => MemberState::Working, // still working, just slow
                    };

                    if prev_state != Some(member_state) {
                        self.states.insert(name.clone(), member_state);

                        // Update nudge countdown on state transitions
                        self.update_nudge_for_state(name, member_state);

                        match member_state {
                            MemberState::Completed => {
                                info!(member = %name, "detected completion");
                                self.handle_completion(name)?;
                            }
                            MemberState::Crashed => {
                                warn!(member = %name, "detected crash");
                                self.handle_crash(name)?;
                            }
                            _ => {}
                        }
                    }
                }
                Err(e) => {
                    warn!(member = %name, error = %e, "watcher poll failed");
                }
            }
        }

        Ok(())
    }

    /// Handle a member completing their task.
    fn handle_completion(&mut self, member_name: &str) -> Result<()> {
        let is_engineer = self
            .config
            .members
            .iter()
            .any(|m| m.name == member_name && m.role_type == RoleType::Engineer);
        if is_engineer && self.active_task_id(member_name).is_some() {
            return self.handle_engineer_completion(member_name);
        }

        self.event_sink
            .emit(TeamEvent::task_completed(member_name))?;

        // Find member's manager and notify them
        let manager_target = self
            .config
            .members
            .iter()
            .find(|m| m.name == member_name)
            .and_then(|member| member.reports_to.clone())
            .and_then(|mgr_name| {
                self.config
                    .pane_map
                    .get(&mgr_name)
                    .cloned()
                    .map(|pane_id| (mgr_name, pane_id))
            });

        if let Some((mgr_name, mgr_pane)) = manager_target {
            // Get the last output from the completed member
            let output = self
                .watchers
                .get(member_name)
                .map(|w| w.last_lines(10))
                .unwrap_or_default();

            let msg = format!("[{member_name}] completed task.\nLast output:\n{output}");
            message::inject_message(&mgr_pane, member_name, &msg)?;
            self.mark_member_working(&mgr_name);
            self.event_sink
                .emit(TeamEvent::message_routed(member_name, &mgr_name))?;
        }

        // Mark as idle, deactivate watcher, start nudge countdown
        self.states
            .insert(member_name.to_string(), MemberState::Idle);
        if let Some(watcher) = self.watchers.get_mut(member_name) {
            watcher.deactivate();
        }
        self.update_nudge_for_state(member_name, MemberState::Idle);

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

        let (tests_passed, output_truncated) = run_tests_in_worktree(&worktree_dir)?;
        if tests_passed {
            let output = std::process::Command::new("kanban-md")
                .args([
                    "move",
                    &task_id.to_string(),
                    "done",
                    "--claim",
                    engineer,
                    "--dir",
                    &board_dir_str,
                ])
                .output()
                .context("failed to mark task done after passing tests")?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                bail!("kanban-md move failed: {stderr}");
            }

            if let Err(e) = merge_engineer_branch(&self.config.project_root, engineer) {
                warn!(engineer, error = %e, "engineer merge failed after passing tests");
            }

            self.clear_active_task(engineer);

            let manager_target = self
                .config
                .members
                .iter()
                .find(|m| m.name == engineer)
                .and_then(|member| member.reports_to.clone())
                .and_then(|mgr_name| {
                    self.config
                        .pane_map
                        .get(&mgr_name)
                        .cloned()
                        .map(|pane_id| (mgr_name, pane_id))
                });
            if let Some((mgr_name, mgr_pane)) = manager_target {
                let msg = format!("[{engineer}] task #{task_id} passed tests and merged.");
                message::inject_message(&mgr_pane, engineer, &msg)?;
                self.mark_member_working(&mgr_name);
                self.event_sink
                    .emit(TeamEvent::message_routed(engineer, &mgr_name))?;
            }

            self.event_sink.emit(TeamEvent::task_completed(engineer))?;
            self.states.insert(engineer.to_string(), MemberState::Idle);
            if let Some(watcher) = self.watchers.get_mut(engineer) {
                watcher.deactivate();
            }
            self.update_nudge_for_state(engineer, MemberState::Idle);
            return Ok(());
        }

        let attempt = self.increment_retry(engineer);
        if attempt <= 2 {
            let Some(pane_id) = self.config.pane_map.get(engineer).cloned() else {
                bail!("no pane found for engineer '{engineer}'");
            };
            let msg = format!(
                "Tests failed (attempt {attempt}/2). Fix the failures and try again:\n{output_truncated}"
            );
            message::inject_message(&pane_id, "batty", &msg)?;
            self.mark_member_working(engineer);
            info!(engineer, attempt, "test failure, sending back for retry");
            return Ok(());
        }

        let manager_target = self
            .config
            .members
            .iter()
            .find(|m| m.name == engineer)
            .and_then(|member| member.reports_to.clone())
            .and_then(|mgr_name| {
                self.config
                    .pane_map
                    .get(&mgr_name)
                    .cloned()
                    .map(|pane_id| (mgr_name, pane_id))
            });
        if let Some((mgr_name, mgr_pane)) = manager_target {
            let msg = format!(
                "[{engineer}] task #{task_id} failed tests after 2 retries. Escalating.\nLast output:\n{output_truncated}"
            );
            message::inject_message(&mgr_pane, engineer, &msg)?;
            self.mark_member_working(&mgr_name);
            self.event_sink
                .emit(TeamEvent::message_routed(engineer, &mgr_name))?;
        }

        let output = std::process::Command::new("kanban-md")
            .args([
                "edit",
                &task_id.to_string(),
                "--block",
                "tests failed after 2 retries",
                "--dir",
                &board_dir_str,
            ])
            .output()
            .context("failed to block task after max retries")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("kanban-md edit failed: {stderr}");
        }

        self.clear_active_task(engineer);
        self.states.insert(engineer.to_string(), MemberState::Idle);
        if let Some(watcher) = self.watchers.get_mut(engineer) {
            watcher.deactivate();
        }
        self.update_nudge_for_state(engineer, MemberState::Idle);
        info!(engineer, task_id, "escalated to manager after max retries");
        Ok(())
    }

    fn mark_member_working(&mut self, member_name: &str) {
        self.states
            .insert(member_name.to_string(), MemberState::Working);
        if let Some(watcher) = self.watchers.get_mut(member_name) {
            watcher.activate();
        }
        self.update_nudge_for_state(member_name, MemberState::Working);
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

    /// Handle a crashed member — restart if possible.
    fn handle_crash(&mut self, member_name: &str) -> Result<()> {
        let Some(pane_id) = self.config.pane_map.get(member_name).cloned() else {
            return Ok(());
        };

        // Check if pane is dead
        if tmux::pane_dead(&pane_id).unwrap_or(false) {
            info!(member = %member_name, "respawning dead pane");
            tmux::respawn_pane(&pane_id, "bash")?;
        }

        self.event_sink
            .emit(TeamEvent::member_crashed(member_name, true))?;

        // Reset to idle — will be reassigned, start nudge countdown
        self.states
            .insert(member_name.to_string(), MemberState::Idle);
        if let Some(watcher) = self.watchers.get_mut(member_name) {
            watcher.deactivate();
        }
        self.update_nudge_for_state(member_name, MemberState::Idle);

        Ok(())
    }

    /// Drain the legacy `commands.jsonl` queue into Maildir inboxes.
    ///
    /// This provides backward compatibility during migration. Commands written
    /// to the old queue file are converted to inbox messages and delivered.
    fn drain_legacy_command_queue(&mut self) -> Result<()> {
        let queue_path = message::command_queue_path(&self.config.project_root);
        let commands = message::drain_command_queue(&queue_path)?;
        if commands.is_empty() {
            return Ok(());
        }

        let root = inbox::inboxes_root(&self.config.project_root);
        for cmd in commands {
            match cmd {
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
                        .any(|r| r.name == to && r.role_type == RoleType::User);

                    if is_user {
                        if let Some(channel) = self.channels.get(&to) {
                            let formatted = format!("[From {from}]\n{msg}");
                            channel.send(&formatted)?;
                        }
                        self.event_sink
                            .emit(TeamEvent::message_routed(&from, &to))?;
                    } else {
                        let inbox_msg = inbox::InboxMessage::new_send(&from, &to, &msg);
                        inbox::deliver_to_inbox(&root, &inbox_msg)?;
                        debug!(from, to, "legacy command routed to inbox");
                    }
                }
                message::QueuedCommand::Assign {
                    from,
                    engineer,
                    task,
                } => {
                    let msg = inbox::InboxMessage::new_assign(&from, &engineer, &task);
                    inbox::deliver_to_inbox(&root, &msg)?;
                    debug!(engineer, "legacy assign routed to inbox");
                }
            }
        }

        Ok(())
    }

    /// Deliver inbox messages to agents that are at their prompt.
    ///
    /// For each member with a pane, check their `new/` inbox. If the agent's
    /// watcher state is Idle or Completed, inject the message via tmux and
    /// move it to `cur/`. If the agent is busy, messages stay in `new/` and
    /// survive daemon restarts.
    fn deliver_inbox_messages(&mut self) -> Result<()> {
        let root = inbox::inboxes_root(&self.config.project_root);

        let member_names: Vec<String> = self.config.pane_map.keys().cloned().collect();

        for name in &member_names {
            // Only deliver to idle/completed agents
            let is_ready = self
                .watchers
                .get(name)
                .map(|w| matches!(w.state, WatcherState::Idle | WatcherState::Completed))
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

                match msg.msg_type {
                    inbox::MessageType::Send => {
                        info!(from = %msg.from, to = %name, id = %msg.id, "delivering inbox message");
                        message::inject_message(&pane_id, &msg.from, &msg.body)?;
                    }
                    inbox::MessageType::Assign => {
                        info!(to = %name, id = %msg.id, "delivering inbox assignment");
                        self.assign_task(name, &msg.body)?;
                    }
                }

                // Mark as delivered (move new/ → cur/)
                if let Err(e) = inbox::mark_delivered(&root, name, &msg.id) {
                    warn!(member = %name, id = %msg.id, error = %e, "failed to mark delivered");
                }

                self.event_sink
                    .emit(TeamEvent::message_routed(&msg.from, name))?;

                // Small delay between multiple messages
                std::thread::sleep(Duration::from_secs(1));
            }

            // Re-activate watcher after delivering messages
            self.mark_member_working(name);
        }

        Ok(())
    }

    /// Assign a task to an engineer: reset context, inject new prompt.
    fn assign_task(&mut self, engineer: &str, task: &str) -> Result<()> {
        info!(engineer, task, "assigning task");

        let Some(pane_id) = self.config.pane_map.get(engineer).cloned() else {
            bail!("no pane found for engineer '{engineer}'");
        };

        // Find member to determine agent type
        let member = self.config.members.iter().find(|m| m.name == engineer);

        let agent_name = member.and_then(|m| m.agent.as_deref()).unwrap_or("claude");

        // Reset agent context
        let adapter = agent::adapter_from_name(agent_name);
        if let Some(adapter) = &adapter {
            for (keys, enter) in adapter.reset_context_keys() {
                tmux::send_keys(&pane_id, &keys, enter)?;
                std::thread::sleep(Duration::from_millis(500));
            }
        }

        // Determine work directory
        let use_worktrees = member.map(|m| m.use_worktrees).unwrap_or(false);
        let work_dir = if use_worktrees {
            self.config
                .project_root
                .join(".batty")
                .join("worktrees")
                .join(engineer)
        } else {
            self.config.project_root.clone()
        };

        let team_config_dir = self.config.project_root.join(".batty").join("team_config");
        if use_worktrees
            && let Err(e) = refresh_engineer_worktree(
                &self.config.project_root,
                &work_dir,
                engineer,
                &team_config_dir,
            )
        {
            warn!(
                engineer,
                error = %e,
                "worktree refresh failed, proceeding with existing"
            );
        }
        let role_context =
            member.map(|m| strip_nudge_section(&self.load_prompt(m, &team_config_dir)));

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
        )?;
        tmux::send_keys(&pane_id, &short_cmd, true)?;

        // Update state
        self.mark_member_working(engineer);

        self.event_sink
            .emit(TeamEvent::task_assigned(engineer, task))?;

        Ok(())
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

            let output = std::process::Command::new("kanban-md")
                .args([
                    "pick",
                    "--claim",
                    &engineer_name,
                    "--move",
                    "in-progress",
                    "--dir",
                    &board_dir_str,
                ])
                .output()
                .context("failed to run kanban-md pick for auto-dispatch")?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                bail!("kanban-md pick failed: {stderr}");
            }

            let assignment_message =
                format!("Task #{}: {}\n\n{}", task.id, task.title, task.description);
            self.assign_task(&engineer_name, &assignment_message)?;
            self.active_tasks.insert(engineer_name.clone(), task.id);
            self.retry_counts.remove(&engineer_name);
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
        let global_interval = self.config.team_config.standup.interval_secs;

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

            let state_str = match state {
                MemberState::Idle => "#[fg=yellow]idle#[default]",
                MemberState::Working => "#[fg=cyan]working#[default]",
                MemberState::Completed => "#[fg=green]done#[default]",
                MemberState::Crashed => "#[fg=red,bold]CRASHED#[default]",
            };

            let nudge_str = if let Some(schedule) = self.nudges.get(&member.name) {
                if schedule.fired_this_idle {
                    // Already fired this idle period — no countdown
                    String::new()
                } else if let Some(idle_since) = schedule.idle_since {
                    let elapsed = idle_since.elapsed();
                    if elapsed < schedule.interval {
                        let remaining = schedule.interval - elapsed;
                        let mins = remaining.as_secs() / 60;
                        let secs = remaining.as_secs() % 60;
                        format!(" #[fg=magenta]nudge {mins}:{secs:02}#[default]")
                    } else {
                        " #[fg=magenta]nudge now#[default]".to_string()
                    }
                } else {
                    // Working — no countdown shown
                    String::new()
                }
            } else {
                String::new()
            };

            // Standup timer for roles that receive standups
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
            let standup_interval_secs = role_def
                .and_then(|r| r.standup_interval_secs)
                .unwrap_or(global_interval);
            let standup_interval = Duration::from_secs(standup_interval_secs);

            let standup_str = if receives {
                if let Some(last) = self.last_standup.get(&member.name) {
                    let elapsed = last.elapsed();
                    if elapsed < standup_interval {
                        let remaining = standup_interval - elapsed;
                        let mins = remaining.as_secs() / 60;
                        let secs = remaining.as_secs() % 60;
                        format!(" #[fg=blue]standup {mins}:{secs:02}#[default]")
                    } else {
                        " #[fg=blue]standup now#[default]".to_string()
                    }
                } else {
                    String::new()
                }
            } else {
                String::new()
            };

            let label = format!("{state_str}{nudge_str}{standup_str}");

            let _ = std::process::Command::new("tmux")
                .args(["set-option", "-p", "-t", pane_id, "@batty_status", &label])
                .output();
        }
    }

    /// Update the nudge countdown when a member's state changes.
    ///
    /// - Transition to idle/completed: start the countdown.
    /// - Transition to working: clear the countdown (member is active).
    fn update_nudge_for_state(&mut self, member_name: &str, new_state: MemberState) {
        if let Some(schedule) = self.nudges.get_mut(member_name) {
            match new_state {
                MemberState::Idle | MemberState::Completed => {
                    if schedule.idle_since.is_none() {
                        schedule.idle_since = Some(Instant::now());
                        schedule.fired_this_idle = false;
                    }
                }
                MemberState::Working => {
                    schedule.idle_since = None;
                    schedule.fired_this_idle = false;
                }
                MemberState::Crashed => {
                    // Treat crash like idle — the agent is stuck
                    if schedule.idle_since.is_none() {
                        schedule.idle_since = Some(Instant::now());
                        schedule.fired_this_idle = false;
                    }
                }
            }
        }
    }

    /// Fire nudges for members that have been idle long enough.
    ///
    /// The nudge only fires once per idle period. The member must transition
    /// back to working and then to idle again before a new nudge can fire.
    fn maybe_fire_nudges(&mut self) -> Result<()> {
        let member_names: Vec<String> = self.nudges.keys().cloned().collect();

        for name in member_names {
            let fire = {
                let schedule = &self.nudges[&name];
                if schedule.fired_this_idle {
                    false
                } else if let Some(idle_since) = schedule.idle_since {
                    idle_since.elapsed() >= schedule.interval
                } else {
                    false // currently working — no nudge
                }
            };

            if fire {
                if let Some(pane_id) = self.config.pane_map.get(&name) {
                    let text = self.nudges[&name].text.clone();
                    info!(member = %name, "firing nudge (idle timeout)");
                    message::inject_message(pane_id, "daemon", &text)?;
                    self.event_sink
                        .emit(TeamEvent::message_routed("daemon", &name))?;
                }
                self.nudges.get_mut(&name).unwrap().fired_this_idle = true;
            }
        }

        Ok(())
    }

    /// Generate and inject standup for each recipient whose interval has elapsed.
    ///
    /// Each recipient gets a scoped standup showing only their direct reports.
    /// The interval is per-role: `standup_interval_secs` on the role definition
    /// takes precedence, falling back to the global `standup.interval_secs`.
    fn maybe_generate_standup(&mut self) -> Result<()> {
        let global_interval = self.config.team_config.standup.interval_secs;

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

            let report = standup::generate_standup_for(
                recipient,
                &self.config.members,
                &self.watchers,
                &self.states,
                self.config.team_config.standup.output_lines as usize,
            );

            if let Some(pane_id) = self.config.pane_map.get(&recipient.name) {
                if let Err(e) = standup::inject_standup(pane_id, &report) {
                    warn!(member = %recipient.name, error = %e, "failed to inject standup");
                } else {
                    self.event_sink
                        .emit(TeamEvent::standup_generated(&recipient.name))?;
                    any_generated = true;
                }
            }

            self.last_standup
                .insert(recipient.name.clone(), Instant::now());
        }

        if any_generated {
            info!("standups generated and injected");
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

            self.event_sink
                .emit(TeamEvent::message_routed("human", "telegram"))?;
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

            let channel = match self.channels.get(user_name) {
                Some(ch) => ch,
                None => {
                    debug!(user = %user_name, "no channel for user role");
                    continue;
                }
            };

            for msg in &messages {
                info!(from = %msg.from, to = %user_name, id = %msg.id, "delivering to user channel");

                let formatted = format!("--- Message from {} ---\n{}", msg.from, msg.body);
                if let Err(e) = channel.send(&formatted) {
                    warn!(to = %user_name, error = %e, "failed to send via channel");
                    // Don't mark as delivered on failure — retry next cycle
                    continue;
                }

                if let Err(e) = inbox::mark_delivered(&root, user_name, &msg.id) {
                    warn!(user = %user_name, id = %msg.id, error = %e, "failed to mark delivered");
                }

                self.event_sink
                    .emit(TeamEvent::message_routed(&msg.from, user_name))?;
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
fn write_launch_script(
    member_name: &str,
    agent_name: &str,
    prompt: &str,
    role_context: Option<&str>,
    work_dir: &Path,
    project_root: &Path,
    idle: bool,
    resume: bool,
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
                // Resume the most recent Claude session in this working directory
                "exec claude --dangerously-skip-permissions --continue".to_string()
            } else if idle {
                format!(
                    "exec claude --dangerously-skip-permissions --append-system-prompt '{escaped_prompt}'"
                )
            } else {
                format!("exec claude --dangerously-skip-permissions '{escaped_prompt}'")
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

    // batty wrapper: points to the exact binary that launched this daemon,
    // avoiding PATH issues where the installed binary may be blocked.
    let real_batty = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| resolve_binary("batty"));
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

fn member_codex_cwd(project_root: &Path, member: &MemberInstance) -> Option<PathBuf> {
    match member.agent.as_deref() {
        Some("codex") | Some("codex-cli") => {
            let work_dir = if member.use_worktrees {
                project_root
                    .join(".batty")
                    .join("worktrees")
                    .join(&member.name)
            } else {
                project_root.to_path_buf()
            };
            Some(
                work_dir
                    .join(".batty")
                    .join("codex-context")
                    .join(&member.name),
            )
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

fn priority_rank(p: &str) -> u32 {
    match p {
        "critical" => 0,
        "high" => 1,
        "medium" => 2,
        "low" => 3,
        _ => 4,
    }
}

fn next_unclaimed_task(board_dir: &Path) -> Result<Option<crate::task::Task>> {
    let tasks = crate::task::load_tasks_from_dir(&board_dir.join("tasks"))?;
    let task_status_by_id: HashMap<u32, String> = tasks
        .iter()
        .map(|task| (task.id, task.status.clone()))
        .collect();

    let mut available: Vec<crate::task::Task> = tasks
        .into_iter()
        .filter(|task| matches!(task.status.as_str(), "backlog" | "todo"))
        .filter(|task| task.claimed_by.is_none())
        .filter(|task| task.blocked.is_none())
        .filter(|task| {
            task.depends_on.iter().all(|dep_id| {
                task_status_by_id
                    .get(dep_id)
                    .is_none_or(|status| status == "done")
            })
        })
        .collect();

    available.sort_by_key(|task| (priority_rank(&task.priority), task.id));
    Ok(available.into_iter().next())
}

fn run_tests_in_worktree(worktree_dir: &Path) -> Result<(bool, String)> {
    let output = std::process::Command::new("cargo")
        .arg("test")
        .current_dir(worktree_dir)
        .output()
        .context("failed to run cargo test in worktree")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut combined = String::new();
    combined.push_str(&stdout);
    if !stdout.is_empty() && !stderr.is_empty() && !stdout.ends_with('\n') {
        combined.push('\n');
    }
    combined.push_str(&stderr);

    let lines: Vec<&str> = combined.lines().collect();
    let trimmed = if lines.len() > 50 {
        lines[lines.len() - 50..].join("\n")
    } else {
        combined
    };

    Ok((output.status.success(), trimmed))
}

/// Set up a git worktree for an engineer with symlinked shared config.
fn setup_engineer_worktree(
    project_root: &Path,
    worktree_dir: &Path,
    branch_name: &str,
    team_config_dir: &Path,
) -> Result<PathBuf> {
    // Create worktree directory
    if let Some(parent) = worktree_dir.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    // Create git worktree if it doesn't exist
    if !worktree_dir.exists() {
        let output = std::process::Command::new("git")
            .args([
                "worktree",
                "add",
                "-b",
                branch_name,
                &worktree_dir.to_string_lossy(),
                "HEAD",
            ])
            .current_dir(project_root)
            .output()
            .context("failed to create git worktree")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // If branch already exists, try without -b
            if stderr.contains("already exists") {
                let output = std::process::Command::new("git")
                    .args([
                        "worktree",
                        "add",
                        &worktree_dir.to_string_lossy(),
                        branch_name,
                    ])
                    .current_dir(project_root)
                    .output()
                    .context("failed to create git worktree")?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    bail!("git worktree add failed: {stderr}");
                }
            } else {
                bail!("git worktree add failed: {stderr}");
            }
        }

        info!(worktree = %worktree_dir.display(), branch = branch_name, "created engineer worktree");
    }

    // Symlink .batty/team_config into the worktree
    let wt_batty_dir = worktree_dir.join(".batty");
    std::fs::create_dir_all(&wt_batty_dir).ok();
    let wt_config_link = wt_batty_dir.join("team_config");

    if !wt_config_link.exists() {
        #[cfg(unix)]
        std::os::unix::fs::symlink(team_config_dir, &wt_config_link).with_context(|| {
            format!(
                "failed to symlink {} -> {}",
                wt_config_link.display(),
                team_config_dir.display()
            )
        })?;

        #[cfg(not(unix))]
        {
            warn!("symlinks not supported on this platform, copying config instead");
            // Fallback: copy the directory
            let _ = std::fs::create_dir_all(&wt_config_link);
        }

        debug!(
            link = %wt_config_link.display(),
            target = %team_config_dir.display(),
            "symlinked team config into worktree"
        );
    }

    Ok(worktree_dir.to_path_buf())
}

fn refresh_engineer_worktree(
    project_root: &Path,
    worktree_dir: &Path,
    branch_name: &str,
    team_config_dir: &Path,
) -> Result<()> {
    if !worktree_dir.exists() {
        return Ok(());
    }

    let status = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(worktree_dir)
        .output()
        .context("failed to inspect worktree status")?;
    if !status.status.success() {
        let stderr = String::from_utf8_lossy(&status.stderr);
        bail!("git status --porcelain failed: {stderr}");
    }

    let dirty = String::from_utf8_lossy(&status.stdout)
        .lines()
        .any(|line| !line.starts_with("?? .batty/"));
    if dirty {
        warn!(
            worktree = %worktree_dir.display(),
            branch = branch_name,
            "skipping worktree refresh because worktree is dirty"
        );
        return Ok(());
    }

    let up_to_date = std::process::Command::new("git")
        .args(["merge-base", "--is-ancestor", "main", branch_name])
        .current_dir(project_root)
        .output()
        .context("failed to compare worktree branch with main")?;
    if up_to_date.status.success() {
        return Ok(());
    }

    let rebase = std::process::Command::new("git")
        .args(["rebase", "main"])
        .current_dir(worktree_dir)
        .output()
        .context("failed to rebase engineer worktree")?;
    if rebase.status.success() {
        info!(
            worktree = %worktree_dir.display(),
            branch = branch_name,
            "refreshed engineer worktree"
        );
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&rebase.stderr).trim().to_string();
    let _ = std::process::Command::new("git")
        .args(["rebase", "--abort"])
        .current_dir(worktree_dir)
        .output();

    let remove = std::process::Command::new("git")
        .args([
            "worktree",
            "remove",
            "--force",
            &worktree_dir.to_string_lossy(),
        ])
        .current_dir(project_root)
        .output()
        .context("failed to remove conflicted worktree")?;
    if !remove.status.success() {
        let remove_stderr = String::from_utf8_lossy(&remove.stderr);
        bail!("git worktree remove --force failed after rebase error '{stderr}': {remove_stderr}");
    }

    let delete = std::process::Command::new("git")
        .args(["branch", "-D", branch_name])
        .current_dir(project_root)
        .output()
        .context("failed to delete conflicted worktree branch")?;
    if !delete.status.success() {
        let delete_stderr = String::from_utf8_lossy(&delete.stderr);
        bail!("git branch -D failed after rebase error '{stderr}': {delete_stderr}");
    }

    warn!(
        worktree = %worktree_dir.display(),
        branch = branch_name,
        rebase_error = %stderr,
        "recreating engineer worktree after rebase conflict"
    );
    setup_engineer_worktree(project_root, worktree_dir, branch_name, team_config_dir)?;
    Ok(())
}

/// Merge an engineer's worktree branch into main.
pub fn merge_engineer_branch(project_root: &Path, engineer_name: &str) -> Result<()> {
    let worktree_dir = project_root
        .join(".batty")
        .join("worktrees")
        .join(engineer_name);

    if !worktree_dir.exists() {
        bail!(
            "no worktree found for '{}' at {}",
            engineer_name,
            worktree_dir.display()
        );
    }

    // Get the branch name from the worktree
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(&worktree_dir)
        .output()
        .context("failed to get worktree branch")?;

    if !output.status.success() {
        bail!("failed to determine worktree branch");
    }

    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    info!(engineer = engineer_name, branch = %branch, "merging worktree branch");

    // Merge the branch into current branch (should be main)
    let output = std::process::Command::new("git")
        .args(["merge", &branch, "--no-edit"])
        .current_dir(project_root)
        .output()
        .context("git merge failed")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("merge failed: {stderr}");
    }

    println!("Merged branch '{branch}' from {engineer_name}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::process::{Command, Output};

    use crate::team::config::{BoardConfig, ChannelConfig, RoleDef, StandupConfig};
    use crate::team::events::EventSink;
    use crate::team::watcher::WatcherState;

    fn git(dir: &Path, args: &[&str]) -> Output {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap_or_else(|e| panic!("git {:?} failed to run: {e}", args))
    }

    fn git_ok(dir: &Path, args: &[&str]) {
        let output = git(dir, args);
        assert!(
            output.status.success(),
            "git {:?} failed: stdout={} stderr={}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_stdout(dir: &Path, args: &[&str]) -> String {
        let output = git(dir, args);
        assert!(
            output.status.success(),
            "git {:?} failed: stdout={} stderr={}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn init_git_repo(tmp: &tempfile::TempDir) -> PathBuf {
        let repo = tmp.path();
        git_ok(repo, &["init", "-b", "main"]);
        git_ok(repo, &["config", "user.email", "batty-test@example.com"]);
        git_ok(repo, &["config", "user.name", "Batty Test"]);
        std::fs::create_dir_all(repo.join(".batty").join("team_config")).unwrap();
        std::fs::write(repo.join("README.md"), "initial\n").unwrap();
        git_ok(repo, &["add", "README.md", ".batty/team_config"]);
        git_ok(repo, &["commit", "-m", "initial"]);
        repo.to_path_buf()
    }

    fn write_task_file(
        dir: &Path,
        id: u32,
        title: &str,
        status: &str,
        priority: &str,
        claimed_by: Option<&str>,
        depends_on: &[u32],
    ) {
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let mut content =
            format!("---\nid: {id}\ntitle: {title}\nstatus: {status}\npriority: {priority}\n");
        if let Some(cb) = claimed_by {
            content.push_str(&format!("claimed_by: {cb}\n"));
        }
        if !depends_on.is_empty() {
            content.push_str("depends_on:\n");
            for dep in depends_on {
                content.push_str(&format!("    - {dep}\n"));
            }
        }
        content.push_str("class: standard\n---\n\nTask description.\n");
        std::fs::write(tasks_dir.join(format!("{id:03}-{title}.md")), content).unwrap();
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
        )
        .unwrap();
        assert!(cmd.contains("batty-launch-arch-1.sh"));
        let script_path = std::env::temp_dir().join("batty-launch-arch-1.sh");
        let content = std::fs::read_to_string(&script_path).unwrap();
        assert!(content.contains("claude --dangerously-skip-permissions 'plan the project'"));
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
        )
        .unwrap();
        assert!(cmd.contains("batty-launch-mgr-1.sh"));
        let script_path = std::env::temp_dir().join("batty-launch-mgr-1.sh");
        let content = std::fs::read_to_string(&script_path).unwrap();
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
        )
        .unwrap();
        let script_path = std::env::temp_dir().join("batty-launch-eng-2.sh");
        let content = std::fs::read_to_string(&script_path).unwrap();
        assert!(content.contains("user'\\''s"));
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
    fn merge_rejects_missing_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let err = merge_engineer_branch(tmp.path(), "eng-1-1").unwrap_err();
        assert!(err.to_string().contains("no worktree found"));
    }

    #[test]
    fn test_refresh_worktree_rebases_behind_main() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        std::fs::write(repo.join("main.txt"), "new main content\n").unwrap();
        git_ok(&repo, &["add", "main.txt"]);
        git_ok(&repo, &["commit", "-m", "advance main"]);

        refresh_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        assert!(worktree_dir.join("main.txt").exists());
        assert_eq!(
            git_stdout(&repo, &["rev-parse", "main"]),
            git_stdout(&worktree_dir, &["rev-parse", "HEAD"])
        );
    }

    #[test]
    fn test_refresh_worktree_recreates_on_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-2");
        let team_config_dir = repo.join(".batty").join("team_config");

        std::fs::write(repo.join("file.txt"), "A\n").unwrap();
        git_ok(&repo, &["add", "file.txt"]);
        git_ok(&repo, &["commit", "-m", "add file"]);

        setup_engineer_worktree(&repo, &worktree_dir, "eng-2", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("file.txt"), "B\n").unwrap();
        git_ok(&worktree_dir, &["add", "file.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer change"]);

        std::fs::write(repo.join("file.txt"), "C\n").unwrap();
        git_ok(&repo, &["add", "file.txt"]);
        git_ok(&repo, &["commit", "-m", "main change"]);

        refresh_engineer_worktree(&repo, &worktree_dir, "eng-2", &team_config_dir).unwrap();

        assert!(worktree_dir.exists());
        assert_eq!(
            std::fs::read_to_string(worktree_dir.join("file.txt")).unwrap(),
            "C\n"
        );
        assert_eq!(
            git_stdout(&repo, &["rev-parse", "main"]),
            git_stdout(&worktree_dir, &["rev-parse", "HEAD"])
        );
    }

    #[test]
    fn test_refresh_worktree_skips_dirty() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-3");
        let team_config_dir = repo.join(".batty").join("team_config");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-3", &team_config_dir).unwrap();
        std::fs::write(worktree_dir.join("scratch.txt"), "uncommitted\n").unwrap();

        std::fs::write(repo.join("main.txt"), "new main content\n").unwrap();
        git_ok(&repo, &["add", "main.txt"]);
        git_ok(&repo, &["commit", "-m", "advance main"]);

        refresh_engineer_worktree(&repo, &worktree_dir, "eng-3", &team_config_dir).unwrap();

        assert!(!worktree_dir.join("main.txt").exists());
        assert_eq!(
            std::fs::read_to_string(worktree_dir.join("scratch.txt")).unwrap(),
            "uncommitted\n"
        );
    }

    #[test]
    fn test_refresh_worktree_noop_when_current() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp);
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-4");
        let team_config_dir = repo.join(".batty").join("team_config");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-4", &team_config_dir).unwrap();
        let before = git_stdout(&worktree_dir, &["rev-parse", "HEAD"]);

        refresh_engineer_worktree(&repo, &worktree_dir, "eng-4", &team_config_dir).unwrap();

        let after = git_stdout(&worktree_dir, &["rev-parse", "HEAD"]);
        assert_eq!(before, after);
        assert!(worktree_dir.exists());
    }

    #[test]
    fn test_next_unclaimed_task_picks_highest_priority() {
        let tmp = tempfile::tempdir().unwrap();
        write_task_file(tmp.path(), 1, "low-task", "todo", "low", None, &[]);
        write_task_file(tmp.path(), 2, "high-task", "todo", "high", None, &[]);
        write_task_file(
            tmp.path(),
            3,
            "critical-task",
            "todo",
            "critical",
            None,
            &[],
        );

        let task = next_unclaimed_task(tmp.path()).unwrap().unwrap();
        assert_eq!(task.id, 3);
        assert_eq!(task.title, "critical-task");
    }

    #[test]
    fn test_next_unclaimed_task_skips_claimed() {
        let tmp = tempfile::tempdir().unwrap();
        write_task_file(
            tmp.path(),
            1,
            "claimed-task",
            "todo",
            "critical",
            Some("eng-1-1"),
            &[],
        );
        write_task_file(tmp.path(), 2, "open-task", "todo", "low", None, &[]);

        let task = next_unclaimed_task(tmp.path()).unwrap().unwrap();
        assert_eq!(task.id, 2);
        assert_eq!(task.title, "open-task");
    }

    #[test]
    fn test_next_unclaimed_task_skips_blocked_dependency() {
        let tmp = tempfile::tempdir().unwrap();
        write_task_file(tmp.path(), 1, "first-task", "backlog", "medium", None, &[]);
        write_task_file(tmp.path(), 2, "second-task", "todo", "critical", None, &[1]);

        let task = next_unclaimed_task(tmp.path()).unwrap().unwrap();
        assert_eq!(task.id, 1);
        assert_eq!(task.title, "first-task");
    }

    #[test]
    fn test_next_unclaimed_task_returns_none_when_empty() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("tasks")).unwrap();

        let task = next_unclaimed_task(tmp.path()).unwrap();
        assert!(task.is_none());
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
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
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
        write_task_file(&board_dir, 1, "auto-task", "todo", "high", None, &[]);

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
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: Vec::new(),
                pane_map: HashMap::new(),
            },
            watchers: HashMap::new(),
            states: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
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
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: Vec::new(),
                pane_map: HashMap::new(),
            },
            watchers: HashMap::new(),
            states: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
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
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: Vec::new(),
                pane_map: HashMap::new(),
            },
            watchers: HashMap::new(),
            states: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
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
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: Vec::new(),
                pane_map: HashMap::new(),
            },
            watchers: HashMap::new(),
            states: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
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
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: vec![engineer],
                pane_map: HashMap::new(),
            },
            watchers: HashMap::new(),
            states: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
            poll_interval: Duration::from_secs(5),
        };

        daemon.active_tasks.insert("eng-1".into(), 42);
        daemon.handle_engineer_completion("eng-1").unwrap();
        assert_eq!(daemon.active_task_id("eng-1"), Some(42));
    }

    #[test]
    fn test_run_tests_in_worktree_returns_pass_fail() {
        let tmp = tempfile::tempdir().unwrap();
        let worktree = tmp.path();
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        std::fs::write(
            worktree.join("Cargo.toml"),
            "[package]\nname = \"batty-testcrate\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();

        std::fs::write(
            worktree.join("src").join("lib.rs"),
            "#[cfg(test)]\nmod tests {\n    #[test]\n    fn passes() {\n        assert_eq!(2 + 2, 4);\n    }\n}\n",
        )
        .unwrap();
        let (passed, output) = run_tests_in_worktree(worktree).unwrap();
        assert!(passed);
        assert!(output.contains("test result: ok"));

        std::fs::write(
            worktree.join("src").join("lib.rs"),
            "#[cfg(test)]\nmod tests {\n    #[test]\n    fn fails() {\n        assert_eq!(2 + 2, 5);\n    }\n}\n",
        )
        .unwrap();
        let (passed, output) = run_tests_in_worktree(worktree).unwrap();
        assert!(!passed);
        assert!(output.contains("FAILED"));
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
                    board: BoardConfig::default(),
                    standup: StandupConfig::default(),
                    layout: None,
                    roles: Vec::new(),
                },
                session: "test".to_string(),
                members: Vec::new(),
                pane_map: HashMap::new(),
            },
            watchers,
            states: HashMap::new(),
            active_tasks: HashMap::new(),
            retry_counts: HashMap::new(),
            channels: HashMap::new(),
            nudges: HashMap::new(),
            telegram_bot: None,
            event_sink: EventSink::new(&tmp.path().join("events.jsonl")).unwrap(),
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            last_auto_dispatch: Instant::now(),
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

    /// Helper: build a minimal DaemonConfig with the given roles.
    fn daemon_config_with_roles(tmp: &tempfile::TempDir, roles: Vec<RoleDef>) -> DaemonConfig {
        DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
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
