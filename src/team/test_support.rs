use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::team::config::{
    AutomationConfig, BoardConfig, OrchestratorPosition, RoleDef, RoleType, StandupConfig,
    TeamConfig, WorkflowMode, WorkflowPolicy,
};
use crate::team::daemon::{DaemonConfig, NudgeSchedule, TeamDaemon};
use crate::team::failure_patterns::FailureTracker;
use crate::team::hierarchy::MemberInstance;
use crate::team::standup::MemberState;
use crate::team::watcher::SessionWatcher;

pub(crate) static PATH_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

pub(crate) struct EnvVarGuard {
    key: &'static str,
    original: Option<String>,
}

impl EnvVarGuard {
    pub(crate) fn set(key: &'static str, value: &str) -> Self {
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

pub(crate) fn git(dir: &Path, args: &[&str]) -> Output {
    let mut last_not_found = None;
    for program in ["git", "/usr/bin/git", "/opt/homebrew/bin/git"] {
        match Command::new(program).args(args).current_dir(dir).output() {
            Ok(output) => return output,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                last_not_found = Some(error);
            }
            Err(error) => panic!("git {:?} failed to run via {program}: {error}", args),
        }
    }

    let error = last_not_found.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "git binary not found")
    });
    panic!("git {:?} failed to run: {error}", args)
}

pub(crate) fn git_ok(dir: &Path, args: &[&str]) {
    let output = git(dir, args);
    assert!(
        output.status.success(),
        "git {:?} failed:\nstdout={}\nstderr={}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Check if a worktree is clean, ignoring batty-managed dirs (.batty/, .cargo/).
pub(crate) fn assert_worktree_clean(dir: &Path) {
    let status = git_stdout(
        dir,
        &["status", "--porcelain", "--", ".", ":(exclude).batty", ":(exclude).cargo"],
    );
    assert!(
        status.trim().is_empty(),
        "worktree should be clean (ignoring .batty/.cargo), got: {status}"
    );
}

pub(crate) fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let output = git(dir, args);
    assert!(
        output.status.success(),
        "git {:?} failed:\nstdout={}\nstderr={}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

pub(crate) fn init_git_repo(tmp: &tempfile::TempDir, package_name: &str) -> PathBuf {
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::create_dir_all(repo.join(".batty").join("team_config")).unwrap();
    std::fs::write(
        repo.join("Cargo.toml"),
        format!("[package]\nname = \"{package_name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
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

pub(crate) fn write_owned_task_file(
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

pub(crate) fn write_owned_task_file_with_context(
    project_root: &Path,
    id: u32,
    title: &str,
    status: &str,
    claimed_by: &str,
    branch: &str,
    worktree_path: &str,
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
            "---\nid: {id}\ntitle: {title}\nstatus: {status}\npriority: critical\nclaimed_by: {claimed_by}\nbranch: {branch}\nworktree_path: {worktree_path}\nclass: standard\n---\n\nTask description.\n"
        ),
    )
    .unwrap();
}

pub(crate) fn setup_fake_claude(tmp: &tempfile::TempDir, member_name: &str) -> (PathBuf, PathBuf) {
    let project_slug = tmp
        .path()
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "default".to_string());
    let fake_bin = std::env::temp_dir().join(format!("batty-bin-{project_slug}-{member_name}"));
    let _ = std::fs::remove_dir_all(&fake_bin);
    std::fs::create_dir_all(&fake_bin).unwrap();

    let fake_log = tmp.path().join(format!("{member_name}-fake-claude.log"));
    let fake_claude = fake_bin.join("claude");
    if let Some(parent) = fake_log.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(
        &fake_claude,
        format!(
            "#!/bin/bash\nmkdir -p '{}'\nprintf '%s\\n' \"$*\" >> '{}'\nsleep 5\n",
            fake_log.parent().unwrap().display(),
            fake_log.display(),
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    (fake_bin, fake_log)
}

pub(crate) fn setup_fake_backend(
    tmp: &tempfile::TempDir,
    program: &str,
    log_name: &str,
) -> (PathBuf, PathBuf) {
    let project_slug = tmp
        .path()
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "default".to_string());
    let fake_bin = std::env::temp_dir().join(format!("batty-bin-{project_slug}-{program}"));
    let _ = std::fs::remove_dir_all(&fake_bin);
    std::fs::create_dir_all(&fake_bin).unwrap();

    let fake_log = tmp.path().join(log_name);
    let fake_program = fake_bin.join(program);
    if let Some(parent) = fake_log.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(
        &fake_program,
        format!(
            "#!/bin/bash\nmkdir -p '{}'\nprintf '%s\\n' \"$*\" >> '{}'\nexit 0\n",
            fake_log.parent().unwrap().display(),
            fake_log.display(),
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_program, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    (fake_bin, fake_log)
}

pub(crate) fn architect_member(name: &str) -> MemberInstance {
    MemberInstance {
        name: name.to_string(),
        role_name: "architect".to_string(),
        role_type: RoleType::Architect,
        agent: Some("claude".to_string()),
        model: None,
        prompt: None,
        posture: None,
        model_class: None,
        provider_overlay: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    }
}

pub(crate) fn manager_member(name: &str, reports_to: Option<&str>) -> MemberInstance {
    MemberInstance {
        name: name.to_string(),
        role_name: name.to_string(),
        role_type: RoleType::Manager,
        agent: Some("claude".to_string()),
        model: None,
        prompt: None,
        posture: None,
        model_class: None,
        provider_overlay: None,
        reports_to: reports_to.map(str::to_string),
        use_worktrees: false,
        ..Default::default()
    }
}

pub(crate) fn engineer_member(
    name: &str,
    reports_to: Option<&str>,
    use_worktrees: bool,
) -> MemberInstance {
    MemberInstance {
        name: name.to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        model: None,
        prompt: None,
        posture: None,
        model_class: None,
        provider_overlay: None,
        reports_to: reports_to.map(str::to_string),
        use_worktrees,
    }
}

pub(crate) fn inferred_role_defs(members: &[MemberInstance]) -> Vec<RoleDef> {
    let mut roles = Vec::new();
    let mut seen = HashSet::new();
    for member in members {
        if !seen.insert(member.role_name.clone()) {
            continue;
        }
        let instances = members
            .iter()
            .filter(|candidate| candidate.role_name == member.role_name)
            .count() as u32;
        let use_worktrees = members
            .iter()
            .any(|candidate| candidate.role_name == member.role_name && candidate.use_worktrees);
        roles.push(RoleDef {
            name: member.role_name.clone(),
            role_type: member.role_type,
            agent: member.agent.clone(),
            model: member.model.clone(),
            auth_mode: None,
            auth_env: vec![],
            instances,
            prompt: None,
            posture: member.posture.clone(),
            model_class: member.model_class.clone(),
            provider_overlay: member.provider_overlay.clone(),
            instance_overrides: HashMap::new(),
            talks_to: vec![],
            channel: None,
            channel_config: None,
            nudge_interval_secs: None,
            receives_standup: None,
            standup_interval_secs: None,
            owns: Vec::new(),
            barrier_group: None,
            use_worktrees,
        });
    }
    roles
}

pub(crate) struct TestDaemonBuilder<'a> {
    project_root: &'a Path,
    session: String,
    members: Vec<MemberInstance>,
    pane_map: HashMap<String, String>,
    states: HashMap<String, MemberState>,
    watchers: Option<HashMap<String, SessionWatcher>>,
    nudges: HashMap<String, NudgeSchedule>,
    workflow_policy: WorkflowPolicy,
    board: BoardConfig,
    automation: AutomationConfig,
    orchestrator_pane: bool,
}

impl<'a> TestDaemonBuilder<'a> {
    pub(crate) fn new(project_root: &'a Path) -> Self {
        Self {
            project_root,
            session: "test".to_string(),
            members: Vec::new(),
            pane_map: HashMap::new(),
            states: HashMap::new(),
            watchers: None,
            nudges: HashMap::new(),
            workflow_policy: WorkflowPolicy::default(),
            board: BoardConfig::default(),
            automation: AutomationConfig::default(),
            orchestrator_pane: true,
        }
    }

    pub(crate) fn session(mut self, session: impl Into<String>) -> Self {
        self.session = session.into();
        self
    }

    pub(crate) fn members(mut self, members: Vec<MemberInstance>) -> Self {
        self.members = members;
        self
    }

    pub(crate) fn pane_map(mut self, pane_map: HashMap<String, String>) -> Self {
        self.pane_map = pane_map;
        self
    }

    pub(crate) fn states(mut self, states: HashMap<String, MemberState>) -> Self {
        self.states = states;
        self
    }

    pub(crate) fn watchers(mut self, watchers: HashMap<String, SessionWatcher>) -> Self {
        self.watchers = Some(watchers);
        self
    }

    pub(crate) fn nudges(mut self, nudges: HashMap<String, NudgeSchedule>) -> Self {
        self.nudges = nudges;
        self
    }

    pub(crate) fn workflow_policy(mut self, workflow_policy: WorkflowPolicy) -> Self {
        self.workflow_policy = workflow_policy;
        self
    }

    pub(crate) fn board(mut self, board: BoardConfig) -> Self {
        self.board = board;
        self
    }

    pub(crate) fn orchestrator_pane(mut self, orchestrator_pane: bool) -> Self {
        self.orchestrator_pane = orchestrator_pane;
        self
    }

    pub(crate) fn build(self) -> TeamDaemon {
        let config = DaemonConfig {
            project_root: self.project_root.to_path_buf(),
            team_config: TeamConfig {
                name: "test".to_string(),
                agent: None,
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: self.workflow_policy,
                board: self.board,
                standup: StandupConfig::default(),
                automation: self.automation,
                automation_sender: None,
                external_senders: Vec::new(),
                orchestrator_pane: self.orchestrator_pane,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                grafana: Default::default(),
                use_shim: false,
                use_sdk_mode: false,
                auto_respawn_on_crash: false,
                shim_health_check_interval_secs: 60,
                shim_health_timeout_secs: 120,
                shim_shutdown_timeout_secs: 30,
                shim_working_state_timeout_secs: 1800,
                pending_queue_max_age_secs: 600,
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles: inferred_role_defs(&self.members),
            },
            session: self.session,
            members: self.members,
            pane_map: self.pane_map,
        };

        let mut daemon = TeamDaemon::new(config).unwrap();
        daemon.states = self.states;
        daemon.nudges = self.nudges;
        daemon.failure_tracker = FailureTracker::new(20);
        daemon.paused_standups = HashSet::new();
        daemon.last_standup = HashMap::new();
        daemon.last_board_rotation = Instant::now();
        daemon.last_auto_dispatch = Instant::now();
        daemon.pipeline_starvation_fired = false;
        daemon.pipeline_starvation_last_fired = None;
        daemon.planning_cycle_last_fired = None;
        daemon.planning_cycle_active = false;
        daemon.retro_generated = false;
        daemon.failed_deliveries = Vec::new();
        daemon.poll_interval = Duration::from_secs(5);
        if let Some(watchers) = self.watchers {
            daemon.watchers = watchers;
        }
        // Test panes are assumed to be already running — pre-confirm readiness so
        // delivery goes through the normal inject-then-inbox path.
        for watcher in daemon.watchers.values_mut() {
            watcher.confirm_ready();
        }
        daemon
    }
}

#[allow(dead_code)]
fn test_role_defs(members: &[MemberInstance]) -> Vec<RoleDef> {
    let mut roles = Vec::new();
    for member in members {
        if let Some(existing) = roles
            .iter_mut()
            .find(|role: &&mut RoleDef| role.name == member.role_name)
        {
            existing.instances += 1;
            existing.use_worktrees |= member.use_worktrees;
            if existing.agent.is_none() {
                existing.agent = member.agent.clone();
            }
            if existing.prompt.is_none() {
                existing.prompt = member.prompt.clone();
            }
            continue;
        }

        roles.push(RoleDef {
            name: member.role_name.clone(),
            role_type: member.role_type,
            agent: member.agent.clone(),
            auth_mode: None,
            auth_env: Vec::new(),
            instances: 1,
            prompt: member.prompt.clone(),
            talks_to: Vec::new(),
            channel: None,
            channel_config: None,
            nudge_interval_secs: None,
            receives_standup: None,
            standup_interval_secs: None,
            owns: Vec::new(),
            barrier_group: None,
            use_worktrees: member.use_worktrees,
            ..Default::default()
        });
    }
    roles
}

pub(crate) fn write_open_task_file(project_root: &Path, id: u32, title: &str, status: &str) {
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

pub(crate) fn write_board_task_file(
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

    let mut content = format!("---\nid: {id}\ntitle: {title}\nstatus: {status}\npriority: high\n");
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

pub(crate) fn backdate_idle_grace(daemon: &mut TeamDaemon, member_name: &str) {
    let grace = Duration::from_secs(
        daemon
            .config
            .team_config
            .automation
            .intervention_idle_grace_secs,
    ) + Duration::from_secs(1);
    daemon
        .idle_started_at
        .insert(member_name.to_string(), Instant::now() - grace);
    if let Some(schedule) = daemon.nudges.get_mut(member_name) {
        schedule.idle_since = Some(Instant::now() - schedule.interval.max(grace));
    }
}
