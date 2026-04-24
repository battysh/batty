use std::path::Path;

use super::config::RoleType;
use super::hierarchy::MemberInstance;

const POSTURE_DEEP_WORKER: &str = include_str!("templates/postures/deep_worker.md");
const POSTURE_FAST_LANE: &str = include_str!("templates/postures/fast_lane.md");
const POSTURE_ORCHESTRATOR: &str = include_str!("templates/postures/orchestrator.md");

const MODEL_CLASS_FRONTIER: &str = include_str!("templates/model_classes/frontier.md");
const MODEL_CLASS_STANDARD: &str = include_str!("templates/model_classes/standard.md");
const MODEL_CLASS_FAST: &str = include_str!("templates/model_classes/fast.md");

const PROVIDER_CLAUDE: &str = "## Provider: Claude\n- Prefer explicit delegation and clear acceptance criteria when coordinating work\n- Use larger synthesis passes when the full local context is available\n";
const PROVIDER_CODEX: &str = "## Provider: Codex\n- Work in explicit implementation steps with concrete verification after each meaningful change\n- Prefer reading the directly relevant files before editing and keep progress updates factual\n";
const PROVIDER_GEMINI: &str = "## Provider: Gemini\n- Keep tool use disciplined and summarize conclusions before moving to the next step\n- When a task depends on uncertain code paths, verify them directly instead of assuming\n";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptContext {
    pub posture: Option<String>,
    pub model_class: Option<String>,
    pub provider_overlay: Option<String>,
}

pub fn compose_prompt(
    base_role: &str,
    posture: Option<&str>,
    model_class: Option<&str>,
    provider_overlay: Option<&str>,
) -> String {
    let mut layers = vec![base_role.trim_end().to_string()];

    if let Some(text) = posture.and_then(load_posture) {
        layers.push(text.to_string());
    }
    if let Some(text) = model_class.and_then(load_model_class) {
        layers.push(text.to_string());
    }
    if let Some(text) = provider_overlay.and_then(load_provider_overlay) {
        layers.push(text.to_string());
    }

    layers.join("\n\n")
}

pub fn render_member_prompt(
    member: &MemberInstance,
    config_dir: &Path,
    context: &PromptContext,
) -> String {
    let path = config_dir.join(
        member
            .prompt
            .as_deref()
            .unwrap_or(default_prompt_file(member.role_type)),
    );
    let content = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        format!(
            "You are {} (role: {:?}). Work on assigned tasks.",
            member.name, member.role_type
        )
    });
    let base = content
        .replace("{{member_name}}", &member.name)
        .replace("{{role_name}}", &member.role_name)
        .replace(
            "{{reports_to}}",
            member.reports_to.as_deref().unwrap_or("none"),
        );
    compose_prompt(
        &base,
        context.posture.as_deref(),
        context.model_class.as_deref(),
        context.provider_overlay.as_deref(),
    )
}

pub fn resolve_prompt_context(member: &MemberInstance) -> PromptContext {
    let provider_overlay = member
        .provider_overlay
        .clone()
        .or_else(|| infer_provider_overlay(member.agent.as_deref()));
    let model_class = member.model_class.clone().or_else(|| {
        infer_model_class(member.model.as_deref(), member.agent.as_deref()).map(str::to_string)
    });

    PromptContext {
        posture: member.posture.clone(),
        model_class,
        provider_overlay,
    }
}

pub fn default_prompt_file(role_type: RoleType) -> &'static str {
    match role_type {
        RoleType::Architect => "architect.md",
        RoleType::Manager => "manager.md",
        RoleType::Engineer => "engineer.md",
        RoleType::User => "architect.md",
    }
}

pub fn infer_provider_overlay(agent: Option<&str>) -> Option<String> {
    match normalize_value(agent?) {
        value if value.contains("claude") => Some("claude".to_string()),
        value if value.contains("codex") || value.contains("gpt") => Some("codex".to_string()),
        value if value.contains("gemini") => Some("gemini".to_string()),
        _ => None,
    }
}

pub fn infer_model_class(model: Option<&str>, agent: Option<&str>) -> Option<&'static str> {
    let source = model.or(agent)?;
    let value = normalize_value(source);

    if value.starts_with("claude-opus-")
        || value == "gemini-2.5-pro"
        || value.starts_with("gpt-5.5")
    {
        return Some("frontier");
    }
    if value.starts_with("claude-sonnet-")
        || value == "gpt-5.4"
        || value == "gpt-5.3"
        || value == "claude"
        || value == "claude-code"
        || value == "codex"
        || value == "codex-cli"
    {
        return Some("standard");
    }
    if value.starts_with("claude-haiku-")
        || value == "gemini-2.5-flash"
        || value == "gpt-5.2-mini"
        || value == "haiku"
    {
        return Some("fast");
    }

    None
}

fn normalize_value(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn load_posture(name: &str) -> Option<&'static str> {
    match normalize_value(name).as_str() {
        "deep_worker" => Some(POSTURE_DEEP_WORKER),
        "fast_lane" => Some(POSTURE_FAST_LANE),
        "orchestrator" => Some(POSTURE_ORCHESTRATOR),
        _ => None,
    }
}

fn load_model_class(name: &str) -> Option<&'static str> {
    match normalize_value(name).as_str() {
        "frontier" => Some(MODEL_CLASS_FRONTIER),
        "standard" => Some(MODEL_CLASS_STANDARD),
        "fast" => Some(MODEL_CLASS_FAST),
        _ => None,
    }
}

fn load_provider_overlay(name: &str) -> Option<&'static str> {
    match normalize_value(name).as_str() {
        "claude" => Some(PROVIDER_CLAUDE),
        "codex" => Some(PROVIDER_CODEX),
        "gemini" => Some(PROVIDER_GEMINI),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::config::RoleType;
    use crate::team::hierarchy::MemberInstance;

    #[test]
    fn compose_prompt_appends_requested_layers() {
        let prompt = compose_prompt("Base", Some("deep_worker"), Some("standard"), Some("codex"));
        assert!(prompt.starts_with("Base"));
        assert!(prompt.contains("## Posture: Deep Worker"));
        assert!(prompt.contains("## Model Class: Standard"));
        assert!(prompt.contains("## Provider: Codex"));
    }

    #[test]
    fn infer_model_class_from_model_or_agent() {
        assert_eq!(
            infer_model_class(Some("claude-opus-4-1"), None),
            Some("frontier")
        );
        assert_eq!(infer_model_class(Some("gpt-5.5"), None), Some("frontier"));
        assert_eq!(infer_model_class(Some("gpt-5.4"), None), Some("standard"));
        assert_eq!(
            infer_model_class(Some("gemini-2.5-flash"), None),
            Some("fast")
        );
        assert_eq!(infer_model_class(None, Some("codex")), Some("standard"));
    }

    #[test]
    fn resolve_prompt_context_infers_model_class_and_provider_from_member() {
        let member = MemberInstance {
            name: "eng-1-1".to_string(),
            role_name: "engineer".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            model: Some("claude-opus-4-1".to_string()),
            prompt: None,
            posture: Some("deep_worker".to_string()),
            model_class: None,
            provider_overlay: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: true,
        };

        let context = resolve_prompt_context(&member);

        assert_eq!(context.posture.as_deref(), Some("deep_worker"));
        assert_eq!(context.model_class.as_deref(), Some("frontier"));
        assert_eq!(context.provider_overlay.as_deref(), Some("claude"));
    }

    #[test]
    fn render_member_prompt_composes_layers_and_substitutes_variables() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("batty_engineer.md"),
            "Hello {{member_name}} from {{role_name}} -> {{reports_to}}",
        )
        .unwrap();
        let member = MemberInstance {
            name: "eng-1-1".to_string(),
            role_name: "engineer".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("codex".to_string()),
            model: Some("gpt-5.5".to_string()),
            prompt: Some("batty_engineer.md".to_string()),
            posture: Some("deep_worker".to_string()),
            model_class: None,
            provider_overlay: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: true,
        };

        let prompt = render_member_prompt(&member, tmp.path(), &resolve_prompt_context(&member));

        assert!(prompt.contains("Hello eng-1-1 from engineer -> manager"));
        assert!(prompt.contains("## Posture: Deep Worker"));
        assert!(prompt.contains("## Model Class: Frontier"));
        assert!(prompt.contains("## Provider: Codex"));
    }
}
