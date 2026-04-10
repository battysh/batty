use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tmux;

const REGISTRY_KIND: &str = "batty.projectRegistry";
pub const REGISTRY_SCHEMA_VERSION: u32 = 2;
const REGISTRY_FILENAME: &str = "project-registry.json";
const REGISTRY_PATH_ENV: &str = "BATTY_PROJECT_REGISTRY_PATH";

const ROUTING_STATE_KIND: &str = "batty.projectRoutingState";
pub const ROUTING_STATE_SCHEMA_VERSION: u32 = 1;
const ROUTING_STATE_FILENAME: &str = "project-routing-state.json";
const ROUTING_STATE_PATH_ENV: &str = "BATTY_PROJECT_ROUTING_STATE_PATH";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRegistry {
    pub kind: String,
    pub schema_version: u32,
    #[serde(default)]
    pub projects: Vec<RegisteredProject>,
}

impl Default for ProjectRegistry {
    fn default() -> Self {
        Self {
            kind: REGISTRY_KIND.to_string(),
            schema_version: REGISTRY_SCHEMA_VERSION,
            projects: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RegisteredProject {
    pub project_id: String,
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub project_root: PathBuf,
    pub board_dir: PathBuf,
    pub team_name: String,
    pub session_name: String,
    #[serde(default)]
    pub channel_bindings: Vec<ProjectChannelBinding>,
    pub owner: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub policy_flags: ProjectPolicyFlags,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectChannelBinding {
    pub channel: String,
    pub binding: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_binding: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProjectPolicyFlags {
    #[serde(default)]
    pub allow_openclaw_supervision: bool,
    #[serde(default)]
    pub allow_cross_project_routing: bool,
    #[serde(default)]
    pub allow_shared_service_routing: bool,
    #[serde(default)]
    pub archived: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRegistration {
    pub project_id: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub project_root: PathBuf,
    pub board_dir: PathBuf,
    pub team_name: String,
    pub session_name: String,
    pub channel_bindings: Vec<ProjectChannelBinding>,
    pub owner: Option<String>,
    pub tags: Vec<String>,
    pub policy_flags: ProjectPolicyFlags,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ProjectLifecycleState {
    Running,
    Stopped,
    Degraded,
    Recovering,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ProjectLifecycleAction {
    Start,
    Stop,
    Restart,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectHealthSummary {
    pub paused: bool,
    pub watchdog_state: String,
    pub unhealthy_members: Vec<String>,
    pub member_count: usize,
    pub active_member_count: usize,
    pub pending_inbox_count: usize,
    pub triage_backlog_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectPipelineMetrics {
    pub active_task_count: usize,
    pub review_queue_count: usize,
    pub runnable_count: u32,
    pub blocked_count: u32,
    pub stale_in_progress_count: u32,
    pub stale_review_count: u32,
    pub auto_merge_rate: Option<f64>,
    pub rework_rate: Option<f64>,
    pub avg_review_latency_secs: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectStatusDto {
    pub project_id: String,
    pub name: String,
    pub team_name: String,
    pub session_name: String,
    pub project_root: PathBuf,
    pub lifecycle: ProjectLifecycleState,
    pub running: bool,
    pub health: ProjectHealthSummary,
    pub pipeline: ProjectPipelineMetrics,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectLifecycleActionResult {
    pub project_id: String,
    pub action: ProjectLifecycleAction,
    pub changed: bool,
    pub lifecycle: ProjectLifecycleState,
    pub running: bool,
    pub audit_message: String,
    pub status: ProjectStatusDto,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRoutingState {
    pub kind: String,
    pub schema_version: u32,
    #[serde(default)]
    pub selections: Vec<ActiveProjectSelection>,
}

impl Default for ProjectRoutingState {
    fn default() -> Self {
        Self {
            kind: ROUTING_STATE_KIND.to_string(),
            schema_version: ROUTING_STATE_SCHEMA_VERSION,
            selections: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ActiveProjectSelection {
    pub project_id: String,
    pub scope: ActiveProjectScope,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ActiveProjectScope {
    Global,
    Channel {
        channel: String,
        binding: String,
    },
    Thread {
        channel: String,
        binding: String,
        thread_binding: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRoutingRequest {
    pub message: String,
    pub channel: Option<String>,
    pub binding: Option<String>,
    pub thread_binding: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoutingConfidence {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRoutingCandidate {
    pub project_id: String,
    pub reason: String,
    pub score: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRoutingDecision {
    pub selected_project_id: Option<String>,
    pub requires_confirmation: bool,
    pub confidence: RoutingConfidence,
    pub reason: String,
    #[serde(default)]
    pub candidates: Vec<ProjectRoutingCandidate>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectRegistryV1 {
    kind: String,
    schema_version: u32,
    #[serde(default)]
    projects: Vec<RegisteredProjectV1>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegisteredProjectV1 {
    project_id: String,
    name: String,
    project_root: PathBuf,
    board_dir: PathBuf,
    team_name: String,
    session_name: String,
    #[serde(default)]
    channel_bindings: Vec<ProjectChannelBindingV1>,
    owner: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    policy_flags: ProjectPolicyFlags,
    created_at: u64,
    updated_at: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectChannelBindingV1 {
    channel: String,
    binding: String,
}

pub fn registry_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os(REGISTRY_PATH_ENV) {
        return Ok(PathBuf::from(path));
    }

    let home = std::env::var("HOME").context("cannot determine home directory")?;
    Ok(PathBuf::from(home).join(".batty").join(REGISTRY_FILENAME))
}

pub fn routing_state_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os(ROUTING_STATE_PATH_ENV) {
        return Ok(PathBuf::from(path));
    }

    let home = std::env::var("HOME").context("cannot determine home directory")?;
    Ok(PathBuf::from(home)
        .join(".batty")
        .join(ROUTING_STATE_FILENAME))
}

pub fn load_registry() -> Result<ProjectRegistry> {
    load_registry_at(&registry_path()?)
}

pub fn register_project(registration: ProjectRegistration) -> Result<RegisteredProject> {
    register_project_at(&registry_path()?, registration)
}

pub fn unregister_project(project_id: &str) -> Result<Option<RegisteredProject>> {
    unregister_project_at(&registry_path()?, project_id)
}

pub fn list_projects() -> Result<Vec<RegisteredProject>> {
    let mut projects = load_registry()?.projects;
    projects.sort_by(|left, right| left.project_id.cmp(&right.project_id));
    Ok(projects)
}

pub fn get_project(project_id: &str) -> Result<Option<RegisteredProject>> {
    get_project_at(&registry_path()?, project_id)
}

pub fn get_project_status(project_id: &str) -> Result<ProjectStatusDto> {
    let Some(project) = get_project(project_id)? else {
        bail!("project '{}' is not registered", project_id);
    };
    project_status(&project)
}

pub fn start_project(project_id: &str) -> Result<ProjectLifecycleActionResult> {
    let Some(project) = get_project(project_id)? else {
        bail!("project '{}' is not registered", project_id);
    };
    let status = project_status(&project)?;
    if status.running {
        return Ok(ProjectLifecycleActionResult {
            project_id: project.project_id.clone(),
            action: ProjectLifecycleAction::Start,
            changed: false,
            lifecycle: status.lifecycle,
            running: status.running,
            audit_message: format!("project '{}' is already running", project.project_id),
            status,
        });
    }

    crate::team::start_team(&project.project_root, false)?;
    let status = project_status(&project)?;
    Ok(ProjectLifecycleActionResult {
        project_id: project.project_id.clone(),
        action: ProjectLifecycleAction::Start,
        changed: true,
        lifecycle: status.lifecycle,
        running: status.running,
        audit_message: format!(
            "started project '{}' in session {}",
            project.project_id, project.session_name
        ),
        status,
    })
}

pub fn stop_project(project_id: &str) -> Result<ProjectLifecycleActionResult> {
    let Some(project) = get_project(project_id)? else {
        bail!("project '{}' is not registered", project_id);
    };
    let status = project_status(&project)?;
    if !status.running {
        return Ok(ProjectLifecycleActionResult {
            project_id: project.project_id.clone(),
            action: ProjectLifecycleAction::Stop,
            changed: false,
            lifecycle: status.lifecycle,
            running: status.running,
            audit_message: format!("project '{}' is already stopped", project.project_id),
            status,
        });
    }

    crate::team::stop_team(&project.project_root)?;
    let status = project_status(&project)?;
    Ok(ProjectLifecycleActionResult {
        project_id: project.project_id.clone(),
        action: ProjectLifecycleAction::Stop,
        changed: true,
        lifecycle: status.lifecycle,
        running: status.running,
        audit_message: format!(
            "stopped project '{}' and recorded shutdown summary",
            project.project_id
        ),
        status,
    })
}

pub fn restart_project(project_id: &str) -> Result<ProjectLifecycleActionResult> {
    let Some(project) = get_project(project_id)? else {
        bail!("project '{}' is not registered", project_id);
    };
    let before = project_status(&project)?;
    if before.running {
        crate::team::stop_team(&project.project_root)?;
    }
    crate::team::start_team(&project.project_root, false)?;
    let status = project_status(&project)?;
    Ok(ProjectLifecycleActionResult {
        project_id: project.project_id.clone(),
        action: ProjectLifecycleAction::Restart,
        changed: true,
        lifecycle: status.lifecycle,
        running: status.running,
        audit_message: if before.running {
            format!(
                "restarted project '{}' in session {}",
                project.project_id, project.session_name
            )
        } else {
            format!(
                "started stopped project '{}' via restart in session {}",
                project.project_id, project.session_name
            )
        },
        status,
    })
}

pub fn load_routing_state() -> Result<ProjectRoutingState> {
    load_routing_state_at(&routing_state_path()?)
}

pub fn set_active_project(
    project_id: &str,
    scope: ActiveProjectScope,
) -> Result<ActiveProjectSelection> {
    set_active_project_at(&registry_path()?, &routing_state_path()?, project_id, scope)
}

pub fn resolve_project_for_message(
    request: &ProjectRoutingRequest,
) -> Result<ProjectRoutingDecision> {
    resolve_project_for_message_at(&registry_path()?, &routing_state_path()?, request)
}

pub fn load_registry_at(path: &Path) -> Result<ProjectRegistry> {
    if !path.exists() {
        return Ok(ProjectRegistry::default());
    }

    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read registry {}", path.display()))?;
    let raw: Value = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse registry {}", path.display()))?;

    let Some(kind) = raw.get("kind").and_then(Value::as_str) else {
        bail!("registry {} is missing kind", path.display());
    };
    if kind != REGISTRY_KIND {
        bail!(
            "registry {} has unsupported kind '{}' (expected '{}')",
            path.display(),
            kind,
            REGISTRY_KIND
        );
    }

    let Some(schema_version) = raw
        .get("schemaVersion")
        .and_then(Value::as_u64)
        .map(|value| value as u32)
    else {
        bail!("registry {} is missing schemaVersion", path.display());
    };

    let registry = match schema_version {
        1 => migrate_registry_v1(raw)?,
        REGISTRY_SCHEMA_VERSION => serde_json::from_value(raw)
            .with_context(|| format!("failed to decode registry {}", path.display()))?,
        other => {
            bail!(
                "registry {} uses unsupported schemaVersion {}",
                path.display(),
                other
            )
        }
    };
    validate_registry(&registry)?;
    Ok(registry)
}

pub fn load_routing_state_at(path: &Path) -> Result<ProjectRoutingState> {
    if !path.exists() {
        return Ok(ProjectRoutingState::default());
    }

    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read routing state {}", path.display()))?;
    let raw: Value = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse routing state {}", path.display()))?;

    let Some(kind) = raw.get("kind").and_then(Value::as_str) else {
        bail!("routing state {} is missing kind", path.display());
    };
    if kind != ROUTING_STATE_KIND {
        bail!(
            "routing state {} has unsupported kind '{}' (expected '{}')",
            path.display(),
            kind,
            ROUTING_STATE_KIND
        );
    }

    let Some(schema_version) = raw
        .get("schemaVersion")
        .and_then(Value::as_u64)
        .map(|value| value as u32)
    else {
        bail!("routing state {} is missing schemaVersion", path.display());
    };
    if schema_version != ROUTING_STATE_SCHEMA_VERSION {
        bail!(
            "routing state {} uses unsupported schemaVersion {}",
            path.display(),
            schema_version
        );
    }

    let state: ProjectRoutingState = serde_json::from_value(raw)
        .with_context(|| format!("failed to decode routing state {}", path.display()))?;
    validate_routing_state(&state)?;
    Ok(state)
}

pub fn register_project_at(
    path: &Path,
    registration: ProjectRegistration,
) -> Result<RegisteredProject> {
    let mut registry = load_registry_at(path)?;
    let project = normalize_registration(registration)?;
    ensure_unique(&registry, &project)?;

    registry.projects.push(project.clone());
    registry
        .projects
        .sort_by(|left, right| left.project_id.cmp(&right.project_id));
    save_registry(path, &registry)?;
    Ok(project)
}

pub fn unregister_project_at(path: &Path, project_id: &str) -> Result<Option<RegisteredProject>> {
    let mut registry = load_registry_at(path)?;
    if let Some(index) = registry
        .projects
        .iter()
        .position(|project| project.project_id == project_id)
    {
        let removed = registry.projects.remove(index);
        save_registry(path, &registry)?;
        Ok(Some(removed))
    } else {
        Ok(None)
    }
}

pub fn get_project_at(path: &Path, project_id: &str) -> Result<Option<RegisteredProject>> {
    let registry = load_registry_at(path)?;
    Ok(registry
        .projects
        .into_iter()
        .find(|project| project.project_id == project_id))
}

pub fn set_active_project_at(
    registry_path: &Path,
    state_path: &Path,
    project_id: &str,
    scope: ActiveProjectScope,
) -> Result<ActiveProjectSelection> {
    let registry = load_registry_at(registry_path)?;
    if registry
        .projects
        .iter()
        .all(|project| project.project_id != project_id)
    {
        bail!("project '{}' is not registered", project_id);
    }

    let mut state = load_routing_state_at(state_path)?;
    let selection = ActiveProjectSelection {
        project_id: project_id.to_string(),
        scope,
        updated_at: crate::team::now_unix(),
    };

    state
        .selections
        .retain(|existing| !same_scope(&existing.scope, &selection.scope));
    state.selections.push(selection.clone());
    sort_selections(&mut state.selections);
    save_routing_state(state_path, &state)?;
    Ok(selection)
}

pub fn resolve_project_for_message_at(
    registry_path: &Path,
    state_path: &Path,
    request: &ProjectRoutingRequest,
) -> Result<ProjectRoutingDecision> {
    let registry = load_registry_at(registry_path)?;
    let routing_state = load_routing_state_at(state_path).unwrap_or_default();
    let projects = registry
        .projects
        .iter()
        .filter(|project| !project.policy_flags.archived)
        .collect::<Vec<_>>();

    if projects.is_empty() {
        return Ok(ProjectRoutingDecision {
            selected_project_id: None,
            requires_confirmation: true,
            confidence: RoutingConfidence::Low,
            reason: "No active projects are registered.".to_string(),
            candidates: Vec::new(),
        });
    }

    let control_action = looks_like_control_action(&request.message);
    if projects.len() == 1 {
        let project = projects[0];
        let requires_confirmation = control_action;
        return Ok(ProjectRoutingDecision {
            selected_project_id: Some(project.project_id.clone()),
            requires_confirmation,
            confidence: if requires_confirmation {
                RoutingConfidence::Medium
            } else {
                RoutingConfidence::High
            },
            reason: if requires_confirmation {
                format!(
                    "Only one registered project exists ({}), but this looks like a control action and should be confirmed.",
                    project.project_id
                )
            } else {
                format!(
                    "Selected {} because it is the only registered project.",
                    project.project_id
                )
            },
            candidates: vec![ProjectRoutingCandidate {
                project_id: project.project_id.clone(),
                reason: "only registered project".to_string(),
                score: 100,
            }],
        });
    }

    let mut candidates = projects
        .iter()
        .filter_map(|project| score_project(project, &routing_state, request))
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.project_id.cmp(&right.project_id))
    });

    let Some(top) = candidates.first().cloned() else {
        return Ok(ProjectRoutingDecision {
            selected_project_id: None,
            requires_confirmation: true,
            confidence: RoutingConfidence::Low,
            reason: "Message did not identify a project with high confidence. Ask the user to choose a projectId.".to_string(),
            candidates,
        });
    };

    let ambiguous = candidates
        .get(1)
        .is_some_and(|second| second.score + 10 >= top.score);
    let requires_confirmation = ambiguous || !auto_route_allowed(&top, control_action);

    Ok(ProjectRoutingDecision {
        selected_project_id: (!ambiguous).then_some(top.project_id.clone()),
        requires_confirmation,
        confidence: routing_confidence(top.score),
        reason: routing_reason(&top, ambiguous, control_action),
        candidates,
    })
}

pub fn parse_channel_binding(spec: &str) -> Result<ProjectChannelBinding> {
    let Some((channel, binding)) = spec.split_once('=') else {
        bail!("invalid channel binding '{spec}'; expected <channel>=<binding>");
    };

    let channel = trim_required("channelBinding.channel", channel)?;
    let binding = trim_required("channelBinding.binding", binding)?;
    Ok(ProjectChannelBinding {
        channel,
        binding,
        thread_binding: None,
    })
}

pub fn parse_thread_binding(spec: &str) -> Result<ProjectChannelBinding> {
    let Some((channel_spec, thread_binding)) = spec.split_once('#') else {
        bail!("invalid thread binding '{spec}'; expected <channel>=<binding>#<thread-binding>");
    };
    let mut binding = parse_channel_binding(channel_spec)?;
    binding.thread_binding = Some(trim_required(
        "channelBinding.threadBinding",
        thread_binding,
    )?);
    Ok(binding)
}

fn migrate_registry_v1(raw: Value) -> Result<ProjectRegistry> {
    let legacy: ProjectRegistryV1 =
        serde_json::from_value(raw).context("failed to decode legacy schemaVersion 1 registry")?;
    if legacy.kind != REGISTRY_KIND {
        bail!("registry kind must be '{}'", REGISTRY_KIND);
    }
    if legacy.schema_version != 1 {
        bail!("legacy registry schemaVersion must be 1");
    }

    Ok(ProjectRegistry {
        kind: REGISTRY_KIND.to_string(),
        schema_version: REGISTRY_SCHEMA_VERSION,
        projects: legacy
            .projects
            .into_iter()
            .map(|project| RegisteredProject {
                project_id: project.project_id,
                name: project.name,
                aliases: Vec::new(),
                project_root: project.project_root,
                board_dir: project.board_dir,
                team_name: project.team_name,
                session_name: project.session_name,
                channel_bindings: project
                    .channel_bindings
                    .into_iter()
                    .map(|binding| ProjectChannelBinding {
                        channel: binding.channel,
                        binding: binding.binding,
                        thread_binding: None,
                    })
                    .collect(),
                owner: project.owner,
                tags: project.tags,
                policy_flags: project.policy_flags,
                created_at: project.created_at,
                updated_at: project.updated_at,
            })
            .collect(),
    })
}

fn save_registry(path: &Path, registry: &ProjectRegistry) -> Result<()> {
    validate_registry(registry)?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create registry dir {}", parent.display()))?;
    }

    let content = serde_json::to_string_pretty(registry)?;
    std::fs::write(path, format!("{content}\n"))
        .with_context(|| format!("failed to write registry {}", path.display()))?;
    Ok(())
}

fn save_routing_state(path: &Path, state: &ProjectRoutingState) -> Result<()> {
    validate_routing_state(state)?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create routing state dir {}", parent.display()))?;
    }

    let content = serde_json::to_string_pretty(state)?;
    std::fs::write(path, format!("{content}\n"))
        .with_context(|| format!("failed to write routing state {}", path.display()))?;
    Ok(())
}

fn validate_registry(registry: &ProjectRegistry) -> Result<()> {
    if registry.kind != REGISTRY_KIND {
        bail!("registry kind must be '{}'", REGISTRY_KIND);
    }
    if registry.schema_version != REGISTRY_SCHEMA_VERSION {
        bail!("registry schemaVersion must be {}", REGISTRY_SCHEMA_VERSION);
    }

    let mut project_ids = HashSet::new();
    let mut project_roots = HashSet::new();
    let mut team_names = HashSet::new();
    let mut session_names = HashSet::new();
    let mut aliases = HashSet::new();

    for project in &registry.projects {
        validate_project(project)?;

        if !project_ids.insert(project.project_id.clone()) {
            bail!("duplicate projectId '{}'", project.project_id);
        }
        if !project_roots.insert(project.project_root.clone()) {
            bail!("duplicate projectRoot '{}'", project.project_root.display());
        }
        if !team_names.insert(project.team_name.clone()) {
            bail!("duplicate teamName '{}'", project.team_name);
        }
        if !session_names.insert(project.session_name.clone()) {
            bail!("duplicate sessionName '{}'", project.session_name);
        }
        for alias in &project.aliases {
            if !aliases.insert(alias.clone()) {
                bail!("duplicate alias '{}'", alias);
            }
        }
    }

    Ok(())
}

fn validate_routing_state(state: &ProjectRoutingState) -> Result<()> {
    if state.kind != ROUTING_STATE_KIND {
        bail!("routing state kind must be '{}'", ROUTING_STATE_KIND);
    }
    if state.schema_version != ROUTING_STATE_SCHEMA_VERSION {
        bail!(
            "routing state schemaVersion must be {}",
            ROUTING_STATE_SCHEMA_VERSION
        );
    }

    let mut scopes = HashSet::new();
    for selection in &state.selections {
        validate_project_id(&selection.project_id)?;
        if !scopes.insert(selection.scope.clone()) {
            bail!("duplicate active-project scope in routing state");
        }
    }
    Ok(())
}

fn ensure_unique(registry: &ProjectRegistry, project: &RegisteredProject) -> Result<()> {
    if registry
        .projects
        .iter()
        .any(|existing| existing.project_id == project.project_id)
    {
        bail!("projectId '{}' is already registered", project.project_id);
    }
    if registry
        .projects
        .iter()
        .any(|existing| existing.project_root == project.project_root)
    {
        bail!(
            "projectRoot '{}' is already registered",
            project.project_root.display()
        );
    }
    if registry
        .projects
        .iter()
        .any(|existing| existing.team_name == project.team_name)
    {
        bail!("teamName '{}' is already registered", project.team_name);
    }
    if registry
        .projects
        .iter()
        .any(|existing| existing.session_name == project.session_name)
    {
        bail!(
            "sessionName '{}' is already registered",
            project.session_name
        );
    }

    let existing_aliases = registry
        .projects
        .iter()
        .flat_map(|existing| existing.aliases.iter())
        .cloned()
        .collect::<HashSet<_>>();
    for alias in &project.aliases {
        if existing_aliases.contains(alias) {
            bail!("alias '{}' is already registered", alias);
        }
    }
    Ok(())
}

fn normalize_registration(registration: ProjectRegistration) -> Result<RegisteredProject> {
    validate_project_id(&registration.project_id)?;

    let name = trim_required("name", &registration.name)?;
    let team_name = trim_required("teamName", &registration.team_name)?;
    let session_name = trim_required("sessionName", &registration.session_name)?;
    let owner = registration
        .owner
        .as_deref()
        .map(str::trim)
        .and_then(|value| {
            if value.is_empty() {
                None
            } else {
                Some(value.to_string())
            }
        });

    let project_root = normalize_path(&registration.project_root)?;
    let board_dir = normalize_path(&registration.board_dir)?;
    if !board_dir.starts_with(&project_root) {
        bail!(
            "boardDir '{}' must be inside projectRoot '{}'",
            board_dir.display(),
            project_root.display()
        );
    }

    let aliases = normalize_labels("alias", registration.aliases)?;
    let tags = normalize_labels("tag", registration.tags)?;

    let mut seen_bindings = HashSet::new();
    let mut channel_bindings = Vec::with_capacity(registration.channel_bindings.len());
    for binding in registration.channel_bindings {
        let channel = trim_required("channelBinding.channel", &binding.channel)?;
        let binding_value = trim_required("channelBinding.binding", &binding.binding)?;
        let thread_binding = binding
            .thread_binding
            .as_deref()
            .map(|value| trim_required("channelBinding.threadBinding", value))
            .transpose()?;
        let binding_key = (
            channel.clone(),
            binding_value.clone(),
            thread_binding.clone().unwrap_or_default(),
        );
        if !seen_bindings.insert(binding_key) {
            bail!("duplicate channel/thread binding for channel '{}'", channel);
        }
        channel_bindings.push(ProjectChannelBinding {
            channel,
            binding: binding_value,
            thread_binding,
        });
    }
    channel_bindings.sort_by(|left, right| {
        left.channel
            .cmp(&right.channel)
            .then_with(|| left.binding.cmp(&right.binding))
            .then_with(|| left.thread_binding.cmp(&right.thread_binding))
    });

    let now = crate::team::now_unix();
    let project = RegisteredProject {
        project_id: registration.project_id,
        name,
        aliases,
        project_root,
        board_dir,
        team_name,
        session_name,
        channel_bindings,
        owner,
        tags,
        policy_flags: registration.policy_flags,
        created_at: now,
        updated_at: now,
    };

    validate_project(&project)?;
    Ok(project)
}

fn project_status(project: &RegisteredProject) -> Result<ProjectStatusDto> {
    let report = load_project_status_report(project)?;
    let lifecycle = resolve_lifecycle_state(&report);
    let workflow_metrics = report.workflow_metrics.unwrap_or_default();

    Ok(ProjectStatusDto {
        project_id: project.project_id.clone(),
        name: project.name.clone(),
        team_name: project.team_name.clone(),
        session_name: project.session_name.clone(),
        project_root: project.project_root.clone(),
        lifecycle,
        running: report.running,
        health: ProjectHealthSummary {
            paused: report.paused,
            watchdog_state: report.watchdog.state,
            unhealthy_members: report.health.unhealthy_members,
            member_count: report.health.member_count,
            active_member_count: report.health.active_member_count,
            pending_inbox_count: report.health.pending_inbox_count,
            triage_backlog_count: report.health.triage_backlog_count,
        },
        pipeline: ProjectPipelineMetrics {
            active_task_count: report.active_tasks.len(),
            review_queue_count: report.review_queue.len(),
            runnable_count: workflow_metrics.runnable_count,
            blocked_count: workflow_metrics.blocked_count,
            stale_in_progress_count: workflow_metrics.stale_in_progress_count,
            stale_review_count: workflow_metrics.stale_review_count,
            auto_merge_rate: workflow_metrics.auto_merge_rate,
            rework_rate: workflow_metrics.rework_rate,
            avg_review_latency_secs: workflow_metrics.avg_review_latency_secs,
        },
    })
}

fn resolve_lifecycle_state(
    report: &crate::team::status::TeamStatusJsonReport,
) -> ProjectLifecycleState {
    if !report.running {
        ProjectLifecycleState::Stopped
    } else if report.watchdog.state == "restarting" {
        ProjectLifecycleState::Recovering
    } else if report.paused
        || report.watchdog.state == "circuit-open"
        || !report.health.unhealthy_members.is_empty()
    {
        ProjectLifecycleState::Degraded
    } else {
        ProjectLifecycleState::Running
    }
}

fn load_project_status_report(
    project: &RegisteredProject,
) -> Result<crate::team::status::TeamStatusJsonReport> {
    let config_path = crate::team::team_config_path(&project.project_root);
    if !config_path.exists() {
        bail!(
            "no team config found for project '{}' at {}",
            project.project_id,
            config_path.display()
        );
    }

    let team_config = crate::team::config::TeamConfig::load(&config_path)?;
    let members = crate::team::hierarchy::resolve_hierarchy(&team_config)?;
    let session_running = tmux::session_exists(&project.session_name);
    let runtime_statuses = if session_running {
        crate::team::status::list_runtime_member_statuses(&project.session_name).unwrap_or_default()
    } else {
        std::collections::HashMap::new()
    };
    let pending_inbox_counts =
        crate::team::status::pending_inbox_counts(&project.project_root, &members);
    let triage_backlog_counts =
        crate::team::status::triage_backlog_counts(&project.project_root, &members);
    let owned_task_buckets =
        crate::team::status::owned_task_buckets(&project.project_root, &members);
    let branch_mismatches =
        crate::team::status::branch_mismatch_by_member(&project.project_root, &members);
    let worktree_staleness =
        crate::team::status::worktree_staleness_by_member(&project.project_root, &members);
    let agent_health = crate::team::status::agent_health_by_member(&project.project_root, &members);
    let paused = crate::team::pause_marker_path(&project.project_root).exists();
    let rows = crate::team::status::build_team_status_rows(
        &members,
        session_running,
        &runtime_statuses,
        &pending_inbox_counts,
        &triage_backlog_counts,
        &owned_task_buckets,
        &branch_mismatches,
        &worktree_staleness,
        &agent_health,
    );
    let workflow_metrics =
        crate::team::status::workflow_metrics_section(&project.project_root, &members)
            .map(|(_, metrics)| metrics);
    let watchdog =
        crate::team::status::load_watchdog_status(&project.project_root, session_running);
    let (active_tasks, review_queue) =
        crate::team::status::board_status_task_queues(&project.project_root)?;

    Ok(crate::team::status::build_team_status_json_report(
        crate::team::status::TeamStatusJsonReportInput {
            team: team_config.name,
            session: project.session_name.clone(),
            session_running,
            paused,
            watchdog,
            workflow_metrics,
            active_tasks,
            review_queue,
            engineer_profiles: None,
            optional_subsystems: None,
            members: rows,
        },
    ))
}

fn validate_project(project: &RegisteredProject) -> Result<()> {
    validate_project_id(&project.project_id)?;
    trim_required("name", &project.name)?;
    trim_required("teamName", &project.team_name)?;
    trim_required("sessionName", &project.session_name)?;
    if !project.project_root.is_absolute() {
        bail!(
            "projectRoot '{}' must be absolute",
            project.project_root.display()
        );
    }
    if !project.board_dir.is_absolute() {
        bail!(
            "boardDir '{}' must be absolute",
            project.board_dir.display()
        );
    }
    if !project.board_dir.starts_with(&project.project_root) {
        bail!(
            "boardDir '{}' must be inside projectRoot '{}'",
            project.board_dir.display(),
            project.project_root.display()
        );
    }
    for alias in &project.aliases {
        validate_label("alias", alias)?;
    }
    for tag in &project.tags {
        validate_label("tag", tag)?;
    }
    Ok(())
}

fn validate_project_id(project_id: &str) -> Result<()> {
    if project_id.is_empty() {
        bail!("projectId cannot be empty");
    }
    if !project_id
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '-' | '_' | '.'))
    {
        bail!(
            "projectId '{}' must use lowercase ASCII letters, digits, '.', '-', or '_'",
            project_id
        );
    }
    Ok(())
}

fn validate_label(field_name: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        bail!("{field_name} cannot be empty");
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '-' | '_' | '.'))
    {
        bail!(
            "{field_name} '{}' must use lowercase ASCII letters, digits, '.', '-', or '_'",
            value
        );
    }
    Ok(())
}

fn trim_required(field_name: &str, value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("{field_name} cannot be empty");
    }
    Ok(trimmed.to_string())
}

fn normalize_labels(field_name: &str, values: Vec<String>) -> Result<Vec<String>> {
    let mut labels = values
        .into_iter()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    labels.sort();
    labels.dedup();
    for label in &labels {
        validate_label(field_name, label)?;
    }
    Ok(labels)
}

fn normalize_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("failed to determine current directory")?
            .join(path)
    };

    if absolute.exists() {
        absolute
            .canonicalize()
            .with_context(|| format!("failed to canonicalize {}", absolute.display()))
    } else {
        Ok(absolute)
    }
}

fn sort_selections(selections: &mut [ActiveProjectSelection]) {
    selections.sort_by(|left, right| {
        scope_rank(&left.scope)
            .cmp(&scope_rank(&right.scope))
            .then_with(|| left.project_id.cmp(&right.project_id))
    });
}

fn scope_rank(scope: &ActiveProjectScope) -> (&'static str, &str, &str, &str) {
    match scope {
        ActiveProjectScope::Global => ("global", "", "", ""),
        ActiveProjectScope::Channel { channel, binding } => ("channel", channel, binding, ""),
        ActiveProjectScope::Thread {
            channel,
            binding,
            thread_binding,
        } => ("thread", channel, binding, thread_binding),
    }
}

fn same_scope(left: &ActiveProjectScope, right: &ActiveProjectScope) -> bool {
    match (left, right) {
        (ActiveProjectScope::Global, ActiveProjectScope::Global) => true,
        (
            ActiveProjectScope::Channel {
                channel: left_channel,
                binding: left_binding,
            },
            ActiveProjectScope::Channel {
                channel: right_channel,
                binding: right_binding,
            },
        ) => left_channel == right_channel && left_binding == right_binding,
        (
            ActiveProjectScope::Thread {
                channel: left_channel,
                binding: left_binding,
                thread_binding: left_thread,
            },
            ActiveProjectScope::Thread {
                channel: right_channel,
                binding: right_binding,
                thread_binding: right_thread,
            },
        ) => {
            left_channel == right_channel
                && left_binding == right_binding
                && left_thread == right_thread
        }
        _ => false,
    }
}

fn score_project(
    project: &RegisteredProject,
    routing_state: &ProjectRoutingState,
    request: &ProjectRoutingRequest,
) -> Option<ProjectRoutingCandidate> {
    let message = request.message.to_ascii_lowercase();
    let tokens = normalized_tokens(&message);
    let mut score = 0u32;
    let mut reasons = Vec::new();

    if tokens.iter().any(|token| token == &project.project_id) {
        score = score.max(100);
        reasons.push("explicit projectId mention".to_string());
    }

    if project
        .aliases
        .iter()
        .any(|alias| tokens.iter().any(|token| token == alias))
    {
        score = score.max(98);
        reasons.push("explicit alias mention".to_string());
    }

    if phrase_match(&message, &project.name.to_ascii_lowercase()) {
        score = score.max(95);
        reasons.push("project name mention".to_string());
    }

    let mentioned_tags = project
        .tags
        .iter()
        .filter(|tag| tokens.iter().any(|token| token == *tag))
        .cloned()
        .collect::<Vec<_>>();
    if !mentioned_tags.is_empty() {
        score = score.max(70);
        reasons.push(format!("tag match ({})", mentioned_tags.join(", ")));
    }

    if let Some(reason) = thread_binding_match(project, request) {
        score = score.max(100);
        reasons.push(reason);
    } else if let Some(reason) = channel_binding_match(project, request) {
        score = score.max(90);
        reasons.push(reason);
    }

    if let Some(reason) = active_selection_match(project, routing_state, request) {
        score = score.max(reason.0);
        reasons.push(reason.1);
    }

    if score == 0 {
        None
    } else {
        Some(ProjectRoutingCandidate {
            project_id: project.project_id.clone(),
            reason: reasons.join("; "),
            score,
        })
    }
}

fn thread_binding_match(
    project: &RegisteredProject,
    request: &ProjectRoutingRequest,
) -> Option<String> {
    let channel = request.channel.as_deref()?;
    let binding = request.binding.as_deref()?;
    let thread_binding = request.thread_binding.as_deref()?;
    project.channel_bindings.iter().find_map(|candidate| {
        (candidate.channel == channel
            && candidate.binding == binding
            && candidate.thread_binding.as_deref() == Some(thread_binding))
        .then(|| "thread binding match".to_string())
    })
}

fn channel_binding_match(
    project: &RegisteredProject,
    request: &ProjectRoutingRequest,
) -> Option<String> {
    let channel = request.channel.as_deref()?;
    let binding = request.binding.as_deref()?;
    project.channel_bindings.iter().find_map(|candidate| {
        (candidate.channel == channel
            && candidate.binding == binding
            && candidate.thread_binding.is_none())
        .then(|| "channel binding match".to_string())
    })
}

fn active_selection_match(
    project: &RegisteredProject,
    routing_state: &ProjectRoutingState,
    request: &ProjectRoutingRequest,
) -> Option<(u32, String)> {
    routing_state
        .selections
        .iter()
        .find(|selection| selection.project_id == project.project_id)
        .and_then(|selection| match &selection.scope {
            ActiveProjectScope::Thread {
                channel,
                binding,
                thread_binding,
            } => (request.channel.as_deref() == Some(channel.as_str())
                && request.binding.as_deref() == Some(binding.as_str())
                && request.thread_binding.as_deref() == Some(thread_binding.as_str()))
            .then(|| (96, "active project selected for this thread".to_string())),
            ActiveProjectScope::Channel { channel, binding } => (request.channel.as_deref()
                == Some(channel.as_str())
                && request.binding.as_deref() == Some(binding.as_str()))
            .then(|| (80, "active project selected for this channel".to_string())),
            ActiveProjectScope::Global => Some((65, "global active project selection".to_string())),
        })
}

fn normalized_tokens(value: &str) -> Vec<String> {
    let mut current = String::new();
    let mut tokens = Vec::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            current.push(ch);
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn phrase_match(message: &str, phrase: &str) -> bool {
    if phrase.is_empty() {
        return false;
    }
    message.contains(phrase)
}

fn looks_like_control_action(message: &str) -> bool {
    let tokens = normalized_tokens(&message.to_ascii_lowercase());
    tokens.iter().any(|token| {
        matches!(
            token.as_str(),
            "stop"
                | "restart"
                | "delete"
                | "archive"
                | "merge"
                | "ship"
                | "deploy"
                | "kill"
                | "pause"
                | "resume"
                | "assign"
                | "unregister"
                | "register"
                | "instruct"
        )
    })
}

fn auto_route_allowed(candidate: &ProjectRoutingCandidate, control_action: bool) -> bool {
    if candidate.score >= 98 {
        return true;
    }
    if candidate.reason.contains("thread binding match") {
        return true;
    }
    if control_action {
        return candidate.reason.contains("explicit projectId mention")
            || candidate.reason.contains("explicit alias mention")
            || candidate.reason.contains("thread binding match");
    }
    candidate.score >= 80
}

fn routing_confidence(score: u32) -> RoutingConfidence {
    if score >= 95 {
        RoutingConfidence::High
    } else if score >= 75 {
        RoutingConfidence::Medium
    } else {
        RoutingConfidence::Low
    }
}

fn routing_reason(top: &ProjectRoutingCandidate, ambiguous: bool, control_action: bool) -> String {
    if ambiguous {
        return format!(
            "Routing is ambiguous across multiple projects. Top match was {} because {}.",
            top.project_id, top.reason
        );
    }
    if control_action && !auto_route_allowed(top, true) {
        return format!(
            "Matched {} because {}, but this looks like a control action and requires confirmation.",
            top.project_id, top.reason
        );
    }
    format!("Selected {} because {}.", top.project_id, top.reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_registration(project_root: &Path) -> ProjectRegistration {
        ProjectRegistration {
            project_id: "alpha".to_string(),
            name: "Alpha".to_string(),
            aliases: vec!["batty".to_string()],
            project_root: project_root.to_path_buf(),
            board_dir: project_root
                .join(".batty")
                .join("team_config")
                .join("board"),
            team_name: "alpha".to_string(),
            session_name: "batty-alpha".to_string(),
            channel_bindings: vec![
                ProjectChannelBinding {
                    channel: "telegram".to_string(),
                    binding: "chat:123".to_string(),
                    thread_binding: None,
                },
                ProjectChannelBinding {
                    channel: "slack".to_string(),
                    binding: "channel:C123".to_string(),
                    thread_binding: Some("thread:abc".to_string()),
                },
            ],
            owner: Some("ops".to_string()),
            tags: vec!["core".to_string(), "pilot".to_string()],
            policy_flags: ProjectPolicyFlags {
                allow_openclaw_supervision: true,
                allow_cross_project_routing: false,
                allow_shared_service_routing: true,
                archived: false,
            },
        }
    }

    fn write_beta(registry_path: &Path, root: &Path) {
        let beta_root = root.join("beta");
        std::fs::create_dir_all(beta_root.join(".batty/team_config/board")).unwrap();
        register_project_at(
            registry_path,
            ProjectRegistration {
                project_id: "beta".to_string(),
                name: "Beta".to_string(),
                aliases: vec!["other".to_string()],
                project_root: beta_root.clone(),
                board_dir: beta_root.join(".batty/team_config/board"),
                team_name: "beta".to_string(),
                session_name: "batty-beta".to_string(),
                channel_bindings: vec![ProjectChannelBinding {
                    channel: "telegram".to_string(),
                    binding: "chat:999".to_string(),
                    thread_binding: None,
                }],
                owner: None,
                tags: vec!["backend".to_string()],
                policy_flags: ProjectPolicyFlags::default(),
            },
        )
        .unwrap();
    }

    #[test]
    fn register_and_get_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("alpha");
        std::fs::create_dir_all(project_root.join(".batty/team_config/board")).unwrap();
        let registry_path = tmp.path().join("project-registry.json");

        let created =
            register_project_at(&registry_path, sample_registration(&project_root)).unwrap();
        assert_eq!(created.project_id, "alpha");
        assert_eq!(created.aliases, vec!["batty"]);
        assert_eq!(created.channel_bindings.len(), 2);

        let fetched = get_project_at(&registry_path, "alpha").unwrap().unwrap();
        assert_eq!(fetched, created);

        let listed = load_registry_at(&registry_path).unwrap();
        assert_eq!(listed.projects.len(), 1);
        assert_eq!(listed.schema_version, REGISTRY_SCHEMA_VERSION);
    }

    #[test]
    fn load_migrates_schema_version_one() {
        let tmp = tempfile::tempdir().unwrap();
        let registry_path = tmp.path().join("project-registry.json");
        std::fs::write(
            &registry_path,
            r#"{
  "kind": "batty.projectRegistry",
  "schemaVersion": 1,
  "projects": [
    {
      "projectId": "alpha",
      "name": "Alpha",
      "projectRoot": "/tmp/alpha",
      "boardDir": "/tmp/alpha/.batty/team_config/board",
      "teamName": "alpha",
      "sessionName": "batty-alpha",
      "channelBindings": [{ "channel": "telegram", "binding": "chat:123" }],
      "owner": null,
      "tags": ["core"],
      "policyFlags": {
        "allowOpenclawSupervision": true,
        "allowCrossProjectRouting": false,
        "allowSharedServiceRouting": false,
        "archived": false
      },
      "createdAt": 1,
      "updatedAt": 1
    }
  ]
}
"#,
        )
        .unwrap();

        let registry = load_registry_at(&registry_path).unwrap();
        assert_eq!(registry.schema_version, 2);
        assert!(registry.projects[0].aliases.is_empty());
        assert_eq!(
            registry.projects[0].channel_bindings[0].thread_binding,
            None
        );
    }

    #[test]
    fn parse_thread_binding_requires_hash_separator() {
        let error = parse_thread_binding("slack=channel:C123").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("expected <channel>=<binding>#<thread-binding>")
        );
    }

    #[test]
    fn set_active_project_upserts_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let registry_path = tmp.path().join("project-registry.json");
        let state_path = tmp.path().join("project-routing-state.json");
        let project_root = tmp.path().join("alpha");
        std::fs::create_dir_all(project_root.join(".batty/team_config/board")).unwrap();
        register_project_at(&registry_path, sample_registration(&project_root)).unwrap();

        set_active_project_at(
            &registry_path,
            &state_path,
            "alpha",
            ActiveProjectScope::Global,
        )
        .unwrap();
        set_active_project_at(
            &registry_path,
            &state_path,
            "alpha",
            ActiveProjectScope::Channel {
                channel: "telegram".to_string(),
                binding: "chat:123".to_string(),
            },
        )
        .unwrap();

        let state = load_routing_state_at(&state_path).unwrap();
        assert_eq!(state.selections.len(), 2);
    }

    #[test]
    fn resolve_prefers_explicit_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let registry_path = tmp.path().join("project-registry.json");
        let state_path = tmp.path().join("project-routing-state.json");
        let project_root = tmp.path().join("alpha");
        std::fs::create_dir_all(project_root.join(".batty/team_config/board")).unwrap();
        register_project_at(&registry_path, sample_registration(&project_root)).unwrap();
        write_beta(&registry_path, tmp.path());

        let decision = resolve_project_for_message_at(
            &registry_path,
            &state_path,
            &ProjectRoutingRequest {
                message: "check batty".to_string(),
                channel: None,
                binding: None,
                thread_binding: None,
            },
        )
        .unwrap();

        assert_eq!(decision.selected_project_id.as_deref(), Some("alpha"));
        assert!(!decision.requires_confirmation);
        assert_eq!(decision.confidence, RoutingConfidence::High);
    }

    #[test]
    fn resolve_uses_thread_binding_as_high_confidence() {
        let tmp = tempfile::tempdir().unwrap();
        let registry_path = tmp.path().join("project-registry.json");
        let state_path = tmp.path().join("project-routing-state.json");
        let project_root = tmp.path().join("alpha");
        std::fs::create_dir_all(project_root.join(".batty/team_config/board")).unwrap();
        register_project_at(&registry_path, sample_registration(&project_root)).unwrap();
        write_beta(&registry_path, tmp.path());

        let decision = resolve_project_for_message_at(
            &registry_path,
            &state_path,
            &ProjectRoutingRequest {
                message: "check status".to_string(),
                channel: Some("slack".to_string()),
                binding: Some("channel:C123".to_string()),
                thread_binding: Some("thread:abc".to_string()),
            },
        )
        .unwrap();

        assert_eq!(decision.selected_project_id.as_deref(), Some("alpha"));
        assert!(!decision.requires_confirmation);
        assert!(decision.reason.contains("thread binding"));
    }

    #[test]
    fn resolve_requires_confirmation_for_control_action_from_global_active_project() {
        let tmp = tempfile::tempdir().unwrap();
        let registry_path = tmp.path().join("project-registry.json");
        let state_path = tmp.path().join("project-routing-state.json");
        let project_root = tmp.path().join("alpha");
        std::fs::create_dir_all(project_root.join(".batty/team_config/board")).unwrap();
        register_project_at(&registry_path, sample_registration(&project_root)).unwrap();
        write_beta(&registry_path, tmp.path());
        set_active_project_at(
            &registry_path,
            &state_path,
            "alpha",
            ActiveProjectScope::Global,
        )
        .unwrap();

        let decision = resolve_project_for_message_at(
            &registry_path,
            &state_path,
            &ProjectRoutingRequest {
                message: "restart it".to_string(),
                channel: None,
                binding: None,
                thread_binding: None,
            },
        )
        .unwrap();

        assert_eq!(decision.selected_project_id.as_deref(), Some("alpha"));
        assert!(decision.requires_confirmation);
        assert_eq!(decision.confidence, RoutingConfidence::Low);
    }

    #[test]
    fn resolve_requires_clarification_when_only_generic_tag_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let registry_path = tmp.path().join("project-registry.json");
        let state_path = tmp.path().join("project-routing-state.json");
        let alpha_root = tmp.path().join("alpha");
        std::fs::create_dir_all(alpha_root.join(".batty/team_config/board")).unwrap();
        register_project_at(&registry_path, sample_registration(&alpha_root)).unwrap();
        let gamma_root = tmp.path().join("gamma");
        std::fs::create_dir_all(gamma_root.join(".batty/team_config/board")).unwrap();
        register_project_at(
            &registry_path,
            ProjectRegistration {
                project_id: "gamma".to_string(),
                name: "Gamma".to_string(),
                aliases: vec!["gamma".to_string()],
                project_root: gamma_root.clone(),
                board_dir: gamma_root.join(".batty/team_config/board"),
                team_name: "gamma".to_string(),
                session_name: "batty-gamma".to_string(),
                channel_bindings: Vec::new(),
                owner: None,
                tags: vec!["core".to_string()],
                policy_flags: ProjectPolicyFlags::default(),
            },
        )
        .unwrap();

        let decision = resolve_project_for_message_at(
            &registry_path,
            &state_path,
            &ProjectRoutingRequest {
                message: "check the core project".to_string(),
                channel: None,
                binding: None,
                thread_binding: None,
            },
        )
        .unwrap();

        assert!(decision.selected_project_id.is_none());
        assert!(decision.requires_confirmation);
        assert!(
            decision.reason.contains("ambiguous") || decision.reason.contains("high confidence")
        );
    }

    #[test]
    fn resolve_lifecycle_state_maps_stopped_recovering_and_degraded() {
        let base = crate::team::status::TeamStatusJsonReport {
            team: "batty".to_string(),
            session: "batty-batty".to_string(),
            running: true,
            paused: false,
            watchdog: crate::team::status::WatchdogStatus {
                state: "running".to_string(),
                restart_count: 0,
                current_backoff_secs: None,
                last_exit_reason: None,
            },
            health: crate::team::status::TeamStatusHealth {
                session_running: true,
                paused: false,
                member_count: 3,
                active_member_count: 1,
                pending_inbox_count: 0,
                triage_backlog_count: 0,
                unhealthy_members: Vec::new(),
            },
            workflow_metrics: None,
            active_tasks: Vec::new(),
            review_queue: Vec::new(),
            engineer_profiles: None,
            members: Vec::new(),
            optional_subsystems: None,
        };

        let mut stopped = base.clone();
        stopped.running = false;
        assert_eq!(
            resolve_lifecycle_state(&stopped),
            ProjectLifecycleState::Stopped
        );

        let mut recovering = base.clone();
        recovering.watchdog.state = "restarting".to_string();
        assert_eq!(
            resolve_lifecycle_state(&recovering),
            ProjectLifecycleState::Recovering
        );

        let mut degraded = base.clone();
        degraded.health.unhealthy_members.push("eng-1".to_string());
        assert_eq!(
            resolve_lifecycle_state(&degraded),
            ProjectLifecycleState::Degraded
        );

        assert_eq!(
            resolve_lifecycle_state(&base),
            ProjectLifecycleState::Running
        );
    }

    #[test]
    fn project_status_dto_serializes_stable_camel_case_shape() {
        let dto = ProjectStatusDto {
            project_id: "alpha".to_string(),
            name: "Alpha".to_string(),
            team_name: "alpha-team".to_string(),
            session_name: "batty-alpha".to_string(),
            project_root: PathBuf::from("/tmp/alpha"),
            lifecycle: ProjectLifecycleState::Recovering,
            running: true,
            health: ProjectHealthSummary {
                paused: false,
                watchdog_state: "restarting".to_string(),
                unhealthy_members: vec!["eng-1".to_string()],
                member_count: 4,
                active_member_count: 2,
                pending_inbox_count: 3,
                triage_backlog_count: 1,
            },
            pipeline: ProjectPipelineMetrics {
                active_task_count: 2,
                review_queue_count: 1,
                runnable_count: 5,
                blocked_count: 1,
                stale_in_progress_count: 0,
                stale_review_count: 1,
                auto_merge_rate: Some(0.75),
                rework_rate: Some(0.2),
                avg_review_latency_secs: Some(120.0),
            },
        };

        let value = serde_json::to_value(&dto).unwrap();
        assert_eq!(value["projectId"], "alpha");
        assert_eq!(value["teamName"], "alpha-team");
        assert_eq!(value["sessionName"], "batty-alpha");
        assert_eq!(value["lifecycle"], "recovering");
        assert_eq!(value["health"]["watchdogState"], "restarting");
        assert_eq!(value["pipeline"]["activeTaskCount"], 2);
        assert_eq!(value["pipeline"]["avgReviewLatencySecs"], 120.0);
    }
}
