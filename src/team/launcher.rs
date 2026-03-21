use super::*;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct LaunchIdentity {
    pub(super) agent: String,
    pub(super) prompt: String,
    pub(super) session_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct MemberLaunchPlan {
    short_cmd: String,
    pub(super) identity: LaunchIdentity,
    initial_state: MemberState,
    activate_watcher: bool,
    resume_summary: String,
}

impl TeamDaemon {
    pub(super) fn prepare_member_launch(
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

    pub(super) fn apply_member_launch(
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
        self.record_agent_spawned(&member.name);
        Ok(())
    }

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

    pub(super) fn sync_launch_state_session_ids(&self) -> Result<()> {
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
) -> Result<String> {
    let project_slug = project_root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
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
        "#!/bin/bash\nexport PATH='{}':\"$PATH\"\ncd '{launch_dir_str}'\n{agent_cmd}\n",
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

pub(super) fn new_member_session_id(agent_name: &str) -> Option<String> {
    (agent_name == "claude-code").then(|| Uuid::new_v4().to_string())
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

    (false, Some(session_id))
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
    fn canonical_agent_name_normalizes_aliases() {
        assert_eq!(canonical_agent_name("claude"), "claude-code");
        assert_eq!(canonical_agent_name("claude-code"), "claude-code");
        assert_eq!(canonical_agent_name("codex"), "codex-cli");
        assert_eq!(canonical_agent_name("codex-cli"), "codex-cli");
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
}
