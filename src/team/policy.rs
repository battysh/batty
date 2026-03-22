#![cfg_attr(not(test), allow(dead_code))]

use super::config::{RoleType, WorkflowPolicy};

pub fn check_wip_limit(policy: &WorkflowPolicy, role_type: RoleType, active_count: u32) -> bool {
    let limit = match role_type {
        RoleType::Engineer => policy.wip_limit_per_engineer,
        RoleType::Architect | RoleType::Manager => policy.wip_limit_per_reviewer,
        RoleType::User => None,
    };

    match limit {
        Some(limit) => active_count < limit,
        None => true,
    }
}

pub fn is_review_nudge_due(policy: &WorkflowPolicy, review_age_secs: u64) -> bool {
    review_age_secs >= policy.review_nudge_threshold_secs
}

pub fn is_review_stale(policy: &WorkflowPolicy, review_age_secs: u64) -> bool {
    review_age_secs >= policy.review_timeout_secs
}

pub fn should_escalate(policy: &WorkflowPolicy, blocked_age_secs: u64) -> bool {
    blocked_age_secs >= policy.escalation_threshold_secs
}

/// Returns the effective nudge threshold for a task with the given priority.
/// Uses per-priority override if configured, otherwise falls back to global.
pub fn effective_nudge_threshold(policy: &WorkflowPolicy, priority: &str) -> u64 {
    if !priority.is_empty() {
        if let Some(ovr) = policy.review_timeout_overrides.get(priority) {
            if let Some(secs) = ovr.review_nudge_threshold_secs {
                return secs;
            }
        }
    }
    policy.review_nudge_threshold_secs
}

/// Returns the effective escalation (review timeout) threshold for a task with
/// the given priority. Uses per-priority override if configured, otherwise
/// falls back to global.
pub fn effective_escalation_threshold(policy: &WorkflowPolicy, priority: &str) -> u64 {
    if !priority.is_empty() {
        if let Some(ovr) = policy.review_timeout_overrides.get(priority) {
            if let Some(secs) = ovr.review_timeout_secs {
                return secs;
            }
        }
    }
    policy.review_timeout_secs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_workflow_policy_has_sensible_defaults() {
        let policy = WorkflowPolicy::default();
        assert_eq!(policy.wip_limit_per_engineer, None);
        assert_eq!(policy.wip_limit_per_reviewer, None);
        assert_eq!(policy.pipeline_starvation_threshold, Some(1));
        assert_eq!(policy.escalation_threshold_secs, 3600);
        assert_eq!(policy.review_nudge_threshold_secs, 1800);
        assert_eq!(policy.review_timeout_secs, 7200);
        assert_eq!(policy.auto_archive_done_after_secs, None);
        assert!(policy.capability_overrides.is_empty());
    }

    #[test]
    fn check_wip_limit_enforces_limits() {
        let policy = WorkflowPolicy {
            wip_limit_per_engineer: Some(2),
            wip_limit_per_reviewer: Some(1),
            ..WorkflowPolicy::default()
        };

        assert!(check_wip_limit(&policy, RoleType::Engineer, 0));
        assert!(check_wip_limit(&policy, RoleType::Engineer, 1));
        assert!(!check_wip_limit(&policy, RoleType::Engineer, 2));
        assert!(check_wip_limit(&policy, RoleType::Manager, 0));
        assert!(!check_wip_limit(&policy, RoleType::Manager, 1));
        assert!(check_wip_limit(&policy, RoleType::User, 99));
    }

    #[test]
    fn stale_and_escalation_threshold_checks_are_inclusive() {
        let policy = WorkflowPolicy {
            escalation_threshold_secs: 120,
            review_timeout_secs: 300,
            ..WorkflowPolicy::default()
        };

        assert!(!should_escalate(&policy, 119));
        assert!(should_escalate(&policy, 120));
        assert!(!is_review_stale(&policy, 299));
        assert!(is_review_stale(&policy, 300));
    }

    #[test]
    fn review_nudge_threshold_check_is_inclusive() {
        let policy = WorkflowPolicy {
            review_nudge_threshold_secs: 1800,
            ..WorkflowPolicy::default()
        };

        assert!(!is_review_nudge_due(&policy, 1799));
        assert!(is_review_nudge_due(&policy, 1800));
        assert!(is_review_nudge_due(&policy, 1801));
    }

    #[test]
    fn effective_nudge_threshold_uses_global_when_no_override() {
        let policy = WorkflowPolicy {
            review_nudge_threshold_secs: 1800,
            ..WorkflowPolicy::default()
        };
        assert_eq!(effective_nudge_threshold(&policy, "high"), 1800);
        assert_eq!(effective_nudge_threshold(&policy, ""), 1800);
    }

    #[test]
    fn effective_nudge_threshold_uses_priority_override() {
        use super::super::config::ReviewTimeoutOverride;
        let mut overrides = std::collections::HashMap::new();
        overrides.insert(
            "critical".to_string(),
            ReviewTimeoutOverride {
                review_nudge_threshold_secs: Some(300),
                review_timeout_secs: Some(600),
            },
        );
        let policy = WorkflowPolicy {
            review_nudge_threshold_secs: 1800,
            review_timeout_overrides: overrides,
            ..WorkflowPolicy::default()
        };
        // Critical uses override
        assert_eq!(effective_nudge_threshold(&policy, "critical"), 300);
        // High falls back to global
        assert_eq!(effective_nudge_threshold(&policy, "high"), 1800);
    }

    #[test]
    fn effective_escalation_threshold_uses_priority_override() {
        use super::super::config::ReviewTimeoutOverride;
        let mut overrides = std::collections::HashMap::new();
        overrides.insert(
            "critical".to_string(),
            ReviewTimeoutOverride {
                review_nudge_threshold_secs: Some(300),
                review_timeout_secs: Some(600),
            },
        );
        overrides.insert(
            "high".to_string(),
            ReviewTimeoutOverride {
                review_nudge_threshold_secs: None,
                review_timeout_secs: Some(3600),
            },
        );
        let policy = WorkflowPolicy {
            review_timeout_secs: 7200,
            review_timeout_overrides: overrides,
            ..WorkflowPolicy::default()
        };
        // Critical uses override
        assert_eq!(effective_escalation_threshold(&policy, "critical"), 600);
        // High uses override
        assert_eq!(effective_escalation_threshold(&policy, "high"), 3600);
        // Medium falls back to global
        assert_eq!(effective_escalation_threshold(&policy, "medium"), 7200);
    }

    #[test]
    fn partial_override_falls_back_per_field() {
        use super::super::config::ReviewTimeoutOverride;
        let mut overrides = std::collections::HashMap::new();
        overrides.insert(
            "high".to_string(),
            ReviewTimeoutOverride {
                review_nudge_threshold_secs: Some(900),
                review_timeout_secs: None, // no escalation override
            },
        );
        let policy = WorkflowPolicy {
            review_nudge_threshold_secs: 1800,
            review_timeout_secs: 7200,
            review_timeout_overrides: overrides,
            ..WorkflowPolicy::default()
        };
        // Nudge uses override
        assert_eq!(effective_nudge_threshold(&policy, "high"), 900);
        // Escalation falls back to global (no override for this field)
        assert_eq!(effective_escalation_threshold(&policy, "high"), 7200);
    }
}
