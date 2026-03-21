//! Rolling failure-signature detection over recent team events.

use std::collections::{HashMap, VecDeque};

use super::events::TeamEvent;

const DEFAULT_WINDOW_SIZE: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatternType {
    RepeatedTestFailure,
    EscalationCluster,
    MergeConflictRecurrence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternMatch {
    pub pattern_type: PatternType,
    pub frequency: u32,
    pub affected_entities: Vec<String>,
    pub description: String,
}

pub struct FailureWindow {
    events: VecDeque<FailureEvent>,
    window_size: usize,
}

#[derive(Debug, Clone)]
struct FailureEvent {
    pub event_type: String,
    pub role: Option<String>,
    pub task: Option<String>,
    pub error: Option<String>,
    pub ts: u64,
}

impl FailureWindow {
    pub fn new(window_size: usize) -> Self {
        Self {
            events: VecDeque::new(),
            window_size: if window_size == 0 {
                DEFAULT_WINDOW_SIZE
            } else {
                window_size
            },
        }
    }

    pub fn push(&mut self, event: &TeamEvent) {
        if !is_failure_relevant(event) {
            return;
        }

        self.events.push_back(FailureEvent {
            event_type: event.event.clone(),
            role: event.role.clone(),
            task: event.task.clone(),
            error: event.error.clone(),
            ts: event.ts,
        });

        while self.events.len() > self.window_size {
            self.events.pop_front();
        }
    }

    pub fn detect_failure_patterns(&self) -> Vec<PatternMatch> {
        let mut patterns = Vec::new();

        let mut error_counts: HashMap<String, u32> = HashMap::new();
        for event in &self.events {
            if event.error.is_some() {
                if let Some(role) = event.role.as_deref() {
                    *error_counts.entry(role.to_string()).or_insert(0) += 1;
                }
            }
        }
        for (role, frequency) in error_counts {
            if frequency >= 2 {
                patterns.push(PatternMatch {
                    pattern_type: PatternType::RepeatedTestFailure,
                    frequency,
                    affected_entities: vec![role.clone()],
                    description: format!(
                        "{role} hit {frequency} error events in the current failure window"
                    ),
                });
            }
        }

        let escalation_events: Vec<&FailureEvent> = self
            .events
            .iter()
            .filter(|event| event.event_type == "task_escalated")
            .collect();
        if escalation_events.len() >= 2 {
            let mut affected_entities: Vec<String> = escalation_events
                .iter()
                .filter_map(|event| event.task.clone())
                .collect();
            affected_entities.sort();
            affected_entities.dedup();
            patterns.push(PatternMatch {
                pattern_type: PatternType::EscalationCluster,
                frequency: escalation_events.len() as u32,
                affected_entities,
                description: format!(
                    "{} escalation events detected in the current failure window",
                    escalation_events.len()
                ),
            });
        }

        let conflict_events: Vec<&FailureEvent> = self
            .events
            .iter()
            .filter(|event| {
                contains_conflict(&event.event_type)
                    || event.error.as_deref().is_some_and(contains_conflict)
            })
            .collect();
        if conflict_events.len() >= 2 {
            let mut affected_entities: Vec<String> = conflict_events
                .iter()
                .filter_map(|event| {
                    event
                        .task
                        .clone()
                        .or_else(|| event.role.clone())
                        .or_else(|| Some(event.ts.to_string()))
                })
                .collect();
            affected_entities.sort();
            affected_entities.dedup();
            patterns.push(PatternMatch {
                pattern_type: PatternType::MergeConflictRecurrence,
                frequency: conflict_events.len() as u32,
                affected_entities,
                description: format!(
                    "{} conflict-related events detected in the current failure window",
                    conflict_events.len()
                ),
            });
        }

        patterns.sort_by(|left, right| {
            right
                .frequency
                .cmp(&left.frequency)
                .then_with(|| {
                    pattern_sort_key(&left.pattern_type).cmp(&pattern_sort_key(&right.pattern_type))
                })
                .then_with(|| left.description.cmp(&right.description))
        });
        patterns
    }
}

fn pattern_sort_key(pattern_type: &PatternType) -> u8 {
    match pattern_type {
        PatternType::RepeatedTestFailure => 0,
        PatternType::EscalationCluster => 1,
        PatternType::MergeConflictRecurrence => 2,
    }
}

fn is_failure_relevant(event: &TeamEvent) -> bool {
    event.event == "task_escalated"
        || event.error.is_some()
        || contains_failure_keyword(&event.event)
}

fn contains_failure_keyword(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    value.contains("fail") || value.contains("conflict")
}

fn contains_conflict(value: &str) -> bool {
    value.to_ascii_lowercase().contains("conflict")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error_event(event: &str, role: &str, error: &str, ts: u64) -> TeamEvent {
        TeamEvent {
            event: event.to_string(),
            role: Some(role.to_string()),
            task: None,
            recipient: None,
            from: None,
            to: None,
            restart: None,
            load: None,
            working_members: None,
            total_members: None,
            session_running: None,
            reason: None,
            step: None,
            error: Some(error.to_string()),
            uptime_secs: None,
            ts,
        }
    }

    fn conflict_event(
        event: &str,
        role: &str,
        task: &str,
        error: Option<&str>,
        ts: u64,
    ) -> TeamEvent {
        TeamEvent {
            event: event.to_string(),
            role: Some(role.to_string()),
            task: Some(task.to_string()),
            recipient: None,
            from: None,
            to: None,
            restart: None,
            load: None,
            working_members: None,
            total_members: None,
            session_running: None,
            reason: None,
            step: None,
            error: error.map(str::to_string),
            uptime_secs: None,
            ts,
        }
    }

    #[test]
    fn test_detect_repeated_test_failures() {
        let mut window = FailureWindow::new(20);
        window.push(&error_event("test_failure", "eng-1", "tests failed", 1));
        window.push(&error_event("test_failure", "eng-1", "tests failed", 2));
        window.push(&error_event("test_failure", "eng-1", "tests failed", 3));

        let patterns = window.detect_failure_patterns();

        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0].pattern_type, PatternType::RepeatedTestFailure);
        assert_eq!(patterns[0].frequency, 3);
        assert_eq!(patterns[0].affected_entities, vec!["eng-1".to_string()]);
    }

    #[test]
    fn test_detect_escalation_cluster() {
        let mut window = FailureWindow::new(20);
        let mut event1 = TeamEvent::task_escalated("eng-1", "101");
        event1.ts = 1;
        let mut event2 = TeamEvent::task_escalated("eng-2", "102");
        event2.ts = 2;
        window.push(&event1);
        window.push(&event2);

        let patterns = window.detect_failure_patterns();

        assert_eq!(patterns.len(), 1);
        assert_eq!(patterns[0].pattern_type, PatternType::EscalationCluster);
        assert_eq!(patterns[0].frequency, 2);
        assert_eq!(
            patterns[0].affected_entities,
            vec!["101".to_string(), "102".to_string()]
        );
    }

    #[test]
    fn test_detect_merge_conflict_recurrence() {
        let mut window = FailureWindow::new(20);
        window.push(&conflict_event("merge_conflict", "eng-1", "201", None, 1));
        window.push(&conflict_event(
            "loop_step_error",
            "eng-1",
            "202",
            Some("rebase conflict on main"),
            2,
        ));

        let patterns = window.detect_failure_patterns();

        assert_eq!(patterns.len(), 1);
        assert_eq!(
            patterns[0].pattern_type,
            PatternType::MergeConflictRecurrence
        );
        assert_eq!(patterns[0].frequency, 2);
        assert_eq!(
            patterns[0].affected_entities,
            vec!["201".to_string(), "202".to_string()]
        );
    }

    #[test]
    fn test_window_rollover() {
        let mut window = FailureWindow::new(5);
        for index in 0..10 {
            window.push(&conflict_event(
                "merge_conflict",
                "eng-1",
                &format!("task-{index}"),
                None,
                index,
            ));
        }

        assert_eq!(window.events.len(), 5);
        assert_eq!(
            window
                .events
                .front()
                .and_then(|event| event.task.as_deref()),
            Some("task-5")
        );
        assert_eq!(
            window.events.back().and_then(|event| event.task.as_deref()),
            Some("task-9")
        );
    }

    #[test]
    fn test_no_patterns_when_below_threshold() {
        let mut window = FailureWindow::new(20);
        window.push(&error_event("test_failure", "eng-1", "tests failed", 1));
        let mut escalation = TeamEvent::task_escalated("eng-1", "101");
        escalation.ts = 2;
        window.push(&escalation);
        window.push(&conflict_event("merge_conflict", "eng-1", "201", None, 3));

        assert!(window.detect_failure_patterns().is_empty());
    }

    #[test]
    fn test_non_failure_events_ignored() {
        let mut window = FailureWindow::new(20);
        let mut routed = TeamEvent::message_routed("manager", "eng-1");
        routed.ts = 2;
        window.push(&TeamEvent::daemon_started());
        window.push(&routed);

        assert!(window.events.is_empty());
        assert!(window.detect_failure_patterns().is_empty());
    }
}
