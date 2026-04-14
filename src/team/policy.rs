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
        assert!(policy.main_smoke.enabled);
        assert_eq!(policy.main_smoke.interval_secs, 600);
        assert_eq!(policy.main_smoke.command, "cargo check");
        assert!(policy.main_smoke.pause_dispatch_on_failure);
        assert!(!policy.main_smoke.auto_revert);
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

    // --- New tests for task #261 ---

    #[test]
    fn wip_limit_none_means_unlimited() {
        let policy = WorkflowPolicy {
            wip_limit_per_engineer: None,
            wip_limit_per_reviewer: None,
            ..WorkflowPolicy::default()
        };

        // Even very large counts should be allowed
        assert!(check_wip_limit(&policy, RoleType::Engineer, 100));
        assert!(check_wip_limit(&policy, RoleType::Engineer, u32::MAX));
        assert!(check_wip_limit(&policy, RoleType::Manager, 100));
        assert!(check_wip_limit(&policy, RoleType::Architect, u32::MAX));
    }

    #[test]
    fn wip_limit_zero_blocks_all_work() {
        let policy = WorkflowPolicy {
            wip_limit_per_engineer: Some(0),
            wip_limit_per_reviewer: Some(0),
            ..WorkflowPolicy::default()
        };

        assert!(!check_wip_limit(&policy, RoleType::Engineer, 0));
        assert!(!check_wip_limit(&policy, RoleType::Manager, 0));
        assert!(!check_wip_limit(&policy, RoleType::Architect, 0));
    }

    #[test]
    fn architect_uses_reviewer_wip_limit() {
        let policy = WorkflowPolicy {
            wip_limit_per_engineer: Some(5),
            wip_limit_per_reviewer: Some(2),
            ..WorkflowPolicy::default()
        };

        // Architect should use reviewer limit (2), not engineer limit (5)
        assert!(check_wip_limit(&policy, RoleType::Architect, 1));
        assert!(!check_wip_limit(&policy, RoleType::Architect, 2));
        assert!(!check_wip_limit(&policy, RoleType::Architect, 3));
    }

    #[test]
    fn user_role_always_passes_wip_check() {
        let policy = WorkflowPolicy {
            wip_limit_per_engineer: Some(1),
            wip_limit_per_reviewer: Some(1),
            ..WorkflowPolicy::default()
        };

        assert!(check_wip_limit(&policy, RoleType::User, 0));
        assert!(check_wip_limit(&policy, RoleType::User, 100));
        assert!(check_wip_limit(&policy, RoleType::User, u32::MAX));
    }

    #[test]
    fn escalation_boundary_values() {
        let policy = WorkflowPolicy {
            escalation_threshold_secs: 0,
            ..WorkflowPolicy::default()
        };

        // Zero threshold means always escalate
        assert!(should_escalate(&policy, 0));
        assert!(should_escalate(&policy, 1));
    }

    #[test]
    fn review_nudge_at_zero_threshold() {
        let policy = WorkflowPolicy {
            review_nudge_threshold_secs: 0,
            ..WorkflowPolicy::default()
        };

        assert!(is_review_nudge_due(&policy, 0));
        assert!(is_review_nudge_due(&policy, 1));
    }

    #[test]
    fn review_stale_at_zero_threshold() {
        let policy = WorkflowPolicy {
            review_timeout_secs: 0,
            ..WorkflowPolicy::default()
        };

        assert!(is_review_stale(&policy, 0));
        assert!(is_review_stale(&policy, 1));
    }

    #[test]
    fn review_nudge_well_before_threshold() {
        let policy = WorkflowPolicy {
            review_nudge_threshold_secs: 10000,
            ..WorkflowPolicy::default()
        };

        assert!(!is_review_nudge_due(&policy, 0));
        assert!(!is_review_nudge_due(&policy, 5000));
        assert!(!is_review_nudge_due(&policy, 9999));
    }

    #[test]
    fn override_with_both_fields_none_falls_back_to_global() {
        use super::super::config::ReviewTimeoutOverride;
        let mut overrides = std::collections::HashMap::new();
        overrides.insert(
            "low".to_string(),
            ReviewTimeoutOverride {
                review_nudge_threshold_secs: None,
                review_timeout_secs: None,
            },
        );
        let policy = WorkflowPolicy {
            review_nudge_threshold_secs: 1800,
            review_timeout_secs: 7200,
            review_timeout_overrides: overrides,
            ..WorkflowPolicy::default()
        };

        // Both should fall back to global
        assert_eq!(effective_nudge_threshold(&policy, "low"), 1800);
        assert_eq!(effective_escalation_threshold(&policy, "low"), 7200);
    }

    #[test]
    fn multiple_priority_overrides_are_independent() {
        use super::super::config::ReviewTimeoutOverride;
        let mut overrides = std::collections::HashMap::new();
        overrides.insert(
            "critical".to_string(),
            ReviewTimeoutOverride {
                review_nudge_threshold_secs: Some(60),
                review_timeout_secs: Some(120),
            },
        );
        overrides.insert(
            "high".to_string(),
            ReviewTimeoutOverride {
                review_nudge_threshold_secs: Some(300),
                review_timeout_secs: Some(600),
            },
        );
        overrides.insert(
            "low".to_string(),
            ReviewTimeoutOverride {
                review_nudge_threshold_secs: Some(3600),
                review_timeout_secs: Some(14400),
            },
        );
        let policy = WorkflowPolicy {
            review_nudge_threshold_secs: 1800,
            review_timeout_secs: 7200,
            review_timeout_overrides: overrides,
            ..WorkflowPolicy::default()
        };

        assert_eq!(effective_nudge_threshold(&policy, "critical"), 60);
        assert_eq!(effective_escalation_threshold(&policy, "critical"), 120);
        assert_eq!(effective_nudge_threshold(&policy, "high"), 300);
        assert_eq!(effective_escalation_threshold(&policy, "high"), 600);
        assert_eq!(effective_nudge_threshold(&policy, "low"), 3600);
        assert_eq!(effective_escalation_threshold(&policy, "low"), 14400);
        // Unknown priority falls back to global
        assert_eq!(effective_nudge_threshold(&policy, "medium"), 1800);
        assert_eq!(effective_escalation_threshold(&policy, "medium"), 7200);
    }

    #[test]
    fn override_with_only_escalation_set() {
        use super::super::config::ReviewTimeoutOverride;
        let mut overrides = std::collections::HashMap::new();
        overrides.insert(
            "urgent".to_string(),
            ReviewTimeoutOverride {
                review_nudge_threshold_secs: None,
                review_timeout_secs: Some(300),
            },
        );
        let policy = WorkflowPolicy {
            review_nudge_threshold_secs: 1800,
            review_timeout_secs: 7200,
            review_timeout_overrides: overrides,
            ..WorkflowPolicy::default()
        };

        // Nudge falls back to global, escalation uses override
        assert_eq!(effective_nudge_threshold(&policy, "urgent"), 1800);
        assert_eq!(effective_escalation_threshold(&policy, "urgent"), 300);
    }

    #[test]
    fn wip_limit_at_exact_boundary() {
        let policy = WorkflowPolicy {
            wip_limit_per_engineer: Some(3),
            ..WorkflowPolicy::default()
        };

        assert!(check_wip_limit(&policy, RoleType::Engineer, 2)); // under limit
        assert!(!check_wip_limit(&policy, RoleType::Engineer, 3)); // at limit
        assert!(!check_wip_limit(&policy, RoleType::Engineer, 4)); // over limit
    }

    #[test]
    fn default_policy_stall_and_health_fields() {
        let policy = WorkflowPolicy::default();
        assert_eq!(policy.stall_threshold_secs, 300);
        assert_eq!(policy.max_stall_restarts, 2);
        assert_eq!(policy.health_check_interval_secs, 60);
        assert_eq!(policy.uncommitted_warn_threshold, 200);
    }

    #[test]
    fn escalation_with_large_values() {
        let policy = WorkflowPolicy {
            escalation_threshold_secs: u64::MAX,
            ..WorkflowPolicy::default()
        };

        // Should never escalate with MAX threshold (unless age is also MAX)
        assert!(!should_escalate(&policy, 0));
        assert!(!should_escalate(&policy, u64::MAX - 1));
        assert!(should_escalate(&policy, u64::MAX));
    }
}
