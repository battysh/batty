//! Helpers for validating multi-provider backend configuration.

/// Returns true when an instance override agent name maps to a supported
/// backend family for mixed-provider teams.
pub(crate) fn is_known_instance_override_backend(agent_name: &str) -> bool {
    matches!(
        agent_name,
        "claude"
            | "claude-code"
            | "codex"
            | "codex-cli"
            | "kiro"
            | "kiro-cli"
            | "gemini"
            | "gemini-cli"
    )
}

#[cfg(test)]
mod tests {
    use super::is_known_instance_override_backend;

    #[test]
    fn accepts_supported_backend_names() {
        for name in [
            "claude",
            "claude-code",
            "codex",
            "codex-cli",
            "kiro",
            "kiro-cli",
            "gemini",
            "gemini-cli",
        ] {
            assert!(is_known_instance_override_backend(name), "{name} should be valid");
        }
    }

    #[test]
    fn rejects_unknown_backend_names() {
        for name in ["", "mystery", "gpt4", "openai"] {
            assert!(
                !is_known_instance_override_backend(name),
                "{name} should be rejected"
            );
        }
    }
}
