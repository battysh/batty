#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SupervisoryPressure {
    ReviewNudge,
    ReviewBacklog,
    DispatchRecovery,
    UtilizationRecovery,
    TriageBacklog,
    IdleNudge,
    RecoveryUpdate,
    StatusUpdate,
}

pub(crate) fn normalized_body(body: &str) -> String {
    body.trim().to_ascii_lowercase()
}

pub(crate) fn classify_supervisory_pressure_normalized(body: &str) -> Option<SupervisoryPressure> {
    if is_review_nudge_normalized(body) {
        Some(SupervisoryPressure::ReviewNudge)
    } else if body.starts_with("review backlog detected:") {
        Some(SupervisoryPressure::ReviewBacklog)
    } else if body.starts_with("dispatch recovery needed:") {
        Some(SupervisoryPressure::DispatchRecovery)
    } else if body.contains("utilization recovery")
        || body.starts_with("utilization gap detected:")
        || body.starts_with("architect utilization")
    {
        Some(SupervisoryPressure::UtilizationRecovery)
    } else if body.starts_with("triage backlog detected:") {
        Some(SupervisoryPressure::TriageBacklog)
    } else if is_idle_nudge_normalized(body) {
        Some(SupervisoryPressure::IdleNudge)
    } else if body.starts_with("recovery:")
        || body.contains("lane blocked")
        || body.contains("stuck-task escalation")
    {
        Some(SupervisoryPressure::RecoveryUpdate)
    } else if is_status_update_normalized(body) {
        Some(SupervisoryPressure::StatusUpdate)
    } else {
        None
    }
}

pub(crate) fn is_idle_nudge(body: &str) -> bool {
    is_idle_nudge_normalized(&normalized_body(body))
}

pub(crate) fn is_idle_nudge_normalized(body: &str) -> bool {
    body.contains("idle nudge:")
        || body.contains("if you are idle, take action now")
        || body.contains("you have been idle past your configured timeout")
}

pub(crate) fn is_review_nudge(body: &str) -> bool {
    is_review_nudge_normalized(&normalized_body(body))
}

pub(crate) fn is_review_nudge_normalized(body: &str) -> bool {
    body.starts_with("review nudge:")
}

pub(crate) fn is_status_update_normalized(body: &str) -> bool {
    body.starts_with("rollup:") || body.contains("status update")
}

pub(crate) fn extract_task_id(body: &str) -> Option<String> {
    let lower = body.to_ascii_lowercase();

    if let Some(pos) = lower.find("task_id") {
        let after = &body[pos + 7..];
        let digits: String = after
            .chars()
            .skip_while(|ch| !ch.is_ascii_digit())
            .take_while(|ch| ch.is_ascii_digit())
            .collect();
        if !digits.is_empty() {
            return Some(digits);
        }
    }

    if let Some(pos) = body.find('#') {
        let digits: String = body[pos + 1..]
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect();
        if !digits.is_empty() {
            return Some(digits);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_review_backlog_pressure() {
        assert_eq!(
            classify_supervisory_pressure_normalized(&normalized_body(
                "Review backlog detected: direct-report work is waiting for your review."
            )),
            Some(SupervisoryPressure::ReviewBacklog)
        );
    }

    #[test]
    fn classify_idle_nudge_pressure_from_instructional_text() {
        assert_eq!(
            classify_supervisory_pressure_normalized(&normalized_body(
                "If you are idle, take action NOW"
            )),
            Some(SupervisoryPressure::IdleNudge)
        );
    }

    #[test]
    fn classify_status_update_pressure() {
        assert_eq!(
            classify_supervisory_pressure_normalized(&normalized_body(
                "Status update: triage queue is unchanged."
            )),
            Some(SupervisoryPressure::StatusUpdate)
        );
    }

    #[test]
    fn extract_task_id_prefers_task_id_field() {
        assert_eq!(
            extract_task_id(r#"{"task_id": 99, "body": "Task #42"}"#),
            Some("99".to_string())
        );
    }

    #[test]
    fn extract_task_id_falls_back_to_hash_reference() {
        assert_eq!(extract_task_id("Task #42 is done"), Some("42".to_string()));
    }
}
