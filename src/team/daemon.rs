//! Core team daemon — polling loop, agent lifecycle, message routing.
//!
//! The daemon ties together all team subsystems: it spawns agents in tmux
//! panes, monitors their output via `SessionWatcher`, routes messages between
//! roles, generates periodic standups, and emits structured events.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
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

/// A scheduled nudge for a member: text to inject on a timer.
struct NudgeSchedule {
    /// The nudge text extracted from the `## Nudge` section of the prompt .md.
    text: String,
    /// How often to inject it.
    interval: Duration,
    /// When it was last injected.
    last_fired: Instant,
}

/// The running team daemon.
pub struct TeamDaemon {
    config: DaemonConfig,
    watchers: HashMap<String, SessionWatcher>,
    states: HashMap<String, MemberState>,
    channels: HashMap<String, Box<dyn Channel>>,
    nudges: HashMap<String, NudgeSchedule>,
    event_sink: EventSink,
    last_standup: HashMap<String, Instant>,
    last_board_rotation: Instant,
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
            watchers.insert(name.clone(), SessionWatcher::new(pane_id, name, stale_secs));
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
                                last_fired: Instant::now(),
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
            channels,
            nudges,
            event_sink,
            last_standup: HashMap::new(),
            last_board_rotation: Instant::now(),
            poll_interval: Duration::from_secs(5),
        })
    }

    /// Run the daemon loop. Blocks until the session is killed or an error occurs.
    pub fn run(&mut self) -> Result<()> {
        self.event_sink.emit(TeamEvent::daemon_started())?;
        info!(session = %self.config.session, "daemon started");

        // Spawn agents in all panes
        self.spawn_all_agents()?;

        // Main polling loop
        loop {
            if !tmux::session_exists(&self.config.session) {
                info!("tmux session gone, shutting down");
                break;
            }

            self.poll_watchers()?;
            self.drain_legacy_command_queue()?;
            self.deliver_inbox_messages()?;
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
    fn spawn_all_agents(&mut self) -> Result<()> {
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
            )?;

            debug!(member = %member.name, agent = agent_name, idle, "spawning agent");
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
        self.event_sink
            .emit(TeamEvent::task_completed(member_name))?;

        // Find member's manager and notify them
        let member = self.config.members.iter().find(|m| m.name == member_name);

        if let Some(member) = member {
            if let Some(mgr_name) = &member.reports_to {
                if let Some(mgr_pane) = self.config.pane_map.get(mgr_name) {
                    // Get the last output from the completed member
                    let output = self
                        .watchers
                        .get(member_name)
                        .map(|w| w.last_lines(10))
                        .unwrap_or_default();

                    let msg = format!("[{member_name}] completed task.\nLast output:\n{output}");
                    message::inject_message(mgr_pane, member_name, &msg)?;
                    self.event_sink
                        .emit(TeamEvent::message_routed(member_name, mgr_name))?;
                }
            }
        }

        // Mark as idle, deactivate watcher
        self.states
            .insert(member_name.to_string(), MemberState::Idle);
        if let Some(watcher) = self.watchers.get_mut(member_name) {
            watcher.deactivate();
        }

        Ok(())
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

        // Reset to idle — will be reassigned
        self.states
            .insert(member_name.to_string(), MemberState::Idle);
        if let Some(watcher) = self.watchers.get_mut(member_name) {
            watcher.deactivate();
        }

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
            if let Some(watcher) = self.watchers.get_mut(name) {
                watcher.activate();
            }
            self.states.insert(name.clone(), MemberState::Working);
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
        let role_context =
            member.map(|m| strip_nudge_section(&self.load_prompt(m, &team_config_dir)));

        // Wait for agent to reset, then launch with new task
        std::thread::sleep(Duration::from_secs(1));
        let short_cmd = write_launch_script(
            engineer,
            agent_name,
            task,
            role_context.as_deref(),
            &work_dir,
            &self.config.project_root,
            false,
        )?;
        tmux::send_keys(&pane_id, &short_cmd, true)?;

        // Update state
        self.states
            .insert(engineer.to_string(), MemberState::Working);
        if let Some(watcher) = self.watchers.get_mut(engineer) {
            watcher.activate();
        }

        self.event_sink
            .emit(TeamEvent::task_assigned(engineer, task))?;

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
                let elapsed = schedule.last_fired.elapsed();
                if elapsed < schedule.interval {
                    let remaining = schedule.interval - elapsed;
                    let mins = remaining.as_secs() / 60;
                    let secs = remaining.as_secs() % 60;
                    format!(" #[fg=magenta]nudge {mins}:{secs:02}#[default]")
                } else {
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

    /// Fire any nudges whose interval has elapsed.
    fn maybe_fire_nudges(&mut self) -> Result<()> {
        let member_names: Vec<String> = self.nudges.keys().cloned().collect();

        for name in member_names {
            let fire = {
                let schedule = &self.nudges[&name];
                schedule.last_fired.elapsed() >= schedule.interval
            };

            if fire {
                if let Some(pane_id) = self.config.pane_map.get(&name) {
                    let text = self.nudges[&name].text.clone();
                    info!(member = %name, "firing nudge");
                    message::inject_message(pane_id, "daemon", &text)?;
                    self.event_sink
                        .emit(TeamEvent::message_routed("daemon", &name))?;
                }
                self.nudges.get_mut(&name).unwrap().last_fired = Instant::now();
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
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
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
            let prefix = "exec codex --dangerously-bypass-approvals-and-sandbox";
            if idle {
                prefix.to_string()
            } else {
                format!("{prefix} '{escaped_prompt}'")
            }
        }
        _ => {
            if idle {
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
        assert!(content
            .contains("exec codex --dangerously-bypass-approvals-and-sandbox 'work the task'"));
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
}
