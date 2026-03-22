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
}
