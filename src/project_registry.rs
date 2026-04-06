use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const REGISTRY_KIND: &str = "batty.projectRegistry";
pub const REGISTRY_SCHEMA_VERSION: u32 = 1;
const REGISTRY_FILENAME: &str = "project-registry.json";
const REGISTRY_PATH_ENV: &str = "BATTY_PROJECT_REGISTRY_PATH";

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
    pub project_root: PathBuf,
    pub board_dir: PathBuf,
    pub team_name: String,
    pub session_name: String,
    pub channel_bindings: Vec<ProjectChannelBinding>,
    pub owner: Option<String>,
    pub tags: Vec<String>,
    pub policy_flags: ProjectPolicyFlags,
}

pub fn registry_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os(REGISTRY_PATH_ENV) {
        return Ok(PathBuf::from(path));
    }

    let home = std::env::var("HOME").context("cannot determine home directory")?;
    Ok(PathBuf::from(home).join(".batty").join(REGISTRY_FILENAME))
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

    match schema_version {
        REGISTRY_SCHEMA_VERSION => {
            let registry: ProjectRegistry = serde_json::from_value(raw)
                .with_context(|| format!("failed to decode registry {}", path.display()))?;
            validate_registry(&registry)?;
            Ok(registry)
        }
        other => bail!(
            "registry {} uses unsupported schemaVersion {}",
            path.display(),
            other
        ),
    }
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

pub fn parse_channel_binding(spec: &str) -> Result<ProjectChannelBinding> {
    let Some((channel, binding)) = spec.split_once('=') else {
        bail!("invalid channel binding '{spec}'; expected <channel>=<binding>");
    };

    let channel = channel.trim();
    let binding = binding.trim();
    if channel.is_empty() || binding.is_empty() {
        bail!("invalid channel binding '{spec}'; channel and binding must be non-empty");
    }

    Ok(ProjectChannelBinding {
        channel: channel.to_string(),
        binding: binding.to_string(),
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

    let mut tags = registration
        .tags
        .into_iter()
        .map(|tag| tag.trim().to_string())
        .filter(|tag| !tag.is_empty())
        .collect::<Vec<_>>();
    tags.sort();
    tags.dedup();

    let mut seen_channels = HashSet::new();
    let mut channel_bindings = Vec::with_capacity(registration.channel_bindings.len());
    for binding in registration.channel_bindings {
        let channel = trim_required("channelBinding.channel", &binding.channel)?;
        let binding_value = trim_required("channelBinding.binding", &binding.binding)?;
        if !seen_channels.insert(channel.clone()) {
            bail!("duplicate channel binding for channel '{}'", channel);
        }
        channel_bindings.push(ProjectChannelBinding {
            channel,
            binding: binding_value,
        });
    }
    channel_bindings.sort_by(|left, right| left.channel.cmp(&right.channel));

    let now = crate::team::now_unix();
    let project = RegisteredProject {
        project_id: registration.project_id,
        name,
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

fn trim_required(field_name: &str, value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("{field_name} cannot be empty");
    }
    Ok(trimmed.to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_registration(project_root: &Path) -> ProjectRegistration {
        ProjectRegistration {
            project_id: "alpha".to_string(),
            name: "Alpha".to_string(),
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
                },
                ProjectChannelBinding {
                    channel: "slack".to_string(),
                    binding: "channel:C123".to_string(),
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

    #[test]
    fn register_and_get_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("alpha");
        std::fs::create_dir_all(project_root.join(".batty/team_config/board")).unwrap();
        let registry_path = tmp.path().join("project-registry.json");

        let created =
            register_project_at(&registry_path, sample_registration(&project_root)).unwrap();
        assert_eq!(created.project_id, "alpha");

        let fetched = get_project_at(&registry_path, "alpha").unwrap().unwrap();
        assert_eq!(fetched, created);

        let listed = load_registry_at(&registry_path).unwrap();
        assert_eq!(listed.projects.len(), 1);
        assert_eq!(listed.schema_version, REGISTRY_SCHEMA_VERSION);
    }

    #[test]
    fn register_rejects_duplicate_session_name() {
        let tmp = tempfile::tempdir().unwrap();
        let registry_path = tmp.path().join("project-registry.json");
        let first_root = tmp.path().join("alpha");
        let second_root = tmp.path().join("beta");
        std::fs::create_dir_all(first_root.join(".batty/team_config/board")).unwrap();
        std::fs::create_dir_all(second_root.join(".batty/team_config/board")).unwrap();

        register_project_at(&registry_path, sample_registration(&first_root)).unwrap();

        let mut second = sample_registration(&second_root);
        second.project_id = "beta".to_string();
        second.name = "Beta".to_string();
        second.team_name = "beta".to_string();

        let error = register_project_at(&registry_path, second).unwrap_err();
        assert!(error.to_string().contains("sessionName 'batty-alpha'"));
    }

    #[test]
    fn unregister_returns_removed_project() {
        let tmp = tempfile::tempdir().unwrap();
        let registry_path = tmp.path().join("project-registry.json");
        let project_root = tmp.path().join("alpha");
        std::fs::create_dir_all(project_root.join(".batty/team_config/board")).unwrap();

        register_project_at(&registry_path, sample_registration(&project_root)).unwrap();
        let removed = unregister_project_at(&registry_path, "alpha")
            .unwrap()
            .unwrap();
        assert_eq!(removed.project_id, "alpha");
        assert!(get_project_at(&registry_path, "alpha").unwrap().is_none());
    }

    #[test]
    fn parse_channel_binding_requires_equals() {
        let error = parse_channel_binding("telegram").unwrap_err();
        assert!(error.to_string().contains("expected <channel>=<binding>"));
    }

    #[test]
    fn load_rejects_unsupported_schema_version() {
        let tmp = tempfile::tempdir().unwrap();
        let registry_path = tmp.path().join("project-registry.json");
        std::fs::write(
            &registry_path,
            r#"{
  "kind": "batty.projectRegistry",
  "schemaVersion": 99,
  "projects": []
}
"#,
        )
        .unwrap();

        let error = load_registry_at(&registry_path).unwrap_err();
        assert!(error.to_string().contains("unsupported schemaVersion 99"));
    }
}
