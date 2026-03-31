use super::*;
use crate::team::task_loop::setup_multi_repo_worktree;
use crate::team::watcher::{SessionTrackerConfig, discover_claude_session_file};
use crate::team::{layout, shim_events_log_path, shim_log_path, shim_logs_dir};
use tracing::debug;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct LaunchIdentity {
    pub(super) agent: String,
    pub(super) prompt: String,
    pub(super) session_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct MemberLaunchPlan {
    short_cmd: String,
    pub(super) work_dir: PathBuf,
    pub(super) identity: LaunchIdentity,
    initial_state: MemberState,
    activate_watcher: bool,
    pub(super) resume_summary: String,
    persist_session_id: bool,
}

impl MemberLaunchPlan {
    fn persisted_identity(&self) -> LaunchIdentity {
        let mut identity = self.identity.clone();
        if !self.persist_session_id {
            identity.session_id = None;
        }
        identity
    }
}

impl TeamDaemon {
    pub(super) fn member_work_dir(&self, member: &MemberInstance) -> PathBuf {
        if !member.use_worktrees {
            debug!(
                member = %member.name,
                "Skipping worktree setup for {}: use_worktrees=false",
                member.name
            );
            return self.config.project_root.clone();
        }

        if !self.is_git_repo && !self.is_multi_repo {
            debug!(
                member = %member.name,
                "Skipping worktree setup: not a git or multi-repo project"
            );
            return self.config.project_root.clone();
        }

        let team_config_dir = self.config.project_root.join(".batty").join("team_config");
        let wt_dir = self
            .config
            .project_root
            .join(".batty")
            .join("worktrees")
            .join(&member.name);
        let branch_name = engineer_base_branch_name(&member.name);

        let result = if self.is_multi_repo {
            setup_multi_repo_worktree(
                &self.config.project_root,
                &wt_dir,
                &branch_name,
                &team_config_dir,
                &self.sub_repo_names,
            )
        } else {
            setup_engineer_worktree(
                &self.config.project_root,
                &wt_dir,
                &branch_name,
                &team_config_dir,
            )
        };

        match result {
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
    }

    pub(super) fn validate_member_panes_on_startup(&mut self) {
        if self.config.team_config.use_shim {
            return;
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

            let expected_dir = self.member_work_dir(member);
            if let Err(error) = self.ensure_member_pane_cwd(&member.name, &pane_id, &expected_dir) {
                warn!(
                    member = %member.name,
                    pane = %pane_id,
                    error = %error,
                    "failed to validate pane cwd on daemon startup"
                );
            }
        }
    }

    pub(super) fn prepare_member_launch(
        &self,
        member: &MemberInstance,
        resume: bool,
        previous_launch_state: &HashMap<String, LaunchIdentity>,
        duplicate_claude_session_ids: &HashSet<&str>,
    ) -> Result<MemberLaunchPlan> {
        let team_config_dir = self.config.project_root.join(".batty").join("team_config");
        let work_dir = self.member_work_dir(member);

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
        let resolved_previous_session_id = previous_identity
            .and_then(|identity| identity.session_id.as_deref())
            .and_then(|session_id| resolve_session_id(&normalized_agent, session_id));
        let session_available = previous_identity
            .and_then(|identity| identity.session_id.as_deref())
            .is_none_or(|_| resolved_previous_session_id.is_some());
        let (member_resume, session_id) = resolve_member_launch_session(
            &normalized_agent,
            previous_identity,
            requested_resume,
            resolved_previous_session_id.clone(),
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
            session_available,
            previous_identity
                .and_then(|identity| identity.session_id.as_deref())
                .is_some_and(|existing| duplicate_claude_session_ids.contains(existing)),
            member_resume,
            session_id.as_deref(),
        );

        let sdk_mode =
            matches!(agent_name, "claude" | "claude-code") && self.config.team_config.use_sdk_mode;
        // SDK mode always uses a fresh session — Claude -p doesn't support --resume
        // and reusing a session_id causes "Session ID already in use" errors.
        let effective_session_id = if sdk_mode {
            Some(uuid::Uuid::new_v4().to_string())
        } else {
            session_id.clone()
        };
        let effective_resume = if sdk_mode { false } else { member_resume };
        let short_cmd = write_launch_script(
            &member.name,
            agent_name,
            &prompt_text,
            Some(&prompt_text),
            &work_dir,
            &self.config.project_root,
            idle,
            effective_resume,
            effective_session_id.as_deref(),
            sdk_mode,
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
            work_dir,
            identity: LaunchIdentity {
                agent: normalized_agent.clone(),
                prompt: prompt_text,
                session_id: effective_session_id,
            },
            initial_state: initial_member_state(idle, member_resume),
            activate_watcher: should_activate_watcher_on_spawn(idle, member_resume),
            resume_summary,
            persist_session_id: member_resume || normalized_agent != "claude-code",
        })
    }

    pub(super) fn apply_member_launch(
        &mut self,
        member: &MemberInstance,
        pane_id: &str,
        plan: &MemberLaunchPlan,
    ) -> Result<()> {
        if let Some(watcher) = self.watchers.get_mut(&member.name) {
            watcher.set_session_id(plan.identity.session_id.clone());
        }
        if !self.config.team_config.use_shim {
            self.ensure_member_pane_cwd(&member.name, pane_id, &plan.work_dir)?;
        }
        tmux::send_keys(pane_id, &plan.short_cmd, true)?;
        self.states.insert(member.name.clone(), plan.initial_state);
        self.update_automation_timers_for_state(&member.name, plan.initial_state);
        if plan.activate_watcher
            && let Some(watcher) = self.watchers.get_mut(&member.name)
        {
            watcher.activate();
        }
        self.record_agent_spawned(&member.name);
        Ok(())
    }

    #[allow(dead_code)]
    pub(super) fn persist_member_launch_identity(
        &self,
        member_name: &str,
        identity: LaunchIdentity,
    ) -> Result<()> {
        let mut launch_state = load_launch_state(&self.config.project_root);
        launch_state.insert(member_name.to_string(), identity);
        save_launch_state(&self.config.project_root, &launch_state)
    }

    pub(super) fn spawn_all_agents(&mut self, resume: bool) -> Result<()> {
        if self.config.team_config.use_shim {
            return self.spawn_all_shim_agents(resume);
        }

        let previous_launch_state = load_launch_state(&self.config.project_root);
        let duplicate_claude_session_ids = duplicate_claude_session_ids(&previous_launch_state);
        let mut next_launch_state = HashMap::new();
        let mut resume_summaries = Vec::new();

        let inboxes = inbox::inboxes_root(&self.config.project_root);
        for member in &self.config.members {
            if let Err(error) = inbox::init_inbox(&inboxes, &member.name) {
                warn!(member = %member.name, error = %error, "failed to init inbox");
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
                    next_launch_state.insert(member.name.clone(), plan.persisted_identity());
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

    fn spawn_all_shim_agents(&mut self, resume: bool) -> Result<()> {
        let previous_launch_state = load_launch_state(&self.config.project_root);
        let duplicate_claude_session_ids = duplicate_claude_session_ids(&previous_launch_state);
        let mut next_launch_state = HashMap::new();
        let mut resume_summaries = Vec::new();

        let inboxes = inbox::inboxes_root(&self.config.project_root);
        for member in &self.config.members {
            if let Err(error) = inbox::init_inbox(&inboxes, &member.name) {
                warn!(member = %member.name, error = %error, "failed to init inbox");
            }
        }

        let shim_logs_dir = shim_logs_dir(&self.config.project_root);
        std::fs::create_dir_all(&shim_logs_dir).with_context(|| {
            format!(
                "failed to create shim log directory {}",
                shim_logs_dir.display()
            )
        })?;

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

                    let log_path = shim_log_path(&self.config.project_root, &member.name);
                    let events_log_path =
                        shim_events_log_path(&self.config.project_root, &member.name);
                    if let Err(error) = layout::respawn_as_display_pane(
                        &pane_id,
                        &self.config.project_root,
                        &member.name,
                        &events_log_path,
                        &log_path,
                    ) {
                        warn!(
                            member = %member.name,
                            pane = %pane_id,
                            error = %error,
                            "failed to respawn pane as shim display"
                        );
                        continue;
                    }

                    let agent_type =
                        shim_agent_type_name(member.agent.as_deref().unwrap_or("claude"));
                    let sdk_mode = agent_type == "claude" && self.config.team_config.use_sdk_mode;
                    match shim_spawn::spawn_shim(
                        &member.name,
                        &agent_type,
                        &plan.short_cmd,
                        &plan.work_dir,
                        Some(&log_path),
                        sdk_mode,
                    ) {
                        Ok(handle) => {
                            if let Some(watcher) = self.watchers.get_mut(&member.name) {
                                watcher.set_session_id(plan.identity.session_id.clone());
                            }
                            self.shim_handles.insert(member.name.clone(), handle);
                            self.states.insert(member.name.clone(), plan.initial_state);
                            self.update_automation_timers_for_state(
                                &member.name,
                                plan.initial_state,
                            );
                            self.record_agent_spawned(&member.name);
                            next_launch_state
                                .insert(member.name.clone(), plan.persisted_identity());
                        }
                        Err(error) => {
                            warn!(
                                member = %member.name,
                                error = %error,
                                "failed to spawn shim for member"
                            );
                        }
                    }
                }
                Err(error) => {
                    warn!(
                        member = %member.name,
                        error = %error,
                        "failed to prepare shim launch"
                    );
                }
            }
        }

        if !resume_summaries.is_empty() {
            self.record_orchestrator_action(format!("resume: {}", resume_summaries.join(", ")));
        }

        if let Err(error) = save_launch_state(&self.config.project_root, &next_launch_state) {
            warn!(
                error = %error,
                "failed to persist launch state after spawning shims"
            );
        }

        Ok(())
    }

    pub(super) fn sync_launch_state_session_ids(&mut self) -> Result<()> {
        let mut launch_state = load_launch_state(&self.config.project_root);
        let mut changed = false;

        for (member_name, watcher) in &mut self.watchers {
            watcher.refresh_session_tracking()?;
            let preferred_session_id = watcher.configured_session_id();
            let session_id = watcher.current_session_id().or_else(|| {
                let member = self
                    .config
                    .members
                    .iter()
                    .find(|member| member.name == *member_name)?;
                let tracker = member_session_tracker_config(&self.config.project_root, member)?;
                match tracker {
                    SessionTrackerConfig::Claude { cwd } => {
                        let projects_root = std::env::var_os("HOME")
                            .map(PathBuf::from)
                            .unwrap_or_else(|| PathBuf::from("/"))
                            .join(".claude")
                            .join("projects");
                        let session_file = discover_claude_session_file(
                            &projects_root,
                            &cwd,
                            preferred_session_id.as_deref(),
                        )
                        .ok()??;
                        let stem = session_file.file_stem()?.to_str()?;
                        Some(stem.to_string())
                    }
                    _ => None,
                }
            });
            let Some(session_id) = session_id else {
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

    pub(super) fn persist_member_session_id(
        &self,
        member_name: &str,
        session_id: &str,
    ) -> Result<()> {
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

pub(super) fn strip_nudge_section(prompt: &str) -> String {
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

#[allow(clippy::too_many_arguments)]
pub(super) fn write_launch_script(
    member_name: &str,
    agent_name: &str,
    prompt: &str,
    role_context: Option<&str>,
    work_dir: &Path,
    project_root: &Path,
    idle: bool,
    resume: bool,
    session_id: Option<&str>,
    sdk_mode: bool,
) -> Result<String> {
    let project_slug = project_root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "default".to_string());
    let script_path =
        std::env::temp_dir().join(format!("batty-launch-{project_slug}-{member_name}.sh"));
    let launch_dir = match agent_name {
        "codex" | "codex-cli" => prepare_codex_context(member_name, role_context, work_dir)?,
        _ => work_dir.to_path_buf(),
    };
    let launch_dir_str = launch_dir.to_string_lossy();

    let adapter = agent::adapter_from_name(agent_name)
        .unwrap_or_else(|| agent::adapter_from_name("claude").unwrap());

    // For kiro, write a per-member agent config so the prompt is loaded as a
    // system prompt via --agent rather than passed as user input.
    let effective_prompt = if matches!(agent_name, "kiro" | "kiro-cli") {
        agent::kiro::write_kiro_agent_config(member_name, prompt, &launch_dir)?
    } else {
        prompt.to_string()
    };

    let agent_cmd = if sdk_mode {
        // In SDK mode, Claude Code uses stream-json protocol.
        // Role prompt goes via --append-system-prompt; tasks arrive via stdin NDJSON.
        use crate::agent::claude::ClaudeCodeAdapter;
        let claude_adapter = ClaudeCodeAdapter::new(None);
        claude_adapter.sdk_launch_command(session_id, Some(&effective_prompt))
    } else {
        adapter.launch_command(&effective_prompt, idle, resume, session_id)?
    };

    let wrapper_dir = std::env::temp_dir().join(format!("batty-bin-{project_slug}-{member_name}"));
    std::fs::create_dir_all(&wrapper_dir).ok();

    #[cfg(unix)]
    let set_executable = |path: &std::path::Path| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).ok();
    };
    #[cfg(not(unix))]
    let set_executable = |_path: &std::path::Path| {};

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

    let real_batty = resolve_binary("batty");
    let batty_wrapper = wrapper_dir.join("batty");
    std::fs::write(
        &batty_wrapper,
        format!("#!/bin/bash\nexec '{}' \"$@\"\n", real_batty),
    )
    .ok();
    set_executable(&batty_wrapper);

    let script = format!(
        "#!/bin/bash\nexport PATH='{}':\"$PATH\"\nexport BATTY_MEMBER='{member_name}'\ncd '{launch_dir_str}'\n{agent_cmd}\n",
        wrapper_dir.to_string_lossy()
    );
    std::fs::write(&script_path, &script)
        .with_context(|| format!("failed to write launch script {}", script_path.display()))?;

    Ok(format!("bash '{}'", script_path.to_string_lossy()))
}

pub(super) fn canonical_agent_name(agent_name: &str) -> String {
    agent::adapter_from_name(agent_name)
        .map(|adapter| adapter.name().to_string())
        .unwrap_or_else(|| agent_name.to_string())
}

fn shim_agent_type_name(agent_name: &str) -> String {
    match canonical_agent_name(agent_name).as_str() {
        "claude-code" => "claude".to_string(),
        "codex-cli" => "codex".to_string(),
        "kiro-cli" => "kiro".to_string(),
        other => other.to_string(),
    }
}

pub(super) fn new_member_session_id(agent_name: &str) -> Option<String> {
    agent::adapter_from_name(agent_name).and_then(|a| a.new_session_id())
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

fn launch_state_path(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("launch-state.json")
}

pub(super) fn load_launch_state(project_root: &Path) -> HashMap<String, LaunchIdentity> {
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

fn default_claude_projects_root() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
        .join(".claude")
        .join("projects")
}

fn default_codex_sessions_root() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
        .join(".codex")
        .join("sessions")
}

fn resolve_session_id(agent_name: &str, session_id: &str) -> Option<String> {
    match agent_name {
        "codex-cli" => codex_resume_session_id_in(&default_codex_sessions_root(), session_id),
        // Kiro doesn't write session files; the ID is only used for
        // launch-state bookkeeping so we always consider it valid.
        "kiro-cli" => (!session_id.is_empty()).then(|| session_id.to_string()),
        _ => claude_session_id_exists_in(&default_claude_projects_root(), session_id)
            .then(|| session_id.to_string()),
    }
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

#[cfg(test)]
fn codex_session_id_exists_in(sessions_root: &Path, session_id: &str) -> bool {
    codex_resume_session_id_in(sessions_root, session_id).is_some()
}

fn codex_resume_session_id_in(sessions_root: &Path, session_id: &str) -> Option<String> {
    let Ok(years) = fs::read_dir(sessions_root) else {
        return None;
    };

    years.flatten().find_map(|year| {
        let Ok(months) = fs::read_dir(year.path()) else {
            return None;
        };
        months.flatten().find_map(|month| {
            let Ok(days) = fs::read_dir(month.path()) else {
                return None;
            };
            days.flatten().find_map(|day| {
                let Ok(entries) = fs::read_dir(day.path()) else {
                    return None;
                };
                entries.flatten().find_map(|entry| {
                    let path = entry.path();
                    if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                        return None;
                    }
                    let file_id = path.file_stem().and_then(|stem| stem.to_str());
                    let meta_id = read_codex_session_meta_id(&path);
                    if file_id == Some(session_id) || meta_id.as_deref() == Some(session_id) {
                        return meta_id.or_else(|| file_id.map(str::to_string));
                    }
                    None
                })
            })
        })
    })
}

fn resolve_member_launch_session(
    agent_name: &str,
    previous_identity: Option<&LaunchIdentity>,
    resume_requested: bool,
    resolved_previous_session_id: Option<String>,
    duplicate_session_id: bool,
) -> (bool, Option<String>) {
    if agent_name == "codex-cli" {
        if !resume_requested || duplicate_session_id {
            return (false, None);
        }

        if previous_identity
            .and_then(|identity| identity.session_id.as_deref())
            .is_some()
        {
            if let Some(previous_session_id) = resolved_previous_session_id {
                return (true, Some(previous_session_id));
            }
            return (false, None);
        }

        return (false, None);
    }

    if agent_name == "kiro-cli" {
        // Kiro doesn't support resume but still needs a session_id for
        // launch-state tracking and delivery machinery.
        let session_id = new_member_session_id(agent_name);
        return (false, session_id);
    }

    let Some(session_id) = new_member_session_id(agent_name) else {
        return (resume_requested, None);
    };

    if !resume_requested {
        return (false, Some(session_id));
    }

    if duplicate_session_id {
        return (false, Some(session_id));
    }

    if let Some(previous_session_id) = resolved_previous_session_id {
        return (true, Some(previous_session_id));
    }

    (false, Some(session_id))
}

fn read_codex_session_meta_id(path: &Path) -> Option<String> {
    let Ok(file) = fs::File::open(path) else {
        return None;
    };
    let reader = std::io::BufReader::new(file);
    for line in std::io::BufRead::lines(reader).map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if entry.get("type").and_then(serde_json::Value::as_str) != Some("session_meta") {
            continue;
        }
        return entry
            .get("payload")
            .and_then(|payload| payload.get("id"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
    }
    None
}

pub(super) fn duplicate_claude_session_ids(
    state: &HashMap<String, LaunchIdentity>,
) -> HashSet<&str> {
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

#[allow(clippy::too_many_arguments)]
fn format_resume_decision_summary(
    member_name: &str,
    current_agent: &str,
    previous_identity: Option<&LaunchIdentity>,
    resume_requested: bool,
    current_prompt: &str,
    session_available: bool,
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
        } else if previous.session_id.is_some() && !session_available {
            "session missing".to_string()
        } else if member_resume {
            session_id
                .map(short_session_summary)
                .unwrap_or_else(|| "identity matched".to_string())
        } else if previous.session_id.is_none() {
            "no saved session".to_string()
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

pub(super) fn member_session_tracker_config(
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
        Some("kiro") | Some("kiro-cli") => None,
        Some("claude") | Some("claude-code") | None => {
            Some(SessionTrackerConfig::Claude { cwd: work_dir })
        }
        _ => None,
    }
}

fn resolve_binary(name: &str) -> String {
    std::process::Command::new("which")
        .arg(name)
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::config::RoleType;
    use crate::team::hierarchy::MemberInstance;
    use crate::team::standup::MemberState;
    use crate::team::watcher::SessionTrackerConfig;
    use std::collections::HashMap;
    use std::path::Path;

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
            false,
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
    fn shim_agent_type_name_maps_known_aliases() {
        assert_eq!(shim_agent_type_name("claude"), "claude");
        assert_eq!(shim_agent_type_name("claude-code"), "claude");
        assert_eq!(shim_agent_type_name("codex"), "codex");
        assert_eq!(shim_agent_type_name("codex-cli"), "codex");
        assert_eq!(shim_agent_type_name("kiro"), "kiro");
        assert_eq!(shim_agent_type_name("kiro-cli"), "kiro");
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
            false,
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
            false,
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
            false,
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
            content.contains("codex --dangerously-bypass-approvals-and-sandbox 'work the task'")
        );
        // Active (non-resume) codex should NOT use exec
        assert!(!content.contains("exec codex"));
        let agents = std::fs::read_to_string(&agents_path).unwrap();
        assert!(agents.contains("role context"));
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
            false,
        )
        .unwrap();
        let script_path = std::env::temp_dir().join("batty-launch-tmp-eng-2.sh");
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
            false,
        )
        .unwrap();
        let script_path = std::env::temp_dir().join("batty-launch-project-architect.sh");
        let content = std::fs::read_to_string(&script_path).unwrap();
        assert!(content.contains(
            "exec claude --dangerously-skip-permissions --resume '44444444-4444-4444-8444-444444444444'"
        ));
    }

    #[test]
    fn launch_script_resume_codex_uses_explicit_session_id() {
        let tmp = tempfile::tempdir().unwrap();
        let work_dir = tmp.path().join("wt");
        std::fs::create_dir_all(&work_dir).unwrap();

        write_launch_script(
            "eng-1",
            "codex",
            "ignored",
            Some("role context"),
            &work_dir,
            tmp.path(),
            true,
            true,
            Some("55555555-5555-4555-8555-555555555555"),
            false,
        )
        .unwrap();
        let project_slug = tmp.path().file_name().unwrap().to_string_lossy();
        let script_path =
            std::env::temp_dir().join(format!("batty-launch-{project_slug}-eng-1.sh"));
        let content = std::fs::read_to_string(&script_path).unwrap();
        assert!(content.contains(
            "codex resume '55555555-5555-4555-8555-555555555555' --dangerously-bypass-approvals-and-sandbox"
        ));
        assert!(!content.contains("--last"));
    }

    #[test]
    fn launch_script_idle_kiro_uses_agent_config() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path();
        write_launch_script(
            "eng-kiro",
            "kiro",
            "idle role prompt",
            None,
            project,
            project,
            true,
            false,
            None,
            false,
        )
        .unwrap();
        let slug = project.file_name().unwrap().to_string_lossy();
        let script_path = std::env::temp_dir().join(format!("batty-launch-{slug}-eng-kiro.sh"));
        let content = std::fs::read_to_string(&script_path).unwrap();
        assert!(content.contains("--agent batty-eng-kiro"));
        // Agent config file should exist
        assert!(project.join(".kiro/agents/batty-eng-kiro.json").exists());
    }

    #[test]
    fn launch_script_active_kiro_uses_agent_config() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path();
        write_launch_script(
            "eng-kiro-active",
            "kiro",
            "solve the bug",
            None,
            project,
            project,
            false,
            false,
            None,
            false,
        )
        .unwrap();
        let slug = project.file_name().unwrap().to_string_lossy();
        let script_path =
            std::env::temp_dir().join(format!("batty-launch-{slug}-eng-kiro-active.sh"));
        let content = std::fs::read_to_string(&script_path).unwrap();
        assert!(content.contains("--agent batty-eng-kiro-active"));
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
    fn canonical_agent_name_normalizes_aliases() {
        assert_eq!(canonical_agent_name("claude"), "claude-code");
        assert_eq!(canonical_agent_name("claude-code"), "claude-code");
        assert_eq!(canonical_agent_name("codex"), "codex-cli");
        assert_eq!(canonical_agent_name("codex-cli"), "codex-cli");
        assert_eq!(canonical_agent_name("kiro"), "kiro-cli");
        assert_eq!(canonical_agent_name("kiro-cli"), "kiro-cli");
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
    fn no_saved_session_is_not_reported_as_unavailable() {
        let previous = LaunchIdentity {
            agent: "codex-cli".to_string(),
            prompt: "same prompt".to_string(),
            session_id: None,
        };

        let summary = format_resume_decision_summary(
            "eng-1-1",
            "codex-cli",
            Some(&previous),
            true,
            "same prompt",
            true,
            false,
            false,
            None,
        );

        assert!(summary.contains("eng-1-1=no"));
        assert!(summary.contains("no saved session"));
        assert!(!summary.contains("session unavailable"));
    }

    #[test]
    fn resolve_member_launch_session_reuses_saved_claude_session_id() {
        let previous = LaunchIdentity {
            agent: "claude-code".to_string(),
            prompt: "same prompt".to_string(),
            session_id: Some("11111111-1111-4111-8111-111111111111".to_string()),
        };

        let (resume, session_id) = resolve_member_launch_session(
            "claude-code",
            Some(&previous),
            true,
            Some("11111111-1111-4111-8111-111111111111".to_string()),
            false,
        );

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
            resolve_member_launch_session("claude-code", Some(&previous), true, None, false);

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

        let (resume, session_id) = resolve_member_launch_session(
            "claude-code",
            Some(&previous),
            true,
            Some("11111111-1111-4111-8111-111111111111".to_string()),
            true,
        );

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
            resolve_member_launch_session("claude-code", Some(&previous), true, None, false);

        assert!(!resume);
        assert!(session_id.is_some());
        assert_ne!(
            session_id.as_deref(),
            Some("11111111-1111-4111-8111-111111111111")
        );
    }

    #[test]
    fn fresh_claude_launch_plan_does_not_persist_provisional_session_id() {
        let plan = MemberLaunchPlan {
            short_cmd: "exec claude".to_string(),
            work_dir: PathBuf::from("/tmp/project"),
            identity: LaunchIdentity {
                agent: "claude-code".to_string(),
                prompt: "prompt".to_string(),
                session_id: Some("11111111-1111-4111-8111-111111111111".to_string()),
            },
            initial_state: MemberState::Idle,
            activate_watcher: false,
            resume_summary: "architect=no (no saved session)".to_string(),
            persist_session_id: false,
        };

        let persisted = plan.persisted_identity();
        assert_eq!(persisted.agent, "claude-code");
        assert_eq!(persisted.prompt, "prompt");
        assert!(persisted.session_id.is_none());
        assert_eq!(
            plan.identity.session_id.as_deref(),
            Some("11111111-1111-4111-8111-111111111111")
        );
    }

    #[test]
    fn resumed_claude_launch_plan_keeps_session_id_for_persistence() {
        let plan = MemberLaunchPlan {
            short_cmd: "exec claude --resume".to_string(),
            work_dir: PathBuf::from("/tmp/project"),
            identity: LaunchIdentity {
                agent: "claude-code".to_string(),
                prompt: "prompt".to_string(),
                session_id: Some("11111111-1111-4111-8111-111111111111".to_string()),
            },
            initial_state: MemberState::Working,
            activate_watcher: true,
            resume_summary: "architect=yes (session 11111111...)".to_string(),
            persist_session_id: true,
        };

        let persisted = plan.persisted_identity();
        assert_eq!(
            persisted.session_id.as_deref(),
            Some("11111111-1111-4111-8111-111111111111")
        );
    }

    #[test]
    fn resolve_member_launch_session_reuses_saved_codex_session_id() {
        let previous = LaunchIdentity {
            agent: "codex-cli".to_string(),
            prompt: "same prompt".to_string(),
            session_id: Some("codex-session-1".to_string()),
        };

        let (resume, session_id) = resolve_member_launch_session(
            "codex-cli",
            Some(&previous),
            true,
            Some("codex-session-1".to_string()),
            false,
        );

        assert!(resume);
        assert_eq!(session_id.as_deref(), Some("codex-session-1"));
    }

    #[test]
    fn resolve_member_launch_session_starts_fresh_when_saved_codex_session_is_missing() {
        let previous = LaunchIdentity {
            agent: "codex-cli".to_string(),
            prompt: "same prompt".to_string(),
            session_id: Some("codex-session-1".to_string()),
        };

        let (resume, session_id) =
            resolve_member_launch_session("codex-cli", Some(&previous), true, None, false);

        assert!(!resume);
        assert!(session_id.is_none());
    }

    #[test]
    fn resolve_member_launch_session_kiro_gets_fresh_session_id() {
        let previous = LaunchIdentity {
            agent: "kiro-cli".to_string(),
            prompt: "same prompt".to_string(),
            session_id: Some("kiro-session-1".to_string()),
        };

        let (resume, session_id) = resolve_member_launch_session(
            "kiro-cli",
            Some(&previous),
            true,
            Some("kiro-session-1".to_string()),
            false,
        );

        assert!(!resume);
        assert!(session_id.is_some());
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
    fn codex_session_id_exists_in_finds_exact_session_file() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("2026").join("03").join("21");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join("codex-session-1.jsonl"), "{}\n").unwrap();

        assert!(codex_session_id_exists_in(tmp.path(), "codex-session-1"));
        assert!(!codex_session_id_exists_in(tmp.path(), "codex-session-2"));
    }

    #[test]
    fn codex_resume_session_id_in_resolves_payload_id_from_rollout_file() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("2026").join("03").join("21");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("rollout-2026-03-26T13-54-07-sample.jsonl"),
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"019d2b48-3d33-7613-bb3d-d0b4ecd45e2e\"}}\n",
        )
        .unwrap();

        assert_eq!(
            codex_resume_session_id_in(tmp.path(), "rollout-2026-03-26T13-54-07-sample").as_deref(),
            Some("019d2b48-3d33-7613-bb3d-d0b4ecd45e2e")
        );
        assert_eq!(
            codex_resume_session_id_in(tmp.path(), "019d2b48-3d33-7613-bb3d-d0b4ecd45e2e")
                .as_deref(),
            Some("019d2b48-3d33-7613-bb3d-d0b4ecd45e2e")
        );
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
    fn member_session_tracker_config_disables_tracker_for_kiro() {
        let tmp = tempfile::tempdir().unwrap();
        let member = MemberInstance {
            name: "eng-1-1".to_string(),
            role_name: "engineer".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("kiro".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: true,
        };

        let tracker = member_session_tracker_config(tmp.path(), &member);

        assert!(tracker.is_none());
    }

    #[test]
    fn skip_worktree_when_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = crate::team::test_support::init_git_repo(&tmp, "batty-skip-wt");

        let member = MemberInstance {
            name: "eng-1-1".to_string(),
            role_name: "engineer".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
        };

        let daemon = crate::team::test_support::TestDaemonBuilder::new(&repo)
            .members(vec![member.clone()])
            .build();

        let work_dir = daemon.member_work_dir(&member);

        // When use_worktrees is false, member_work_dir returns the project root
        assert_eq!(work_dir, repo);

        // No worktree directory should have been created
        let worktree_path = repo.join(".batty").join("worktrees").join("eng-1-1");
        assert!(!worktree_path.exists());
    }

    #[test]
    fn worktree_setup_when_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = crate::team::test_support::init_git_repo(&tmp, "batty-setup-wt");

        let member = MemberInstance {
            name: "eng-1-1".to_string(),
            role_name: "engineer".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: true,
        };

        let daemon = crate::team::test_support::TestDaemonBuilder::new(&repo)
            .members(vec![member.clone()])
            .build();

        let work_dir = daemon.member_work_dir(&member);

        // When use_worktrees is true, a worktree should be created
        let worktree_path = repo.join(".batty").join("worktrees").join("eng-1-1");
        assert!(worktree_path.exists());
        assert_eq!(work_dir, worktree_path);
    }
}
