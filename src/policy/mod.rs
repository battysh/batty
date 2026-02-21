use std::collections::HashMap;

use crate::config::Policy;

/// What the policy engine decides to do with a detected prompt.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Log only — don't respond. (observe mode)
    Observe { prompt: String },
    /// Show a suggestion and wait for user confirmation. (suggest mode)
    Suggest { prompt: String, response: String },
    /// Auto-respond with the matched answer. (act mode)
    Act { prompt: String, response: String },
    /// No matching auto-answer rule — escalate to user regardless of mode.
    Escalate { prompt: String },
}

/// Simple policy engine that evaluates PTY output against auto-answer rules.
pub struct PolicyEngine {
    policy: Policy,
    auto_answers: HashMap<String, String>,
}

impl PolicyEngine {
    pub fn new(policy: Policy, auto_answers: HashMap<String, String>) -> Self {
        Self {
            policy,
            auto_answers,
        }
    }

    /// Evaluate a detected prompt against the policy and auto-answer rules.
    ///
    /// The `prompt` is the text detected in the PTY output (e.g., "Continue? [y/n]").
    pub fn evaluate(&self, prompt: &str) -> Decision {
        // Try to find a matching auto-answer rule (substring match)
        let matched = self
            .auto_answers
            .iter()
            .find(|(pattern, _)| prompt.contains(pattern.as_str()));

        match self.policy {
            Policy::Observe => Decision::Observe {
                prompt: prompt.to_string(),
            },
            Policy::Suggest => match matched {
                Some((_, response)) => Decision::Suggest {
                    prompt: prompt.to_string(),
                    response: response.clone(),
                },
                None => Decision::Escalate {
                    prompt: prompt.to_string(),
                },
            },
            Policy::Act => match matched {
                Some((_, response)) => Decision::Act {
                    prompt: prompt.to_string(),
                    response: response.clone(),
                },
                None => Decision::Escalate {
                    prompt: prompt.to_string(),
                },
            },
        }
    }

    pub fn policy(&self) -> Policy {
        self.policy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_auto_answers() -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("Continue? [y/n]".to_string(), "y".to_string());
        m.insert("Allow tool".to_string(), "y".to_string());
        m
    }

    #[test]
    fn observe_always_returns_observe() {
        let engine = PolicyEngine::new(Policy::Observe, test_auto_answers());

        // Even with a matching pattern, observe mode just logs
        let decision = engine.evaluate("Continue? [y/n]");
        assert_eq!(
            decision,
            Decision::Observe {
                prompt: "Continue? [y/n]".to_string()
            }
        );
    }

    #[test]
    fn observe_with_unknown_prompt() {
        let engine = PolicyEngine::new(Policy::Observe, test_auto_answers());
        let decision = engine.evaluate("What model should I use?");
        assert_eq!(
            decision,
            Decision::Observe {
                prompt: "What model should I use?".to_string()
            }
        );
    }

    #[test]
    fn suggest_with_matching_pattern() {
        let engine = PolicyEngine::new(Policy::Suggest, test_auto_answers());
        let decision = engine.evaluate("Continue? [y/n]");
        assert_eq!(
            decision,
            Decision::Suggest {
                prompt: "Continue? [y/n]".to_string(),
                response: "y".to_string()
            }
        );
    }

    #[test]
    fn suggest_escalates_unknown_prompt() {
        let engine = PolicyEngine::new(Policy::Suggest, test_auto_answers());
        let decision = engine.evaluate("What database should I use?");
        assert_eq!(
            decision,
            Decision::Escalate {
                prompt: "What database should I use?".to_string()
            }
        );
    }

    #[test]
    fn act_auto_responds_to_matching_pattern() {
        let engine = PolicyEngine::new(Policy::Act, test_auto_answers());
        let decision = engine.evaluate("Continue? [y/n]");
        assert_eq!(
            decision,
            Decision::Act {
                prompt: "Continue? [y/n]".to_string(),
                response: "y".to_string()
            }
        );
    }

    #[test]
    fn act_escalates_unknown_prompt() {
        let engine = PolicyEngine::new(Policy::Act, test_auto_answers());
        let decision = engine.evaluate("Should I refactor the auth module?");
        assert_eq!(
            decision,
            Decision::Escalate {
                prompt: "Should I refactor the auth module?".to_string()
            }
        );
    }

    #[test]
    fn act_with_substring_match() {
        let engine = PolicyEngine::new(Policy::Act, test_auto_answers());
        // "Allow tool" is a substring of the full prompt
        let decision = engine.evaluate("Allow tool Read on /home/user/file.rs? [y/n]");
        assert_eq!(
            decision,
            Decision::Act {
                prompt: "Allow tool Read on /home/user/file.rs? [y/n]".to_string(),
                response: "y".to_string()
            }
        );
    }

    #[test]
    fn empty_auto_answers_always_escalates_in_act_mode() {
        let engine = PolicyEngine::new(Policy::Act, HashMap::new());
        let decision = engine.evaluate("Continue? [y/n]");
        assert_eq!(
            decision,
            Decision::Escalate {
                prompt: "Continue? [y/n]".to_string()
            }
        );
    }

    #[test]
    fn policy_getter() {
        let engine = PolicyEngine::new(Policy::Act, HashMap::new());
        assert_eq!(engine.policy(), Policy::Act);
    }
}
