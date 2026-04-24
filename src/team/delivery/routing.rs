use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{debug, info, warn};
use uuid::Uuid;

use super::{MessageDelivery, PendingMessage};
use crate::team::append_shim_event_log;
use crate::team::completion;
use crate::team::config::RoleType;
use crate::team::daemon::{PendingAckInboxMessage, PendingShimAck, TeamDaemon};
use crate::team::errors::DeliveryError;
use crate::team::events::TeamEvent;
use crate::team::inbox;
use crate::team::message;
use crate::team::standup::MemberState;
use crate::team::status;
use crate::team::supervisory_notice::{
    SupervisoryPressure, SupervisoryPressureSnapshot,
    actionable_supervisory_pressure_count_from_bodies, classify_supervisory_pressure_normalized,
    extract_task_id, is_idle_nudge, is_review_nudge, is_status_update_normalized, normalized_body,
};

const MANAGER_DIGEST_BACKPRESSURE_COOLDOWN_SECS: u64 = 30;

/// Extract a task ID from assignment body text like "Task #42: ..." or "Task #42 ...".
fn extract_task_id_from_body(body: &str) -> Option<u32> {
    let body = body.trim();
    // Match "Task #N" at the start of the body
    if let Some(rest) = body.strip_prefix("Task #") {
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        return digits.parse().ok();
    }
    // Match "TASK #N" (case-insensitive)
    let lower = body.to_lowercase();
    if let Some(rest) = lower.strip_prefix("task #") {
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        return digits.parse().ok();
    }
    None
}

fn shim_log_preview(body: &str) -> String {
    let single_line = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut preview = single_line.chars().take(160).collect::<String>();
    if single_line.chars().count() > 160 {
        preview.push_str("...");
    }
    preview
}

fn format_batched_message(messages: &[inbox::InboxMessage]) -> String {
    messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            format!(
                "--- Message {}/{} from {} ---\n{}",
                index + 1,
                messages.len(),
                message.from,
                message.body
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OrchestratorOnlyReason {
    Nudge,
    StatusQuery,
    StandupRequest,
    EngineerChatter,
}

impl OrchestratorOnlyReason {
    fn label(self) -> &'static str {
        match self {
            Self::Nudge => "nudge",
            Self::StatusQuery => "status query",
            Self::StandupRequest => "standup request",
            Self::EngineerChatter => "engineer chatter",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ManagerNoticeClass {
    Completion,
    Triage,
    Review,
    Immediate,
    Escalation,
    Dispatch,
    Utilization,
    Recovery,
    Status,
}

impl ManagerNoticeClass {
    fn label(self) -> &'static str {
        match self {
            Self::Completion => "completion",
            Self::Escalation => "escalation",
            Self::Immediate => "immediate",
            Self::Triage => "triage",
            Self::Review => "review",
            Self::Dispatch => "dispatch",
            Self::Utilization => "utilization",
            Self::Recovery => "recovery",
            Self::Status => "status",
        }
    }

    fn priority(self) -> u8 {
        match self {
            Self::Completion => 0,
            Self::Review => 1,
            Self::Triage => 2,
            Self::Utilization => 3,
            Self::Dispatch => 4,
            Self::Immediate => 5,
            Self::Escalation => 6,
            Self::Recovery => 7,
            Self::Status => 8,
        }
    }
}

#[derive(Debug, Clone)]
struct SupervisoryDigestEntry {
    class: ManagerNoticeClass,
    from: String,
    preview: String,
    duplicate_count: usize,
    first_seen: usize,
}

#[derive(Debug, Clone)]
struct SupervisoryDigest {
    entries: Vec<SupervisoryDigestEntry>,
    total_messages: usize,
    duplicates_suppressed: usize,
    pressure: SupervisoryPressureSnapshot,
}

fn is_status_query(body: &str) -> bool {
    let body = normalized_body(body);
    body == "status"
        || body == "status?"
        || (body.starts_with("status ") && !body.starts_with("status update"))
        || body.contains("what's the status")
        || body.contains("what is the status")
        || body.contains("current status")
        || body.contains("progress update?")
        || body.contains("screen state")
}

fn is_standup_request(body: &str) -> bool {
    let body = normalized_body(body);
    body == "standup"
        || body == "standup?"
        || body.starts_with("standup ")
        || body.contains("standup request")
}

fn classify_manager_notice(body: &str) -> ManagerNoticeClass {
    let body = normalized_body(body);

    if is_manager_completion_notice(&body) {
        ManagerNoticeClass::Completion
    } else if is_manager_escalation_notice(&body) {
        ManagerNoticeClass::Escalation
    } else if let Some(pressure) = classify_supervisory_pressure_normalized(&body) {
        match pressure {
            SupervisoryPressure::ReviewNudge | SupervisoryPressure::ReviewBacklog => {
                ManagerNoticeClass::Review
            }
            SupervisoryPressure::TriageBacklog => ManagerNoticeClass::Triage,
            SupervisoryPressure::IdleActiveRecovery => ManagerNoticeClass::Utilization,
            SupervisoryPressure::DispatchGap => ManagerNoticeClass::Dispatch,
            SupervisoryPressure::PlanningBacklog => ManagerNoticeClass::Dispatch,
            SupervisoryPressure::IdleNudge
            | SupervisoryPressure::RecoveryUpdate
            | SupervisoryPressure::ResolvedUpdate => ManagerNoticeClass::Recovery,
            SupervisoryPressure::StatusUpdate => ManagerNoticeClass::Status,
        }
    } else {
        ManagerNoticeClass::Immediate
    }
}

fn should_batch_manager_notice(class: ManagerNoticeClass) -> bool {
    matches!(
        class,
        ManagerNoticeClass::Escalation
            | ManagerNoticeClass::Immediate
            | ManagerNoticeClass::Dispatch
            | ManagerNoticeClass::Utilization
            | ManagerNoticeClass::Triage
            | ManagerNoticeClass::Review
            | ManagerNoticeClass::Recovery
            | ManagerNoticeClass::Status
    )
}

#[allow(dead_code)]
pub(in crate::team) fn actionable_supervisory_notice_count(
    messages: &[inbox::InboxMessage],
) -> usize {
    actionable_supervisory_pressure_count_from_bodies(
        messages.iter().map(|message| message.body.as_str()),
    )
}

fn is_manager_completion_notice(body: &str) -> bool {
    body.contains("awaiting manual review")
        || body.contains("requires manual review")
        || is_structured_completion_packet(body)
        || (!body.starts_with("rollup:")
            && body.contains("task #")
            && body.contains("tests: passed")
            && body.contains("merge: success"))
}

fn is_structured_completion_packet(body: &str) -> bool {
    let mentions_task = body.contains("\"task_id\":") || body.contains("task_id:");
    let tests_passed = body.contains("\"tests_passed\":true")
        || body.contains("\"tests_passed\": true")
        || body.contains("tests_passed: true");
    let ready_for_review = body.contains("\"outcome\":\"ready_for_review\"")
        || body.contains("\"outcome\": \"ready_for_review\"")
        || body.contains("outcome: ready_for_review");

    mentions_task && tests_passed && ready_for_review
}

#[derive(Debug, Clone)]
struct ReviewPacketTriageNudge {
    body: String,
    cooldown_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StaleDoneTaskNotice {
    task_id: u32,
    proof_ref: String,
}

fn extract_bracketed_sender(body: &str) -> Option<String> {
    let trimmed = body.trim_start();
    let rest = trimmed.strip_prefix('[')?;
    let end = rest.find(']')?;
    let value = rest[..end].trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn extract_task_id_field(body: &str) -> Option<u32> {
    let field_start = body.find("task_id")?;
    let after_field = &body[field_start + "task_id".len()..];
    let digit_start = after_field.find(|c: char| c.is_ascii_digit())?;
    let digits = after_field[digit_start..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>();
    digits.parse().ok()
}

fn extract_task_id_number(body: &str) -> Option<u32> {
    extract_task_id(body)
        .and_then(|id| id.parse().ok())
        .or_else(|| extract_task_id_field(body))
}

fn extract_json_string_field(body: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\"");
    let field_start = body.find(&needle)?;
    let after_field = &body[field_start + needle.len()..];
    let colon = after_field.find(':')?;
    let after_colon = after_field[colon + 1..].trim_start();
    let rest = after_colon.strip_prefix('"')?;
    let end = rest.find('"')?;
    let value = rest[..end].trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn is_stale_done_task_notice_candidate(normalized: &str) -> bool {
    normalized.starts_with("branch/task mismatch detected for task #")
        || normalized.starts_with("reconciliation alert:")
        || normalized.starts_with("review urgency:")
        || normalized.starts_with("stale review normalization:")
        || normalized.starts_with("dispatch recovery needed:")
}

fn extract_expected_branch_from_notice(body: &str) -> Option<String> {
    for line in body.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(label_start) = lower.find("expected branch") {
            let after_label = &line[label_start + "expected branch".len()..];
            if let Some((_, value)) = after_label.split_once(':')
                && let Some(branch) = extract_branch_value(value.trim())
            {
                return Some(branch);
            }
        }
        if let Some(label_start) = lower.find("instead of") {
            let value = &line[label_start + "instead of".len()..];
            if let Some(branch) = extract_branch_value(value) {
                return Some(branch);
            }
        }
        if let Some(label_start) = lower.find("should be on") {
            let value = &line[label_start + "should be on".len()..];
            if let Some(branch) = extract_branch_value(value) {
                return Some(branch);
            }
        }
    }
    None
}

fn extract_branch_value(value: &str) -> Option<String> {
    for quote in ['\'', '"', '`'] {
        if let Some(start) = value.find(quote) {
            let rest = &value[start + quote.len_utf8()..];
            if let Some(end) = rest.find(quote) {
                let quoted = rest[..end].trim();
                if !quoted.is_empty() {
                    return Some(quoted.to_string());
                }
            }
        }
    }
    let value = value
        .trim_matches(|ch: char| ch == '\'' || ch == '"' || ch == '`' || ch == '.')
        .trim();
    let value = value
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches(|ch: char| ch == '\'' || ch == '"' || ch == '`' || ch == '.' || ch == ',');
    (!value.is_empty()).then(|| value.to_string())
}

fn merged_done_task_notice(
    project_root: &std::path::Path,
    body: &str,
    trunk_branch: &str,
) -> Option<StaleDoneTaskNotice> {
    let normalized = normalized_body(body);
    if !is_stale_done_task_notice_candidate(&normalized) {
        return None;
    }

    let task_id = extract_task_id_number(body)?;
    let tasks_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    let task_path = crate::task::find_task_path_by_id(&tasks_dir, task_id).ok()?;
    let task = crate::task::Task::from_file(&task_path).ok()?;
    if !matches!(task.status.as_str(), "done" | "archived") {
        return None;
    }
    if task.claimed_by.is_some() {
        return None;
    }

    let mut proof_refs = Vec::new();
    if let Some(commit) = task.commit.as_deref().filter(|value| !value.is_empty()) {
        proof_refs.push(commit.to_string());
    }
    if let Some(branch) = task.branch.as_deref().filter(|value| !value.is_empty()) {
        proof_refs.push(branch.to_string());
    }
    if let Some(branch) = extract_expected_branch_from_notice(body) {
        proof_refs.push(branch);
    }

    proof_refs.into_iter().find_map(|proof_ref| {
        crate::team::git_cmd::merge_base_is_ancestor(project_root, &proof_ref, trunk_branch)
            .ok()
            .filter(|merged| *merged)
            .map(|_| StaleDoneTaskNotice { task_id, proof_ref })
    })
}

fn extract_line_field(body: &str, field: &str) -> Option<String> {
    body.lines().find_map(|line| {
        let trimmed = line.trim();
        let (key, value) = trimmed.split_once(':')?;
        if key.trim().eq_ignore_ascii_case(field) {
            let value = value.trim().trim_matches('"');
            (!value.is_empty()).then(|| value.to_string())
        } else {
            None
        }
    })
}

fn tests_summary(body: &str, packet: Option<&completion::CompletionPacket>) -> String {
    if let Some(packet) = packet {
        return match (packet.tests_run, packet.tests_passed) {
            (true, true) => "passed (reported run)".to_string(),
            (true, false) => "failed (reported run)".to_string(),
            (false, true) => "passed (run not reported)".to_string(),
            (false, false) => "not reported".to_string(),
        };
    }

    let normalized = normalized_body(body);
    if normalized.contains("\"tests_passed\":true")
        || normalized.contains("\"tests_passed\": true")
        || normalized.contains("tests_passed: true")
        || normalized.contains("tests: passed")
    {
        "passed".to_string()
    } else if normalized.contains("\"tests_passed\":false")
        || normalized.contains("\"tests_passed\": false")
        || normalized.contains("tests_passed: false")
        || normalized.contains("tests: failed")
    {
        "failed".to_string()
    } else {
        "not reported".to_string()
    }
}

fn requested_review_action(body: &str, packet: Option<&completion::CompletionPacket>) -> String {
    if let Some(packet) = packet
        && !packet.outcome.trim().is_empty()
    {
        return format!(
            "{}; review completion and decide merge, rework, or escalation",
            packet.outcome.trim()
        );
    }

    let normalized = normalized_body(body);
    if normalized.contains("requires manual review")
        || normalized.contains("awaiting manual review")
    {
        "manual review required; decide merge, rework, or escalation".to_string()
    } else if normalized.contains("ready_for_review") || normalized.contains("ready for review") {
        "ready for review; decide merge, rework, or escalation".to_string()
    } else {
        "review the packet and decide the next action".to_string()
    }
}

fn build_review_packet_triage_nudge(
    recipient: &str,
    message: &inbox::InboxMessage,
) -> Option<ReviewPacketTriageNudge> {
    let normalized = normalized_body(&message.body);
    if normalized.starts_with("review packet delivered:") {
        return None;
    }

    let packet = completion::parse_completion(&message.body).ok();
    let task_id = packet
        .as_ref()
        .map(|packet| packet.task_id)
        .or_else(|| extract_task_id_number(&message.body));
    let is_review_packet = packet.is_some()
        || is_manager_completion_notice(&normalized)
        || (matches!(
            inbox::classify_message(message),
            inbox::MessageCategory::ReviewRequest
        ) && task_id.is_some());

    if !is_review_packet {
        return None;
    }

    let engineer = extract_bracketed_sender(&message.body).unwrap_or_else(|| message.from.clone());
    let branch = packet
        .as_ref()
        .and_then(|packet| packet.branch.clone())
        .or_else(|| extract_json_string_field(&message.body, "branch"))
        .or_else(|| extract_line_field(&message.body, "branch"));
    let commit = packet
        .as_ref()
        .and_then(|packet| packet.commit.clone())
        .or_else(|| extract_json_string_field(&message.body, "commit"))
        .or_else(|| extract_line_field(&message.body, "commit"));
    let task = task_id
        .map(|id| format!("#{id}"))
        .unwrap_or_else(|| "unknown".to_string());
    let branch = branch.unwrap_or_else(|| "not provided".to_string());
    let commit = commit.unwrap_or_else(|| "not provided".to_string());
    let tests = tests_summary(&message.body, packet.as_ref());
    let action = requested_review_action(&message.body, packet.as_ref());

    let body = format!(
        "Review packet delivered: triage this completion now.\n\
         Task: {task}\n\
         Engineer: {engineer}\n\
         Branch: {branch}\n\
         Commit: {commit}\n\
         Tests: {tests}\n\
         Requested review action: {action}\n\n\
         Packet contents:\n{}",
        message.body
    );

    Some(ReviewPacketTriageNudge {
        body,
        cooldown_key: format!("review_packet_triage:{recipient}:{}", message.id),
    })
}

fn is_manager_status_update(body: &str) -> bool {
    is_status_update_normalized(body)
}

fn is_manager_escalation_notice(body: &str) -> bool {
    body.contains("escalation:")
        || body.contains("escalating")
        || body.contains("assignment failed.")
        || body.contains("verification max iterations")
        || body.contains("could not be merged to main")
}

fn manager_notice_preview(body: &str) -> String {
    let first_line = body
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_else(|| body.trim());
    let single_line = first_line.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut preview = single_line.chars().take(220).collect::<String>();
    if single_line.chars().count() > 220 {
        preview.push_str("...");
    }
    preview
}

fn manager_notice_digest_key(message: &inbox::InboxMessage, class: ManagerNoticeClass) -> String {
    let task_scope = extract_task_id(&message.body)
        .map(|task_id| format!("task#{task_id}"))
        .unwrap_or_else(|| format!("from:{}", message.from));

    match class {
        ManagerNoticeClass::Triage => format!("{}:from:{}", class.label(), message.from),
        _ => format!("{}:{task_scope}", class.label()),
    }
}

fn build_supervisory_digest(messages: &[inbox::InboxMessage]) -> SupervisoryDigest {
    let mut entries: Vec<SupervisoryDigestEntry> = Vec::new();
    let mut entry_by_key: std::collections::HashMap<(ManagerNoticeClass, String), usize> =
        std::collections::HashMap::new();
    let mut pressure = SupervisoryPressureSnapshot::default();

    for (index, message) in messages.iter().enumerate() {
        pressure.add_notice_body(&message.body);
        let class = classify_manager_notice(&message.body);
        let preview = manager_notice_preview(&message.body);
        let dedupe_key = (class, manager_notice_digest_key(message, class));
        if let Some(existing) = entry_by_key.get(&dedupe_key) {
            entries[*existing].duplicate_count += 1;
            continue;
        }

        entry_by_key.insert(dedupe_key, entries.len());
        entries.push(SupervisoryDigestEntry {
            class,
            from: message.from.clone(),
            preview,
            duplicate_count: 1,
            first_seen: index,
        });
    }

    entries.sort_by(|left, right| {
        left.class
            .priority()
            .cmp(&right.class.priority())
            .then_with(|| right.duplicate_count.cmp(&left.duplicate_count))
            .then_with(|| left.first_seen.cmp(&right.first_seen))
    });

    SupervisoryDigest {
        duplicates_suppressed: messages.len().saturating_sub(entries.len()),
        total_messages: messages.len(),
        entries,
        pressure,
    }
}

fn format_supervisory_digest(digest: &SupervisoryDigest) -> String {
    let pressure_summary = digest
        .pressure
        .status_summary()
        .unwrap_or_else(|| "pressure 0: low-signal only".to_string());
    let header = if digest.duplicates_suppressed == 0 {
        format!(
            "[manager-digest] {pressure_summary}; {} low-signal supervisory notice(s) collapsed by actionability.",
            digest.total_messages
        )
    } else {
        format!(
            "[manager-digest] {pressure_summary}; {} low-signal supervisory notice(s) collapsed by actionability ({} duplicate(s) suppressed).",
            digest.total_messages, digest.duplicates_suppressed
        )
    };

    let entries = digest
        .entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let repeats = if entry.duplicate_count > 1 {
                format!(" x{}", entry.duplicate_count)
            } else {
                String::new()
            };
            format!(
                "{}. {} [{}{}]\n   {}",
                index + 1,
                entry.class.label(),
                entry.from,
                repeats,
                entry.preview
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "{header}\n{entries}\nImmediate tasking, direct report results, and explicit rework continue to deliver live."
    )
}

fn has_recent_delivered_duplicate(
    root: &std::path::Path,
    member_name: &str,
    new_msg: &inbox::InboxMessage,
    max_age: Duration,
) -> bool {
    let signature = inbox::message_signature(&new_msg.body);
    inbox::all_messages(root, member_name)
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, delivered)| *delivered)
        .rev()
        .any(|(existing, _)| {
            existing.age() <= max_age
                && existing.from == new_msg.from
                && existing.msg_type == new_msg.msg_type
                && inbox::message_signature(&existing.body) == signature
        })
}

impl TeamDaemon {
    fn uses_management_batching(&self, member_name: &str) -> bool {
        self.config
            .members
            .iter()
            .find(|member| member.name == member_name)
            .is_some_and(|member| {
                matches!(member.role_type, RoleType::Architect | RoleType::Manager)
            })
    }

    fn is_manager_member(&self, member_name: &str) -> bool {
        self.config
            .members
            .iter()
            .find(|member| member.name == member_name)
            .is_some_and(|member| member.role_type == RoleType::Manager)
    }

    fn is_engineer_member(&self, member_name: &str) -> bool {
        self.config
            .members
            .iter()
            .find(|member| member.name == member_name)
            .is_some_and(|member| member.role_type == RoleType::Engineer)
    }

    pub(in crate::team) fn member_ready_for_delivery(&self, member_name: &str) -> bool {
        if let Some(handle) = self.shim_handles.get(member_name) {
            if handle.is_terminal() {
                return false;
            }
            if handle.is_ready() {
                return true;
            }
            return self.states.get(member_name) == Some(&MemberState::Idle);
        }

        self.watchers
            .get(member_name)
            .map(|watcher| {
                matches!(
                    watcher.state,
                    super::super::watcher::WatcherState::Ready
                        | super::super::watcher::WatcherState::Idle
                )
            })
            .unwrap_or(true)
    }

    fn member_receives_pty_delivery(&self, member_name: &str) -> bool {
        if self.shim_handles.contains_key(member_name)
            || self.config.pane_map.contains_key(member_name)
        {
            return true;
        }
        self.config
            .members
            .iter()
            .find(|member| member.name == member_name)
            .is_some_and(|member| member.role_type != RoleType::User)
    }

    fn orchestrator_only_reason(
        &self,
        recipient: &str,
        body: &str,
    ) -> Option<OrchestratorOnlyReason> {
        if !self.member_receives_pty_delivery(recipient) {
            return None;
        }

        if is_idle_nudge(body) || is_review_nudge(body) {
            return Some(OrchestratorOnlyReason::Nudge);
        }
        if is_status_query(body) {
            return Some(OrchestratorOnlyReason::StatusQuery);
        }
        if is_standup_request(body) {
            return Some(OrchestratorOnlyReason::StandupRequest);
        }
        if self.is_engineer_member(recipient) && is_engineer_ambient_chatter(body) {
            return Some(OrchestratorOnlyReason::EngineerChatter);
        }

        None
    }

    fn cached_member_status_summary(&self, member_name: &str) -> String {
        let inbox_root = inbox::inboxes_root(&self.config.project_root);
        let pending_inbox = inbox::pending_message_count(&inbox_root, member_name).unwrap_or(0);
        let mut owned_task_buckets =
            status::owned_task_buckets(&self.config.project_root, &self.config.members);
        let owned_tasks = owned_task_buckets.remove(member_name).unwrap_or_default();
        let state = self
            .states
            .get(member_name)
            .copied()
            .unwrap_or(MemberState::Idle);
        let nudge_status = status::format_nudge_status(self.nudges.get(member_name));
        let standup_status = crate::team::standup::standup_interval_for_member_name(
            &self.config.team_config,
            &self.config.members,
            member_name,
        )
        .map(|interval| {
            status::format_standup_status(
                self.last_standup.get(member_name).copied(),
                interval,
                self.paused_standups.contains(member_name),
            )
        })
        .unwrap_or_default();
        let label = status::compose_pane_status_label(status::PaneStatusLabelArgs {
            state,
            pending_inbox,
            triage_backlog: 0,
            active_task_ids: &owned_tasks.active,
            review_task_ids: &owned_tasks.review,
            globally_paused: super::super::pause_marker_path(&self.config.project_root).exists(),
            nudge_status: &nudge_status,
            standup_status: &standup_status,
        });
        let watcher_state = self
            .watchers
            .get(member_name)
            .map(|watcher| format!("{:?}", watcher.state))
            .unwrap_or_else(|| "Unknown".to_string())
            .to_ascii_lowercase();
        format!(
            "{} | watcher {watcher_state}",
            status::strip_tmux_style(&label)
        )
    }

    fn record_orchestrator_only_message(
        &self,
        from: &str,
        recipient: &str,
        body: &str,
        reason: OrchestratorOnlyReason,
    ) {
        let preview = shim_log_preview(body);
        match reason {
            OrchestratorOnlyReason::Nudge => self.record_orchestrator_action(format!(
                "notification isolation: diverted {} for {} from PTY injection ({preview})",
                reason.label(),
                recipient
            )),
            OrchestratorOnlyReason::StatusQuery | OrchestratorOnlyReason::StandupRequest => {
                let cached = self.cached_member_status_summary(recipient);
                self.record_orchestrator_action(format!(
                    "notification isolation: answered {} from {} about {} using cached state -> {}",
                    reason.label(),
                    from,
                    recipient,
                    cached
                ));
            }
            OrchestratorOnlyReason::EngineerChatter => self.record_orchestrator_action(format!(
                "notification isolation: diverted non-actionable engineer notice from {} for {} ({preview})",
                from, recipient
            )),
        }
    }

    fn suppress_stale_done_task_notice(
        &mut self,
        from: &str,
        recipient: &str,
        notice: &StaleDoneTaskNotice,
    ) {
        let cooldown_key = format!(
            "stale-done-task-notice:{}:{}:{}",
            recipient, notice.task_id, notice.proof_ref
        );
        if self.intervention_cooldowns.contains_key(&cooldown_key) {
            return;
        }

        self.intervention_cooldowns
            .insert(cooldown_key, Instant::now());
        let task = notice.task_id.to_string();
        self.emit_event(TeamEvent::state_reconciliation(
            Some(recipient),
            Some(&task),
            "stale_done_notice_suppressed",
        ));
        self.record_orchestrator_action(format!(
            "stale done notice suppressed: {recipient} from {from} for task #{} already merged via {}",
            notice.task_id, notice.proof_ref
        ));
    }

    fn pending_ack_contains_inbox_id(&self, inbox_id: &str) -> bool {
        self.pending_shim_acks.values().any(|pending| {
            pending
                .inbox_messages
                .iter()
                .any(|message| message.id == inbox_id)
        })
    }

    fn pending_ack_contains_any_inbox_id(&self, messages: &[inbox::InboxMessage]) -> bool {
        messages
            .iter()
            .any(|message| self.pending_ack_contains_inbox_id(&message.id))
    }

    fn manager_digest_backpressure_key(member_name: &str) -> String {
        format!("manager-digest-backpressure::{member_name}")
    }

    fn manager_digest_backpressure_on_cooldown(&self, member_name: &str) -> bool {
        self.intervention_cooldowns
            .get(&Self::manager_digest_backpressure_key(member_name))
            .is_some_and(|sent_at| {
                sent_at.elapsed() < Duration::from_secs(MANAGER_DIGEST_BACKPRESSURE_COOLDOWN_SECS)
            })
    }

    fn enqueue_pending_shim_ack(
        &mut self,
        message_id: String,
        recipient: &str,
        from: &str,
        body: &str,
        inbox_messages: Vec<PendingAckInboxMessage>,
    ) {
        self.pending_shim_acks.insert(
            message_id,
            PendingShimAck {
                recipient: recipient.to_string(),
                from: from.to_string(),
                body: body.to_string(),
                inbox_messages,
            },
        );
    }

    fn send_live_shim_message(
        &mut self,
        recipient: &str,
        from: &str,
        body: &str,
        message_id: String,
        inbox_messages: Vec<PendingAckInboxMessage>,
    ) -> Result<MessageDelivery> {
        let result = {
            let Some(handle) = self.shim_handles.get_mut(recipient) else {
                return Ok(MessageDelivery::InboxQueued);
            };
            handle.send_message_with_id(from, body, Some(message_id.clone()))
        };

        match result {
            Ok(()) => {
                let ack_tracks_inbox = !inbox_messages.is_empty();
                self.enqueue_pending_shim_ack(message_id, recipient, from, body, inbox_messages);
                let _ = append_shim_event_log(
                    &self.config.project_root,
                    recipient,
                    &format!("-> {from}: {}", shim_log_preview(body)),
                );
                info!(from, to = recipient, "delivered message via shim channel");
                if !ack_tracks_inbox {
                    self.record_message_routed(from, recipient);
                    self.record_notification_delivery_sample(from, recipient, 0, "live");
                }
                self.mark_member_working(recipient);
                Ok(MessageDelivery::LivePane)
            }
            Err(error) => Err(error),
        }
    }

    /// Drain pending messages for an agent that just became ready.
    /// Called from `poll_watchers()` when `ready_confirmed` transitions to true.
    pub(in crate::team) fn drain_pending_queue(&mut self, recipient: &str) -> Result<()> {
        let messages = self
            .pending_delivery_queue
            .remove(recipient)
            .unwrap_or_default();
        if messages.is_empty() {
            return Ok(());
        }
        info!(
            recipient,
            count = messages.len(),
            "draining pending delivery queue after agent became ready"
        );
        for msg in messages {
            self.deliver_message_with_id(&msg.from, recipient, &msg.body, msg.message_id)?;
        }
        Ok(())
    }

    /// Expire pending messages that have been queued longer than the
    /// configured max age, falling back to inbox delivery so they aren't
    /// silently lost when an agent appears permanently busy.
    ///
    /// When multiple messages from the same sender expire at once, they are
    /// collapsed into a single digest to avoid flooding the recipient.
    pub(in crate::team) fn expire_stale_pending_messages(&mut self) -> Result<()> {
        let max_age_secs = self.config.team_config.pending_queue_max_age_secs;
        if max_age_secs == 0 {
            return Ok(());
        }
        let max_age = Duration::from_secs(max_age_secs);

        let recipients: Vec<String> = self.pending_delivery_queue.keys().cloned().collect();
        let inbox_root = inbox::inboxes_root(&self.config.project_root);
        let tiered = self.config.team_config.workflow_policy.tiered_inboxes;

        for recipient in recipients {
            let Some(messages) = self.pending_delivery_queue.get_mut(&recipient) else {
                continue;
            };

            let mut expired: Vec<super::PendingMessage> = Vec::new();
            let mut kept = Vec::new();
            for msg in messages.drain(..) {
                if msg.queued_at.elapsed() >= max_age {
                    expired.push(msg);
                } else {
                    kept.push(msg);
                }
            }

            if !expired.is_empty() {
                let total_expired = expired.len();
                warn!(
                    recipient = recipient.as_str(),
                    count = total_expired,
                    max_age_secs,
                    "expiring stale pending messages to inbox fallback"
                );

                // Group expired messages by sender and deliver digests
                // to avoid flooding the recipient with hundreds of individual messages.
                const DIGEST_THRESHOLD: usize = 3;
                let mut by_sender: std::collections::HashMap<String, Vec<&super::PendingMessage>> =
                    std::collections::HashMap::new();
                for msg in &expired {
                    by_sender.entry(msg.from.clone()).or_default().push(msg);
                }

                for (sender, sender_msgs) in &by_sender {
                    if sender_msgs.len() <= DIGEST_THRESHOLD {
                        // Few messages — deliver individually
                        for msg in sender_msgs {
                            let inbox_msg =
                                inbox::InboxMessage::new_send(&msg.from, &recipient, &msg.body);
                            if let Err(error) = crate::team::inbox_tiered::deliver_flag_aware(
                                &inbox_root,
                                &inbox_msg,
                                tiered,
                            ) {
                                warn!(
                                    from = msg.from.as_str(),
                                    to = recipient.as_str(),
                                    error = %error,
                                    "failed to deliver expired pending message to inbox"
                                );
                            }
                        }
                    } else {
                        // Many messages — collapse into a single digest
                        let oldest_age_secs = sender_msgs
                            .iter()
                            .map(|m| m.queued_at.elapsed().as_secs())
                            .max()
                            .unwrap_or(0);
                        let newest = sender_msgs.last().unwrap();
                        let newest_preview: String = newest
                            .body
                            .lines()
                            .next()
                            .unwrap_or("")
                            .chars()
                            .take(200)
                            .collect();
                        let digest = format!(
                            "[digest] {} messages from {} expired after {}s (oldest: {}s ago). Most recent:\n{}",
                            sender_msgs.len(),
                            sender,
                            max_age_secs,
                            oldest_age_secs,
                            newest_preview,
                        );
                        let inbox_msg = inbox::InboxMessage::new_send(sender, &recipient, &digest);
                        if let Err(error) = crate::team::inbox_tiered::deliver_flag_aware(
                            &inbox_root,
                            &inbox_msg,
                            tiered,
                        ) {
                            warn!(
                                from = sender.as_str(),
                                to = recipient.as_str(),
                                error = %error,
                                "failed to deliver digest message to inbox"
                            );
                        }
                        info!(
                            from = sender.as_str(),
                            to = recipient.as_str(),
                            count = sender_msgs.len(),
                            "collapsed expired messages into digest"
                        );
                    }
                }
            }

            if kept.is_empty() {
                self.pending_delivery_queue.remove(&recipient);
            } else {
                *self.pending_delivery_queue.get_mut(&recipient).unwrap() = kept;
            }
        }
        Ok(())
    }

    pub(in crate::team) fn queue_daemon_message(
        &mut self,
        recipient: &str,
        body: &str,
    ) -> Result<MessageDelivery> {
        let visible_sender = self.automation_sender_for(recipient);
        self.deliver_message(&visible_sender, recipient, body)
    }

    pub(in crate::team) fn queue_message(
        &mut self,
        from: &str,
        recipient: &str,
        body: &str,
    ) -> Result<()> {
        self.deliver_message(from, recipient, body).map(|_| ())
    }

    fn deliver_message(
        &mut self,
        from: &str,
        recipient: &str,
        body: &str,
    ) -> Result<MessageDelivery> {
        self.deliver_message_with_id(from, recipient, body, None)
    }

    fn deliver_message_with_id(
        &mut self,
        from: &str,
        recipient: &str,
        body: &str,
        message_id: Option<String>,
    ) -> Result<MessageDelivery> {
        if from != "human"
            && let Some(notice) = merged_done_task_notice(
                &self.config.project_root,
                body,
                self.config.team_config.trunk_branch(),
            )
        {
            info!(
                from,
                to = recipient,
                task_id = notice.task_id,
                proof_ref = %notice.proof_ref,
                "suppressing stale done-task notice"
            );
            self.suppress_stale_done_task_notice(from, recipient, &notice);
            return Ok(MessageDelivery::OrchestratorLogged);
        }

        if let Some(channel) = self.channels.get(recipient) {
            let _ = channel;
            return self.deliver_channel_message(from, recipient, body);
        }

        if let Some(reason) = self.orchestrator_only_reason(recipient, body) {
            info!(
                from,
                to = recipient,
                reason = reason.label(),
                "diverting message to orchestrator log"
            );
            self.record_orchestrator_only_message(from, recipient, body, reason);
            return Ok(MessageDelivery::OrchestratorLogged);
        }

        // Shim delivery path: deliver via the structured shim channel.
        if let Some((is_ready, is_terminal, state)) = self
            .shim_handles
            .get(recipient)
            .map(|handle| (handle.is_ready(), handle.is_terminal(), handle.state))
        {
            let message_id = message_id.unwrap_or_else(|| Uuid::new_v4().to_string());
            if is_ready {
                match self.send_live_shim_message(recipient, from, body, message_id, Vec::new()) {
                    Ok(delivery) => return Ok(delivery),
                    Err(error) => {
                        warn!(from, to = recipient, error = %error, "shim channel delivery failed; falling through to inbox");
                    }
                }
            } else if !is_terminal {
                info!(
                    from,
                    to = recipient,
                    state = %state,
                    "shim agent not ready; deferring to pending queue"
                );
                self.pending_delivery_queue
                    .entry(recipient.to_string())
                    .or_default()
                    .push(PendingMessage {
                        from: from.to_string(),
                        body: body.to_string(),
                        message_id: Some(message_id),
                        queued_at: Instant::now(),
                    });
                let _ = append_shim_event_log(
                    &self.config.project_root,
                    recipient,
                    &format!(".. pending {from}: {}", shim_log_preview(body)),
                );
                return Ok(MessageDelivery::DeferredPending);
            }
            // Terminal state falls through to inbox
        }

        let known_recipient = self.config.pane_map.contains_key(recipient)
            || self
                .config
                .members
                .iter()
                .any(|member| member.name == recipient);
        if !known_recipient {
            debug!(from, recipient, "skipping message for unknown recipient");
            return Ok(MessageDelivery::SkippedUnknownRecipient);
        }

        // All delivery falls through to inbox when shim channel is unavailable.
        let root = inbox::inboxes_root(&self.config.project_root);
        let msg = inbox::InboxMessage::new_send(from, recipient, body);
        let tiered = self.config.team_config.workflow_policy.tiered_inboxes;
        crate::team::inbox_tiered::deliver_flag_aware(&root, &msg, tiered).map_err(|error| {
            DeliveryError::InboxQueue {
                recipient: recipient.to_string(),
                detail: error.to_string(),
            }
        })?;
        self.record_message_routed(from, recipient);
        Ok(MessageDelivery::InboxQueued)
    }

    pub(in crate::team) fn drain_legacy_command_queue(&mut self) -> Result<()> {
        let queue_path = message::command_queue_path(&self.config.project_root);
        let commands = message::read_command_queue(&queue_path)?;
        if commands.is_empty() {
            return Ok(());
        }

        let root = inbox::inboxes_root(&self.config.project_root);
        let tiered = self.config.team_config.workflow_policy.tiered_inboxes;
        let mut remaining_commands = Vec::new();
        for cmd in commands {
            let result: Result<()> = (|| match &cmd {
                message::QueuedCommand::Send {
                    from,
                    to,
                    message: msg,
                } => {
                    let is_user =
                        self.config.team_config.roles.iter().any(|role| {
                            role.name == to.as_str() && role.role_type == RoleType::User
                        });

                    if is_user {
                        if let Some(channel) = self.channels.get(to.as_str()) {
                            let formatted = format!("[From {from}]\n{msg}");
                            let _ = channel;
                            self.deliver_channel_message(from, to, &formatted)?;
                        }
                    } else {
                        let inbox_msg = inbox::InboxMessage::new_send(from, to, msg);
                        crate::team::inbox_tiered::deliver_flag_aware(&root, &inbox_msg, tiered)?;
                        debug!(from, to, "legacy command routed to inbox");
                    }
                    Ok(())
                }
                message::QueuedCommand::Assign {
                    from,
                    engineer,
                    task,
                } => {
                    let msg = inbox::InboxMessage::new_assign(from, engineer, task);
                    crate::team::inbox_tiered::deliver_flag_aware(&root, &msg, tiered)?;
                    debug!(engineer, "legacy assign routed to inbox");
                    Ok(())
                }
            })();

            if let Err(error) = result {
                warn!(error = %error, "failed to process legacy command; preserving in queue");
                remaining_commands.push(cmd);
            }
        }

        message::write_command_queue(&queue_path, &remaining_commands)?;
        Ok(())
    }

    pub(in crate::team) fn deliver_inbox_messages(&mut self) -> Result<()> {
        let root = inbox::inboxes_root(&self.config.project_root);
        let member_names: Vec<String> = self.config.pane_map.keys().cloned().collect();

        for name in &member_names {
            if !self.member_ready_for_delivery(name) {
                continue;
            }

            let messages = match inbox::pending_messages(&root, name) {
                Ok(msgs) => msgs,
                Err(error) => {
                    debug!(member = %name, error = %error, "failed to read inbox");
                    continue;
                }
            };

            if messages.is_empty() {
                continue;
            }

            let mut delivered_any = false;
            let mut digested_ids = std::collections::HashSet::new();
            let mut suppressed_ids = std::collections::HashSet::new();
            let mut pending_manager_digest: Option<Vec<inbox::InboxMessage>> = None;
            let mut pending_manager_digest_ids = std::collections::HashSet::new();
            for message in &messages {
                if !matches!(message.msg_type, inbox::MessageType::Send) || message.from == "human"
                {
                    continue;
                }
                let Some(notice) = merged_done_task_notice(
                    &self.config.project_root,
                    &message.body,
                    self.config.team_config.trunk_branch(),
                ) else {
                    continue;
                };
                if let Err(error) = inbox::mark_delivered(&root, name, &message.id) {
                    warn!(
                        member = %name,
                        id = %message.id,
                        error = %error,
                        "failed to mark stale done-task notice delivered"
                    );
                    continue;
                }
                suppressed_ids.insert(message.id.clone());
                self.suppress_stale_done_task_notice(&message.from, name, &notice);
            }
            if self.is_manager_member(name) {
                let suppression_window = Duration::from_secs(600);
                for message in &messages {
                    if suppressed_ids.contains(&message.id) {
                        continue;
                    }
                    if !matches!(message.msg_type, inbox::MessageType::Send) {
                        continue;
                    }
                    if !is_manager_escalation_notice(&normalized_body(&message.body)) {
                        continue;
                    }
                    if !has_recent_delivered_duplicate(&root, name, message, suppression_window) {
                        continue;
                    }
                    if let Err(error) = inbox::mark_delivered(&root, name, &message.id) {
                        warn!(
                            member = %name,
                            id = %message.id,
                            error = %error,
                            "failed to mark duplicate escalation delivered"
                        );
                        continue;
                    }
                    suppressed_ids.insert(message.id.clone());
                    self.record_orchestrator_action(format!(
                        "supervision routing: suppressed duplicate escalation for {name} from {} within cooldown",
                        message.from
                    ));
                }
                let digestible_messages: Vec<inbox::InboxMessage> = messages
                    .iter()
                    .filter(|msg| !suppressed_ids.contains(&msg.id))
                    .filter(|msg| !self.pending_ack_contains_inbox_id(&msg.id))
                    .filter(|msg| matches!(msg.msg_type, inbox::MessageType::Send))
                    .filter(|msg| should_batch_manager_notice(classify_manager_notice(&msg.body)))
                    .cloned()
                    .collect();
                if digestible_messages.len() > 1 {
                    pending_manager_digest_ids = digestible_messages
                        .iter()
                        .map(|message| message.id.clone())
                        .collect();
                    pending_manager_digest = Some(digestible_messages);
                }
            } else if self.uses_management_batching(name) {
                let batched_messages: Vec<inbox::InboxMessage> = messages
                    .iter()
                    .filter(|msg| matches!(msg.msg_type, inbox::MessageType::Send))
                    .cloned()
                    .collect();
                if batched_messages.len() > 1
                    && self.deliver_batched_management_messages(&root, name, &batched_messages)?
                {
                    self.mark_member_working(name);
                    continue;
                }
            }

            let Some(_pane_id) = self.config.pane_map.get(name).cloned() else {
                continue;
            };

            let mut ordered_messages = messages.clone();
            if self.is_manager_member(name) {
                ordered_messages.sort_by_key(|message| {
                    (
                        classify_manager_notice(&message.body).priority(),
                        message.timestamp,
                    )
                });
            }

            for msg in &ordered_messages {
                if digested_ids.contains(&msg.id)
                    || suppressed_ids.contains(&msg.id)
                    || pending_manager_digest_ids.contains(&msg.id)
                    || self.pending_ack_contains_inbox_id(&msg.id)
                {
                    continue;
                }
                let from_role = self.resolve_role_name(&msg.from);
                let to_role = self.resolve_role_name(name);
                if !self.config.team_config.can_talk(&from_role, &to_role) {
                    warn!(
                        from = %msg.from, from_role, to = %name, to_role,
                        "blocked message: routing not allowed"
                    );
                    let _ = inbox::mark_delivered(&root, name, &msg.id);
                    continue;
                }

                let is_send = matches!(msg.msg_type, inbox::MessageType::Send);
                let delivery_result: Result<MessageDelivery> = match msg.msg_type {
                    inbox::MessageType::Send => {
                        if let Some(reason) = self.orchestrator_only_reason(name, &msg.body) {
                            info!(
                                from = %msg.from,
                                to = %name,
                                id = %msg.id,
                                reason = reason.label(),
                                "diverting inbox message to orchestrator log"
                            );
                            self.record_orchestrator_only_message(
                                &msg.from, name, &msg.body, reason,
                            );
                            Ok(MessageDelivery::OrchestratorLogged)
                        } else {
                            let review_packet_nudge = build_review_packet_triage_nudge(name, msg)
                                .filter(|nudge| {
                                    let cooldown = Duration::from_secs(
                                        self.config
                                            .team_config
                                            .automation
                                            .intervention_cooldown_secs,
                                    );
                                    self.intervention_cooldowns
                                        .get(&nudge.cooldown_key)
                                        .is_none_or(|sent_at| sent_at.elapsed() >= cooldown)
                                });
                            let delivery_body = review_packet_nudge
                                .as_ref()
                                .map(|nudge| nudge.body.as_str())
                                .unwrap_or(&msg.body);
                            info!(from = %msg.from, to = %name, id = %msg.id, "delivering inbox message via shim");
                            if self.shim_handles.contains_key(name) {
                                let result = self.send_live_shim_message(
                                    name,
                                    &msg.from,
                                    delivery_body,
                                    msg.id.clone(),
                                    vec![PendingAckInboxMessage {
                                        id: msg.id.clone(),
                                        from: msg.from.clone(),
                                        age_secs: msg.age().as_secs(),
                                        sample_kind: "inbox",
                                    }],
                                );
                                if result.is_ok()
                                    && let Some(handle) = self.shim_handles.get_mut(name)
                                {
                                    handle.apply_state_change(
                                        crate::shim::protocol::ShimState::Working,
                                    );
                                }
                                if result.is_ok()
                                    && let Some(nudge) = review_packet_nudge
                                {
                                    self.intervention_cooldowns
                                        .insert(nudge.cooldown_key, Instant::now());
                                }
                                result
                            } else {
                                // No shim handle — skip, leave in inbox
                                continue;
                            }
                        }
                    }
                    inbox::MessageType::Assign => {
                        // Check WIP limit: don't assign if engineer already has active work
                        let board_dir = self.board_dir();
                        let tasks_dir = board_dir.join("tasks");
                        let active_count = if tasks_dir.exists() {
                            crate::task::load_tasks_from_dir(&tasks_dir)
                                .unwrap_or_default()
                                .iter()
                                .filter(|t| {
                                    t.claimed_by.as_deref() == Some(name)
                                        && matches!(t.status.as_str(), "in-progress" | "review")
                                })
                                .count()
                        } else {
                            0
                        };
                        if active_count > 0 {
                            warn!(
                                to = %name,
                                from = %msg.from,
                                active_count,
                                "rejecting assignment: engineer already has {active_count} active board item(s)"
                            );
                            // Notify the sender with details of what the engineer is working on
                            let active_tasks_desc: String = if tasks_dir.exists() {
                                crate::task::load_tasks_from_dir(&tasks_dir)
                                    .unwrap_or_default()
                                    .iter()
                                    .filter(|t| {
                                        t.claimed_by.as_deref() == Some(name)
                                            && matches!(t.status.as_str(), "in-progress" | "review")
                                    })
                                    .map(|t| format!("#{} ({}: {})", t.id, t.status, t.title))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            } else {
                                String::new()
                            };
                            let reject_msg = format!(
                                "Assignment to {name} rejected (WIP limit): engineer already has {active_count} active item(s): {active_tasks_desc}. \
                                 Merge or complete the current work first, then re-assign."
                            );
                            let _ = self.queue_message("daemon", &msg.from, &reject_msg);
                            // Still mark delivered so it doesn't retry
                            Ok(MessageDelivery::OrchestratorLogged)
                        } else {
                            info!(to = %name, id = %msg.id, "delivering inbox assignment");
                            self.manual_assign_cooldowns
                                .insert(name.to_string(), Instant::now());
                            let task_id = extract_task_id_from_body(&msg.body);
                            // Claim the task on the board BEFORE launching the
                            // assignment so auto-dispatch sees claimed_by and
                            // skips this task. Without this, there is a race
                            // window where auto-dispatch grabs the unclaimed
                            // task and assigns it to a different engineer.
                            if let Some(tid) = task_id {
                                if let Err(e) = crate::team::task_cmd::assign_task_owners(
                                    &board_dir,
                                    tid,
                                    Some(name),
                                    None,
                                ) {
                                    debug!(task_id = tid, error = %e, "could not set claimed_by on manual assign");
                                }
                            }
                            self.assign_task_with_task_id_as(&msg.from, name, &msg.body, task_id)
                            .map(|launch| {
                                if let Some(tid) = task_id {
                                    if let Err(e) = crate::team::task_cmd::transition_task_with_attribution(
                                        &board_dir,
                                        tid,
                                        "in-progress",
                                        crate::team::task_cmd::StatusTransitionAttribution::daemon(
                                            "daemon.delivery.routing.assign",
                                        ),
                                    ) {
                                        debug!(task_id = tid, error = %e, "could not transition task to in-progress on assign");
                                    }
                                }
                                self.record_assignment_success(name, &msg.id, &msg.body, &launch);
                                self.notify_assignment_sender_success(
                                    &msg.from, name, &msg.id, &msg.body, &launch,
                                );
                                MessageDelivery::LivePane
                            })
                        }
                    }
                };

                let mut mark_delivered = false;
                match delivery_result {
                    Ok(delivery) => {
                        if matches!(delivery, MessageDelivery::LivePane) {
                            delivered_any = true;
                        }
                        mark_delivered =
                            !(is_send && matches!(delivery, MessageDelivery::LivePane));
                        if is_send && mark_delivered {
                            // Shim delivery is authoritative once the command reaches the
                            // structured channel. Pane-marker verification is a legacy tmux
                            // heuristic and produces false negatives for Claude/Codex shims.
                            self.clear_failed_delivery(name, &msg.from, &msg.body);
                        }
                    }
                    Err(error) => {
                        warn!(
                            from = %msg.from,
                            to = %name,
                            id = %msg.id,
                            error = %error,
                            "failed to deliver inbox message"
                        );
                        if matches!(msg.msg_type, inbox::MessageType::Assign) {
                            mark_delivered = true;
                            self.record_assignment_failure(name, &msg.id, &msg.body, &error);
                            self.notify_assignment_sender_failure(
                                &msg.from, name, &msg.id, &msg.body, &error,
                            );
                        }
                    }
                }

                if !mark_delivered {
                    continue;
                }

                if let Err(error) = inbox::mark_delivered(&root, name, &msg.id) {
                    warn!(
                        member = %name,
                        id = %msg.id,
                        error = %error,
                        "failed to mark delivered"
                    );
                } else {
                    self.record_message_routed(&msg.from, name);
                    self.record_notification_delivery_sample(
                        &msg.from,
                        name,
                        msg.age().as_secs(),
                        "inbox",
                    );
                }

                std::thread::sleep(Duration::from_secs(1));
            }

            if !delivered_any && let Some(digest_messages) = pending_manager_digest.as_ref() {
                let flushed_ids = self.flush_manager_digest(&root, name, digest_messages)?;
                if !flushed_ids.is_empty() {
                    delivered_any = true;
                    digested_ids.extend(flushed_ids);
                }
            }

            if delivered_any {
                self.mark_member_working(name);
            }
        }

        Ok(())
    }

    fn deliver_batched_management_messages(
        &mut self,
        _root: &std::path::Path,
        member_name: &str,
        messages: &[inbox::InboxMessage],
    ) -> Result<bool> {
        if !self.shim_handles.contains_key(member_name) {
            return Ok(false);
        }

        let first_sender = messages
            .first()
            .map(|message| message.from.as_str())
            .unwrap_or("daemon");
        let batched_body = format_batched_message(messages);
        let ack_messages = messages
            .iter()
            .map(|message| PendingAckInboxMessage {
                id: message.id.clone(),
                from: message.from.clone(),
                age_secs: message.age().as_secs(),
                sample_kind: "batched",
            })
            .collect::<Vec<_>>();
        let message_id = format!("batched-{}", Uuid::new_v4());
        info!(
            to = %member_name,
            count = messages.len(),
            "delivering batched inbox messages via shim"
        );
        if let Err(error) = self.send_live_shim_message(
            member_name,
            first_sender,
            &batched_body,
            message_id,
            ack_messages,
        ) {
            warn!(
                to = %member_name,
                count = messages.len(),
                error = %error,
                "failed to deliver batched inbox messages"
            );
            return Ok(false);
        }

        for message in messages {
            debug!(member = %member_name, id = %message.id, "batched inbox message waiting for shim delivery ack");
        }

        Ok(true)
    }

    fn flush_manager_digest(
        &mut self,
        _root: &std::path::Path,
        member_name: &str,
        messages: &[inbox::InboxMessage],
    ) -> Result<Vec<String>> {
        let sender = self.automation_sender_for(member_name);
        if !self
            .shim_handles
            .get(member_name)
            .is_some_and(|handle| handle.is_ready())
        {
            return Ok(Vec::new());
        }
        if self.pending_ack_contains_any_inbox_id(messages) {
            debug!(
                to = %member_name,
                count = messages.len(),
                "skipping manager supervisory digest while prior digest ack is pending"
            );
            return Ok(Vec::new());
        }
        if self.manager_digest_backpressure_on_cooldown(member_name) {
            debug!(
                to = %member_name,
                count = messages.len(),
                "skipping manager supervisory digest while backpressure cooldown is active"
            );
            return Ok(Vec::new());
        }

        let digest = build_supervisory_digest(messages);
        let digest_body = format_supervisory_digest(&digest);
        let ack_messages = messages
            .iter()
            .map(|message| PendingAckInboxMessage {
                id: message.id.clone(),
                from: message.from.clone(),
                age_secs: message.age().as_secs(),
                sample_kind: "digest",
            })
            .collect::<Vec<_>>();
        let message_id = format!("digest-{}", Uuid::new_v4());
        info!(
            to = %member_name,
            count = digest.total_messages,
            unique = digest.entries.len(),
            duplicates_suppressed = digest.duplicates_suppressed,
            "delivering manager supervisory digest via shim"
        );
        if let Err(error) = self.send_live_shim_message(
            member_name,
            &sender,
            &digest_body,
            message_id,
            ack_messages,
        ) {
            warn!(
                to = %member_name,
                count = digest.total_messages,
                error = %error,
                "failed to deliver manager supervisory digest"
            );
            self.intervention_cooldowns.insert(
                Self::manager_digest_backpressure_key(member_name),
                Instant::now(),
            );
            return Ok(Vec::new());
        }
        self.intervention_cooldowns
            .remove(&Self::manager_digest_backpressure_key(member_name));
        let mut class_counts: std::collections::HashMap<&'static str, usize> =
            std::collections::HashMap::new();
        for entry in &digest.entries {
            *class_counts.entry(entry.class.label()).or_default() += entry.duplicate_count;
        }
        let mut class_summary = class_counts.into_iter().collect::<Vec<_>>();
        class_summary.sort_by_key(|(left, _)| *left);
        let class_summary = class_summary
            .into_iter()
            .map(|(label, count)| format!("{label}={count}"))
            .collect::<Vec<_>>()
            .join(", ");
        self.record_orchestrator_action(format!(
            "supervision digest: {member_name} batched {} notice(s) into {} digest line(s) (duplicates suppressed: {}; classes: {class_summary})",
            digest.total_messages,
            digest.entries.len(),
            digest.duplicates_suppressed,
        ));
        self.record_supervisory_digest_emitted(
            member_name,
            u32::try_from(digest.total_messages).unwrap_or(u32::MAX),
            u32::try_from(digest.duplicates_suppressed).unwrap_or(u32::MAX),
        );

        Ok(messages.iter().map(|message| message.id.clone()).collect())
    }

    fn resolve_role_name(&self, member_name: &str) -> String {
        if member_name == "human" || member_name == "daemon" {
            return member_name.to_string();
        }
        self.config
            .members
            .iter()
            .find(|member| member.name == member_name)
            .map(|member| member.role_name.clone())
            .unwrap_or_else(|| member_name.to_string())
    }
}

fn is_engineer_actionable_message(body: &str) -> bool {
    let normalized = normalized_body(body);
    let category =
        inbox::classify_message(&inbox::InboxMessage::new_send("daemon", "engineer", body));

    if matches!(
        category,
        inbox::MessageCategory::ReviewRequest | inbox::MessageCategory::Blocker
    ) {
        return true;
    }

    normalized.contains("verification rejected this completion")
        || normalized.contains("fix attempt ")
        || normalized.contains("fix these failures")
        || normalized.contains("merge conflict during rebase")
        || normalized.contains("wait for lead direction before making more changes")
        || normalized.contains("your task passed tests, but")
}

fn is_engineer_ambient_chatter(body: &str) -> bool {
    if is_engineer_actionable_message(body) {
        return false;
    }

    let normalized = normalized_body(body);
    is_idle_nudge(body)
        || is_review_nudge(body)
        || is_manager_status_update(&normalized)
        || normalized.starts_with("[digest]")
        || normalized.starts_with("[manager-digest]")
        || normalized.starts_with("recovery:")
        || normalized.starts_with("review urgency:")
        || normalized.starts_with("stale review normalization:")
        || normalized.starts_with("triage backlog detected:")
        || normalized.starts_with("dispatch recovery needed:")
        || normalized.contains("utilization recovery")
        || normalized.starts_with("utilization gap detected:")
        || normalized.starts_with("architect utilization")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::super::{MessageDelivery, PendingMessage};
    use super::OrchestratorOnlyReason;
    use crate::team::AssignmentResultStatus;
    use crate::team::comms::Channel;
    use crate::team::config::OrchestratorPosition;
    use crate::team::config::RoleDef;
    use crate::team::config::RoleType;
    use crate::team::config::{
        AutomationConfig, BoardConfig, StandupConfig, WorkflowMode, WorkflowPolicy,
    };
    use crate::team::daemon::{DaemonConfig, TeamDaemon};
    use crate::team::errors::DeliveryError;
    use crate::team::events::EventSink;
    use crate::team::hierarchy::MemberInstance;
    use crate::team::inbox;
    use crate::team::message;
    use crate::team::standup::MemberState;
    use crate::team::test_support::{
        TestDaemonBuilder, architect_member, engineer_member, git_ok, inferred_role_defs,
        init_git_repo, manager_member, test_channel_config, write_board_task_file,
    };

    struct RecordingChannel {
        messages: Arc<Mutex<Vec<String>>>,
    }

    impl Channel for RecordingChannel {
        fn send(&self, message: &str) -> std::result::Result<(), DeliveryError> {
            self.messages.lock().unwrap().push(message.to_string());
            Ok(())
        }

        fn channel_type(&self) -> &str {
            "test"
        }
    }

    struct FailingChannel;

    impl Channel for FailingChannel {
        fn send(&self, _message: &str) -> std::result::Result<(), DeliveryError> {
            Err(DeliveryError::ChannelSend {
                recipient: "test-recipient".to_string(),
                detail: "synthetic channel failure".to_string(),
            })
        }

        fn channel_type(&self) -> &str {
            "test-failing"
        }
    }

    struct FailingWriter;

    impl io::Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("synthetic event sink failure"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("synthetic event sink failure"))
        }
    }

    fn empty_legacy_daemon(tmp: &tempfile::TempDir) -> TeamDaemon {
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        daemon.event_sink = EventSink::new(&tmp.path().join("events.jsonl")).unwrap();
        daemon
    }

    fn failed_delivery_test_daemon(tmp: &tempfile::TempDir) -> TeamDaemon {
        let mut daemon = empty_legacy_daemon(tmp);
        daemon.config.members = vec![
            architect_member("architect"),
            manager_member("manager", Some("architect")),
            engineer_member("eng-1", Some("manager"), false),
        ];
        daemon.config.team_config.roles = inferred_role_defs(&daemon.config.members);
        daemon.config.pane_map = HashMap::from([("eng-1".to_string(), "%9999999".to_string())]);
        daemon
    }

    fn install_idle_test_shim(
        daemon: &mut TeamDaemon,
        tmp: &tempfile::TempDir,
        member: &str,
    ) -> crate::shim::protocol::Channel {
        let (parent_sock, child_sock) = crate::shim::protocol::socketpair().unwrap();
        let parent_channel = crate::shim::protocol::Channel::new(parent_sock);
        let child_channel = crate::shim::protocol::Channel::new(child_sock);
        let mut handle = crate::team::daemon::agent_handle::AgentHandle::new(
            member.to_string(),
            parent_channel,
            12345,
            "claude".to_string(),
            "claude".to_string(),
            tmp.path().to_path_buf(),
        );
        handle.apply_state_change(crate::shim::protocol::ShimState::Idle);
        daemon.shim_handles.insert(member.to_string(), handle);
        daemon
            .states
            .insert(member.to_string(), crate::team::standup::MemberState::Idle);
        child_channel
    }

    #[test]
    fn queue_daemon_message_routes_to_channel_for_user_roles() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        let sent = Arc::new(Mutex::new(Vec::new()));
        daemon.channels.insert(
            "human".to_string(),
            Box::new(RecordingChannel {
                messages: Arc::clone(&sent),
            }),
        );

        daemon
            .queue_daemon_message("human", "Assignment delivered.")
            .unwrap();

        assert_eq!(sent.lock().unwrap().as_slice(), ["Assignment delivered."]);
    }

    #[test]
    fn queue_daemon_message_ignores_event_sink_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        daemon.event_sink = EventSink::from_writer(
            tmp.path().join("broken-events.jsonl").as_path(),
            FailingWriter,
        );

        let sent = Arc::new(Mutex::new(Vec::new()));
        daemon.channels.insert(
            "human".to_string(),
            Box::new(RecordingChannel {
                messages: Arc::clone(&sent),
            }),
        );

        daemon
            .queue_daemon_message("human", "Event sink can fail without breaking delivery.")
            .unwrap();

        assert_eq!(
            sent.lock().unwrap().as_slice(),
            ["Event sink can fail without breaking delivery."]
        );
    }

    #[test]
    fn drain_legacy_command_queue_preserves_failed_commands() {
        let tmp = tempfile::tempdir().unwrap();
        let queue_path = message::command_queue_path(tmp.path());
        message::enqueue_command(
            &queue_path,
            &message::QueuedCommand::Send {
                from: "architect".into(),
                to: "human".into(),
                message: "status".into(),
            },
        )
        .unwrap();
        message::enqueue_command(
            &queue_path,
            &message::QueuedCommand::Assign {
                from: "manager".into(),
                engineer: "eng-1".into(),
                task: "Task #7: recover".into(),
            },
        )
        .unwrap();

        let mut daemon = empty_legacy_daemon(&tmp);
        daemon.config.team_config.roles = vec![RoleDef {
            name: "human".to_string(),
            role_type: RoleType::User,
            agent: None,
            auth_mode: None,
            auth_env: vec![],
            instances: 1,
            prompt: None,
            talks_to: vec![],
            channel: Some("telegram".to_string()),
            channel_config: Some(test_channel_config("123", "fake")),
            nudge_interval_secs: None,
            receives_standup: None,
            standup_interval_secs: None,
            owns: Vec::new(),
            barrier_group: None,
            use_worktrees: false,
            ..Default::default()
        }];
        daemon.config.members = vec![engineer_member("eng-1", None, false)];
        daemon.channels = HashMap::from([(
            "human".to_string(),
            Box::new(FailingChannel) as Box<dyn Channel>,
        )]);

        daemon.drain_legacy_command_queue().unwrap();

        let remaining = message::read_command_queue(&queue_path).unwrap();
        assert_eq!(remaining.len(), 1);
        match &remaining[0] {
            message::QueuedCommand::Send { to, message, .. } => {
                assert_eq!(to, "human");
                assert_eq!(message, "status");
            }
            other => panic!("expected failed send command to remain queued, got {other:?}"),
        }

        let engineer_pending =
            inbox::pending_messages(&inbox::inboxes_root(tmp.path()), "eng-1").unwrap();
        assert_eq!(engineer_pending.len(), 1);
        assert_eq!(engineer_pending[0].from, "manager");
        assert!(engineer_pending[0].body.contains("Task #7: recover"));
    }

    #[test]
    fn deliver_inbox_messages_reports_failed_assignment_without_crashing() {
        let tmp = tempfile::tempdir().unwrap();
        let roles = vec![
            RoleDef {
                name: "manager".to_string(),
                role_type: RoleType::Manager,
                agent: Some("claude".to_string()),
                auth_mode: None,
                auth_env: vec![],
                instances: 1,
                prompt: None,
                talks_to: vec![],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                barrier_group: None,
                use_worktrees: false,
                ..Default::default()
            },
            RoleDef {
                name: "eng-1".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("claude".to_string()),
                auth_mode: None,
                auth_env: vec![],
                instances: 1,
                prompt: None,
                talks_to: vec![],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                barrier_group: None,
                use_worktrees: false,
                ..Default::default()
            },
        ];
        let members = vec![
            MemberInstance {
                name: "manager".to_string(),
                role_name: "manager".to_string(),
                role_type: RoleType::Manager,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: None,
                use_worktrees: false,
                ..Default::default()
            },
            MemberInstance {
                name: "eng-1".to_string(),
                role_name: "eng-1".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: Some("manager".to_string()),
                use_worktrees: false,
                ..Default::default()
            },
        ];

        let mut pane_map = HashMap::new();
        pane_map.insert("eng-1".to_string(), "%999".to_string());

        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: crate::team::config::TeamConfig {
                name: "test".to_string(),
                workspace_type: crate::team::config::WorkspaceType::Generic,
                trunk_branch: crate::team::config::default_trunk_branch(),
                agent: None,
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                external_senders: Vec::new(),
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                grafana: Default::default(),
                use_shim: false,
                use_sdk_mode: false,
                auto_respawn_on_crash: false,
                shim_health_check_interval_secs: 60,
                shim_health_timeout_secs: 120,
                shim_shutdown_timeout_secs: 30,
                shim_working_state_timeout_secs: 1800,
                pending_queue_max_age_secs: 600,
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles,
            },
            session: "test".to_string(),
            members,
            pane_map,
        })
        .unwrap();

        let root = inbox::inboxes_root(tmp.path());
        let assign = inbox::InboxMessage::new_assign("manager", "eng-1", "Task #13: fix it");
        let id = inbox::deliver_to_inbox(&root, &assign).unwrap();

        daemon.deliver_inbox_messages().unwrap();

        let engineer_pending = inbox::pending_messages(&root, "eng-1").unwrap();
        assert!(engineer_pending.is_empty());

        let engineer_all = inbox::all_messages(&root, "eng-1").unwrap();
        assert!(
            engineer_all
                .iter()
                .any(|(msg, delivered)| msg.id == id && *delivered)
        );

        let manager_pending = inbox::pending_messages(&root, "manager").unwrap();
        assert_eq!(manager_pending.len(), 1);
        assert_eq!(manager_pending[0].from, "daemon");
        assert!(manager_pending[0].body.contains("Assignment failed."));
        assert!(manager_pending[0].body.contains("Engineer: eng-1"));
        assert!(manager_pending[0].body.contains("Message ID:"));
        let result = crate::team::load_assignment_result(tmp.path(), &id)
            .unwrap()
            .unwrap();
        assert_eq!(result.status, AssignmentResultStatus::Failed);
        assert_eq!(result.engineer, "eng-1");
        assert_eq!(daemon.states.get("eng-1"), None);
    }

    #[test]
    fn queue_message_falls_back_to_inbox_when_live_delivery_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        daemon.config.members = vec![manager_member("manager", None)];
        daemon.config.pane_map = HashMap::from([("manager".to_string(), "%999".to_string())]);

        daemon
            .queue_message("eng-1", "manager", "Need review on merge handling.")
            .unwrap();

        let messages =
            inbox::pending_messages(&inbox::inboxes_root(tmp.path()), "manager").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].from, "eng-1");
        assert!(messages[0].body.contains("Need review on merge handling."));
    }

    #[test]
    fn external_sender_delivery() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);

        daemon.config.team_config.external_senders = vec!["email-router".to_string()];
        daemon.config.team_config.roles = vec![RoleDef {
            name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            auth_mode: None,
            auth_env: vec![],
            instances: 1,
            prompt: None,
            talks_to: vec![],
            channel: None,
            channel_config: None,
            nudge_interval_secs: None,
            receives_standup: None,
            standup_interval_secs: None,
            owns: Vec::new(),
            barrier_group: None,
            use_worktrees: false,
            ..Default::default()
        }];
        daemon.config.members = vec![MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        }];

        daemon
            .queue_message("email-router", "manager", "New email from user@example.com")
            .unwrap();

        let root = inbox::inboxes_root(tmp.path());
        let messages = inbox::pending_messages(&root, "manager").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].from, "email-router");
        assert!(messages[0].body.contains("New email from user@example.com"));

        assert!(
            daemon
                .config
                .team_config
                .can_talk("email-router", "manager")
        );
    }

    #[test]
    fn deliver_inbox_assignment_uses_existing_shim_instead_of_relaunch() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(
            tmp.path()
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks"),
        )
        .unwrap();
        let roles = vec![
            RoleDef {
                name: "manager".to_string(),
                role_type: RoleType::Manager,
                agent: Some("claude".to_string()),
                auth_mode: None,
                auth_env: vec![],
                instances: 1,
                prompt: None,
                talks_to: vec![],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                barrier_group: None,
                use_worktrees: false,
                ..Default::default()
            },
            RoleDef {
                name: "eng".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("codex".to_string()),
                auth_mode: None,
                auth_env: vec![],
                instances: 1,
                prompt: None,
                talks_to: vec![],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                barrier_group: None,
                use_worktrees: false,
                ..Default::default()
            },
        ];
        let members = vec![
            MemberInstance {
                name: "manager".to_string(),
                role_name: "manager".to_string(),
                role_type: RoleType::Manager,
                agent: Some("claude".to_string()),
                prompt: None,
                reports_to: None,
                use_worktrees: false,
                ..Default::default()
            },
            MemberInstance {
                name: "eng-1".to_string(),
                role_name: "eng".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("codex".to_string()),
                prompt: None,
                reports_to: Some("manager".to_string()),
                use_worktrees: false,
                ..Default::default()
            },
        ];

        let mut pane_map = HashMap::new();
        pane_map.insert("eng-1".to_string(), "%999".to_string());

        let mut daemon = TeamDaemon::new(DaemonConfig {
            project_root: tmp.path().to_path_buf(),
            team_config: crate::team::config::TeamConfig {
                name: "test".to_string(),
                workspace_type: crate::team::config::WorkspaceType::Generic,
                trunk_branch: crate::team::config::default_trunk_branch(),
                agent: None,
                workflow_mode: WorkflowMode::Legacy,
                workflow_policy: WorkflowPolicy::default(),
                board: BoardConfig::default(),
                standup: StandupConfig::default(),
                automation: AutomationConfig::default(),
                automation_sender: None,
                external_senders: Vec::new(),
                orchestrator_pane: true,
                orchestrator_position: OrchestratorPosition::Bottom,
                layout: None,
                cost: Default::default(),
                grafana: Default::default(),
                use_shim: true,
                use_sdk_mode: false,
                auto_respawn_on_crash: false,
                shim_health_check_interval_secs: 60,
                shim_health_timeout_secs: 120,
                shim_shutdown_timeout_secs: 30,
                shim_working_state_timeout_secs: 1800,
                pending_queue_max_age_secs: 600,
                event_log_max_bytes: crate::team::DEFAULT_EVENT_LOG_MAX_BYTES,
                retro_min_duration_secs: 60,
                roles,
            },
            session: "test".to_string(),
            members,
            pane_map,
        })
        .unwrap();

        let (parent_sock, child_sock) = crate::shim::protocol::socketpair().unwrap();
        let parent_channel = crate::shim::protocol::Channel::new(parent_sock);
        let mut child_channel = crate::shim::protocol::Channel::new(child_sock);
        let mut handle = crate::team::daemon::agent_handle::AgentHandle::new(
            "eng-1".to_string(),
            parent_channel,
            12345,
            "codex".to_string(),
            "codex".to_string(),
            tmp.path().to_path_buf(),
        );
        handle.apply_state_change(crate::shim::protocol::ShimState::Idle);
        daemon.shim_handles.insert("eng-1".to_string(), handle);
        daemon
            .shim_handles
            .get_mut("eng-1")
            .unwrap()
            .channel
            .set_read_timeout(Some(Duration::from_millis(5)))
            .unwrap();

        let root = inbox::inboxes_root(tmp.path());
        let assign = inbox::InboxMessage::new_assign("manager", "eng-1", "Task #13: fix it");
        let id = inbox::deliver_to_inbox(&root, &assign).unwrap();

        daemon.deliver_inbox_messages().unwrap();

        let cmd: crate::shim::protocol::Command = child_channel.recv().unwrap().unwrap();
        match cmd {
            crate::shim::protocol::Command::SendMessage { from, body, .. } => {
                assert_eq!(from, "manager");
                assert_eq!(body, "Task #13: fix it");
            }
            other => panic!("expected SendMessage, got {other:?}"),
        }

        let engineer_pending = inbox::pending_messages(&root, "eng-1").unwrap();
        assert!(engineer_pending.is_empty());

        let engineer_all = inbox::all_messages(&root, "eng-1").unwrap();
        assert!(
            engineer_all
                .iter()
                .any(|(msg, delivered)| msg.id == id && *delivered)
        );
        // Shim-managed agents: state driven by shim events, not speculative mark_member_working
        assert_ne!(daemon.states.get("eng-1"), Some(&MemberState::Working));
    }

    // --- Readiness gate tests ---

    #[test]
    fn deliver_inbox_skips_agents_not_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);

        let root = inbox::inboxes_root(tmp.path());
        let msg = inbox::InboxMessage::new_send("manager", "eng-1", "test assignment");
        inbox::deliver_to_inbox(&root, &msg).unwrap();

        let mut watcher = crate::team::watcher::SessionWatcher::new("%9999999", "eng-1", 300, None);
        watcher.activate();
        daemon.watchers.insert("eng-1".to_string(), watcher);

        daemon.deliver_inbox_messages().unwrap();

        let pending = inbox::pending_messages(&root, "eng-1").unwrap();
        assert_eq!(
            pending.len(),
            1,
            "message should remain pending for active agent"
        );
    }

    #[test]
    fn deliver_inbox_delivers_to_ready_agents() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);

        let root = inbox::inboxes_root(tmp.path());
        let msg = inbox::InboxMessage::new_send("manager", "eng-1", "test assignment");
        inbox::deliver_to_inbox(&root, &msg).unwrap();

        let mut watcher = crate::team::watcher::SessionWatcher::new("%9999999", "eng-1", 300, None);
        watcher.confirm_ready();
        daemon.watchers.insert("eng-1".to_string(), watcher);

        daemon.deliver_inbox_messages().unwrap();

        let pending = inbox::pending_messages(&root, "eng-1").unwrap();
        let _ = pending;
    }

    #[test]
    fn deliver_inbox_batches_low_signal_manager_notices() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        let root = inbox::inboxes_root(tmp.path());
        daemon
            .config
            .pane_map
            .insert("manager".to_string(), "%123".to_string());

        let first = inbox::InboxMessage::new_send(
            "architect",
            "manager",
            "Review backlog detected: direct-report work is waiting for your review.",
        );
        let second = inbox::InboxMessage::new_send(
            "architect",
            "manager",
            "Dispatch recovery needed: idle reports still have active work.",
        );
        inbox::deliver_to_inbox(&root, &first).unwrap();
        inbox::deliver_to_inbox(&root, &second).unwrap();

        let (parent_sock, child_sock) = crate::shim::protocol::socketpair().unwrap();
        let parent_channel = crate::shim::protocol::Channel::new(parent_sock);
        let mut child_channel = crate::shim::protocol::Channel::new(child_sock);
        let mut handle = crate::team::daemon::agent_handle::AgentHandle::new(
            "manager".to_string(),
            parent_channel,
            12345,
            "claude".to_string(),
            "claude".to_string(),
            tmp.path().to_path_buf(),
        );
        handle.apply_state_change(crate::shim::protocol::ShimState::Idle);
        daemon.shim_handles.insert("manager".to_string(), handle);
        daemon.states.insert(
            "manager".to_string(),
            crate::team::standup::MemberState::Idle,
        );
        daemon
            .shim_handles
            .get_mut("manager")
            .unwrap()
            .channel
            .set_read_timeout(Some(Duration::from_millis(5)))
            .unwrap();

        daemon.deliver_inbox_messages().unwrap();

        child_channel
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let first_cmd: crate::shim::protocol::Command = child_channel.recv().unwrap().unwrap();
        let digest_message_id = match first_cmd {
            crate::shim::protocol::Command::SendMessage {
                body, message_id, ..
            } => {
                assert!(body.contains("[manager-digest]"));
                assert!(body.contains("review [architect]"));
                // "Dispatch recovery needed: idle reports still have active work" is
                // now classified as IdleActiveRecovery (not DispatchGap), so the
                // digest entry label is "utilization" rather than "dispatch".
                assert!(body.contains("utilization [architect]"));
                assert!(body.contains("Review backlog detected"));
                assert!(body.contains("Dispatch recovery needed"));
                message_id.expect("digest delivery should carry a shim message id")
            }
            other => panic!("expected SendMessage, got {other:?}"),
        };

        let delivery = daemon.deliver_message(
            "architect",
            "manager",
            "Task #42: merge immediately and reply with the result.",
        );
        assert!(delivery.is_ok());
        assert_eq!(delivery.unwrap(), MessageDelivery::LivePane);

        let second_cmd: crate::shim::protocol::Command = child_channel.recv().unwrap().unwrap();
        match second_cmd {
            crate::shim::protocol::Command::SendMessage { body, .. } => {
                assert_eq!(
                    body,
                    "Task #42: merge immediately and reply with the result."
                );
            }
            other => panic!("expected SendMessage, got {other:?}"),
        }

        child_channel
            .send(&crate::shim::protocol::Event::MessageDelivered {
                id: digest_message_id,
            })
            .unwrap();
        daemon.poll_shim_handles().unwrap();

        assert!(
            inbox::pending_messages(&root, "manager")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn manager_digest_suppresses_duplicate_notices_and_records_telemetry() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        daemon.config.team_config.workflow_mode = WorkflowMode::Hybrid;
        let root = inbox::inboxes_root(tmp.path());
        daemon
            .config
            .pane_map
            .insert("manager".to_string(), "%123".to_string());

        for _ in 0..3 {
            let msg = inbox::InboxMessage::new_send(
                "architect",
                "manager",
                "Review backlog detected: direct-report work is waiting for your review.",
            );
            inbox::deliver_to_inbox(&root, &msg).unwrap();
        }

        let (parent_sock, child_sock) = crate::shim::protocol::socketpair().unwrap();
        let parent_channel = crate::shim::protocol::Channel::new(parent_sock);
        let mut child_channel = crate::shim::protocol::Channel::new(child_sock);
        let mut handle = crate::team::daemon::agent_handle::AgentHandle::new(
            "manager".to_string(),
            parent_channel,
            12345,
            "claude".to_string(),
            "claude".to_string(),
            tmp.path().to_path_buf(),
        );
        handle.apply_state_change(crate::shim::protocol::ShimState::Idle);
        daemon.shim_handles.insert("manager".to_string(), handle);
        daemon.states.insert(
            "manager".to_string(),
            crate::team::standup::MemberState::Idle,
        );

        daemon.deliver_inbox_messages().unwrap();

        child_channel
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let cmd: crate::shim::protocol::Command = child_channel.recv().unwrap().unwrap();
        match cmd {
            crate::shim::protocol::Command::SendMessage { body, .. } => {
                assert!(body.contains("[manager-digest]"));
                assert!(body.contains("pressure "));
                assert!(body.contains("2 duplicate(s) suppressed"));
                assert!(body.contains("review [architect x3]"));
            }
            other => panic!("expected SendMessage, got {other:?}"),
        }

        let log = std::fs::read_to_string(crate::team::orchestrator_log_path(tmp.path())).unwrap();
        assert!(log.contains("supervision digest: manager batched 3 notice(s)"));
        assert!(log.contains("duplicates suppressed: 2"));

        let events = std::fs::read_to_string(tmp.path().join("events.jsonl")).unwrap();
        assert!(events.contains("\"event\":\"supervisory_digest_emitted\""));
        assert!(events.contains("\"role\":\"manager\""));
        assert!(events.contains("notice_count=3 suppressed_duplicates=2"));
    }

    #[test]
    fn manager_digest_collapses_repeated_idle_nudges() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        daemon.config.team_config.workflow_mode = WorkflowMode::Hybrid;
        let root = inbox::inboxes_root(tmp.path());
        daemon
            .config
            .pane_map
            .insert("manager".to_string(), "%123".to_string());

        for _ in 0..2 {
            inbox::deliver_to_inbox(
                &root,
                &inbox::InboxMessage::new_send(
                    "daemon",
                    "manager",
                    "Idle nudge: you have been idle past your configured timeout.",
                ),
            )
            .unwrap();
        }

        let (parent_sock, child_sock) = crate::shim::protocol::socketpair().unwrap();
        let parent_channel = crate::shim::protocol::Channel::new(parent_sock);
        let mut child_channel = crate::shim::protocol::Channel::new(child_sock);
        let mut handle = crate::team::daemon::agent_handle::AgentHandle::new(
            "manager".to_string(),
            parent_channel,
            12345,
            "claude".to_string(),
            "claude".to_string(),
            tmp.path().to_path_buf(),
        );
        handle.apply_state_change(crate::shim::protocol::ShimState::Idle);
        daemon.shim_handles.insert("manager".to_string(), handle);
        daemon.states.insert(
            "manager".to_string(),
            crate::team::standup::MemberState::Idle,
        );

        daemon.deliver_inbox_messages().unwrap();

        child_channel
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let cmd: crate::shim::protocol::Command = child_channel.recv().unwrap().unwrap();
        match cmd {
            crate::shim::protocol::Command::SendMessage { body, .. } => {
                assert!(body.contains("[manager-digest]"));
                assert!(body.contains("pressure 0: low-signal only"));
                assert!(body.contains("1 duplicate(s) suppressed"));
                assert!(body.contains("recovery [daemon x2]"));
                assert!(
                    body.contains("Idle nudge: you have been idle past your configured timeout.")
                );
            }
            other => panic!("expected SendMessage, got {other:?}"),
        }

        let second: Result<Option<crate::shim::protocol::Command>, _> = child_channel.recv();
        assert!(
            second.is_err(),
            "repeated nudges should collapse into one digest"
        );
    }

    #[test]
    fn manager_digest_orders_review_and_utilization_ahead_of_status_noise() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        daemon.config.team_config.workflow_mode = WorkflowMode::Hybrid;
        let root = inbox::inboxes_root(tmp.path());
        daemon
            .config
            .pane_map
            .insert("manager".to_string(), "%123".to_string());

        inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send(
                "daemon",
                "manager",
                "Status update: triage queue is unchanged.",
            ),
        )
        .unwrap();
        inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send(
                "daemon",
                "manager",
                "Utilization recovery needed: 2 idle engineer(s) have no active task, 0 idle engineer(s) are still parked on active work, and 2 dispatchable task(s) are available. Top dispatchable items: #42 (high) Inbox fix; #43 (medium) Metrics cleanup.",
            ),
        )
        .unwrap();
        inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send(
                "daemon",
                "manager",
                "Review backlog detected: direct-report work is waiting for your review on Task #99.",
            ),
        )
        .unwrap();

        let (parent_sock, child_sock) = crate::shim::protocol::socketpair().unwrap();
        let parent_channel = crate::shim::protocol::Channel::new(parent_sock);
        let mut child_channel = crate::shim::protocol::Channel::new(child_sock);
        let mut handle = crate::team::daemon::agent_handle::AgentHandle::new(
            "manager".to_string(),
            parent_channel,
            12345,
            "claude".to_string(),
            "claude".to_string(),
            tmp.path().to_path_buf(),
        );
        handle.apply_state_change(crate::shim::protocol::ShimState::Idle);
        daemon.shim_handles.insert("manager".to_string(), handle);
        daemon.states.insert(
            "manager".to_string(),
            crate::team::standup::MemberState::Idle,
        );

        daemon.deliver_inbox_messages().unwrap();

        child_channel
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let cmd: crate::shim::protocol::Command = child_channel.recv().unwrap().unwrap();
        match cmd {
            crate::shim::protocol::Command::SendMessage { body, .. } => {
                let review_idx = body.find("review [daemon]").unwrap();
                let utilization_idx = body.find("utilization [daemon]").unwrap();
                let status_idx = body.find("status [daemon]").unwrap();
                assert!(review_idx < utilization_idx);
                assert!(utilization_idx < status_idx);
            }
            other => panic!("expected SendMessage, got {other:?}"),
        }
    }

    #[test]
    fn manager_digest_collapses_task_scoped_escalations_with_stable_key() {
        let messages = vec![
            inbox::InboxMessage::new_send(
                "daemon",
                "manager",
                "Escalation: task #77 is blocked on provider access.",
            ),
            inbox::InboxMessage::new_send(
                "daemon",
                "manager",
                "Escalation: task #77 is blocked on provider access after another retry.",
            ),
            inbox::InboxMessage::new_send(
                "daemon",
                "manager",
                "Escalation: task #88 is blocked on a different provider issue.",
            ),
        ];

        let digest = super::build_supervisory_digest(&messages);
        assert_eq!(digest.entries.len(), 2);
        assert_eq!(digest.duplicates_suppressed, 1);
        assert_eq!(digest.entries[0].duplicate_count, 2);
        assert!(digest.entries[0].preview.contains("task #77"));
    }

    #[test]
    fn manager_inbox_prioritizes_completion_notice_over_status_update() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        let root = inbox::inboxes_root(tmp.path());
        daemon
            .config
            .pane_map
            .insert("manager".to_string(), "%123".to_string());

        inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send(
                "architect",
                "manager",
                "Status update: triage queue is unchanged.",
            ),
        )
        .unwrap();
        inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send(
                "architect",
                "manager",
                "[eng-1] Task #42 passed tests but requires manual review.\nTitle: Inbox routing",
            ),
        )
        .unwrap();

        let (parent_sock, child_sock) = crate::shim::protocol::socketpair().unwrap();
        let parent_channel = crate::shim::protocol::Channel::new(parent_sock);
        let mut child_channel = crate::shim::protocol::Channel::new(child_sock);
        let mut handle = crate::team::daemon::agent_handle::AgentHandle::new(
            "manager".to_string(),
            parent_channel,
            12345,
            "claude".to_string(),
            "claude".to_string(),
            tmp.path().to_path_buf(),
        );
        handle.apply_state_change(crate::shim::protocol::ShimState::Idle);
        daemon.shim_handles.insert("manager".to_string(), handle);
        daemon.states.insert(
            "manager".to_string(),
            crate::team::standup::MemberState::Idle,
        );

        daemon.deliver_inbox_messages().unwrap();

        child_channel
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let first_cmd: crate::shim::protocol::Command = child_channel.recv().unwrap().unwrap();
        match first_cmd {
            crate::shim::protocol::Command::SendMessage { body, .. } => {
                assert!(body.contains("requires manual review"));
            }
            other => panic!("expected SendMessage, got {other:?}"),
        }
        let second_cmd: crate::shim::protocol::Command = child_channel.recv().unwrap().unwrap();
        match second_cmd {
            crate::shim::protocol::Command::SendMessage { body, .. } => {
                assert!(body.contains("Status update: triage queue is unchanged."));
            }
            other => panic!("expected SendMessage, got {other:?}"),
        }
    }

    #[test]
    fn manager_inbox_prioritizes_structured_completion_packet_over_status_update() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        let root = inbox::inboxes_root(tmp.path());
        daemon
            .config
            .pane_map
            .insert("manager".to_string(), "%123".to_string());

        inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send(
                "architect",
                "manager",
                "Status update: triage queue is unchanged.",
            ),
        )
        .unwrap();
        inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send(
                "eng-1",
                "manager",
                r#"Task complete.

```json
{"task_id":42,"branch":"eng-1/task-42","commit":"abc1234","tests_run":["cargo test"],"tests_passed":true,"outcome":"ready_for_review"}
```"#,
            ),
        )
        .unwrap();

        let (parent_sock, child_sock) = crate::shim::protocol::socketpair().unwrap();
        let parent_channel = crate::shim::protocol::Channel::new(parent_sock);
        let mut child_channel = crate::shim::protocol::Channel::new(child_sock);
        let mut handle = crate::team::daemon::agent_handle::AgentHandle::new(
            "manager".to_string(),
            parent_channel,
            12345,
            "claude".to_string(),
            "claude".to_string(),
            tmp.path().to_path_buf(),
        );
        handle.apply_state_change(crate::shim::protocol::ShimState::Idle);
        daemon.shim_handles.insert("manager".to_string(), handle);
        daemon.states.insert(
            "manager".to_string(),
            crate::team::standup::MemberState::Idle,
        );

        daemon.deliver_inbox_messages().unwrap();

        child_channel
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let first_cmd: crate::shim::protocol::Command = child_channel.recv().unwrap().unwrap();
        match first_cmd {
            crate::shim::protocol::Command::SendMessage { body, .. } => {
                assert!(body.contains("\"task_id\":42"));
                assert!(body.contains("\"outcome\":\"ready_for_review\""));
            }
            other => panic!("expected SendMessage, got {other:?}"),
        }
        let second_cmd: crate::shim::protocol::Command = child_channel.recv().unwrap().unwrap();
        match second_cmd {
            crate::shim::protocol::Command::SendMessage { body, .. } => {
                assert!(body.contains("Status update: triage queue is unchanged."));
            }
            other => panic!("expected SendMessage, got {other:?}"),
        }
    }

    #[test]
    fn completion_packet_delivery_adds_targeted_review_triage_nudge() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        let root = inbox::inboxes_root(tmp.path());
        daemon
            .config
            .pane_map
            .insert("manager".to_string(), "%123".to_string());

        let body = r#"## Completion Packet
```json
{"task_id":42,"branch":"eng-1/task-42","worktree_path":".batty/worktrees/eng-1","commit":"abc1234","changed_paths":["src/team/delivery/routing.rs"],"tests_run":true,"tests_passed":true,"artifacts":[],"outcome":"ready_for_review"}
```"#;
        let id = inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send("eng-1", "manager", body),
        )
        .unwrap();

        let mut child_channel = install_idle_test_shim(&mut daemon, &tmp, "manager");

        daemon.deliver_inbox_messages().unwrap();

        child_channel
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let cmd: crate::shim::protocol::Command = child_channel.recv().unwrap().unwrap();
        match cmd {
            crate::shim::protocol::Command::SendMessage { body, .. } => {
                assert!(body.starts_with("Review packet delivered"));
                assert!(body.contains("Task: #42"));
                assert!(body.contains("Engineer: eng-1"));
                assert!(body.contains("Branch: eng-1/task-42"));
                assert!(body.contains("Commit: abc1234"));
                assert!(body.contains("Tests: passed"));
                assert!(body.contains("Requested review action: ready_for_review"));
                assert!(body.contains("Packet contents:"));
                assert!(body.contains("\"task_id\":42"));
            }
            other => panic!("expected SendMessage, got {other:?}"),
        }

        assert!(
            daemon
                .intervention_cooldowns
                .contains_key(&format!("review_packet_triage:manager:{id}"))
        );
    }

    #[test]
    fn completion_packet_delivery_cooldown_prevents_repeated_triage_wrapper_for_same_packet() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        let root = inbox::inboxes_root(tmp.path());
        daemon
            .config
            .pane_map
            .insert("manager".to_string(), "%123".to_string());

        let body = r#"## Completion Packet
```json
{"task_id":42,"branch":"eng-1/task-42","worktree_path":".batty/worktrees/eng-1","commit":"abc1234","changed_paths":["src/team/delivery/routing.rs"],"tests_run":true,"tests_passed":true,"artifacts":[],"outcome":"ready_for_review"}
```"#;
        inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send("eng-1", "manager", body),
        )
        .unwrap();

        let mut child_channel = install_idle_test_shim(&mut daemon, &tmp, "manager");
        child_channel
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();

        daemon.deliver_inbox_messages().unwrap();
        let first_cmd: crate::shim::protocol::Command = child_channel.recv().unwrap().unwrap();
        match first_cmd {
            crate::shim::protocol::Command::SendMessage { body, .. } => {
                assert!(body.starts_with("Review packet delivered"));
            }
            other => panic!("expected SendMessage, got {other:?}"),
        }

        daemon.pending_shim_acks.clear();
        daemon
            .shim_handles
            .get_mut("manager")
            .unwrap()
            .apply_state_change(crate::shim::protocol::ShimState::Idle);

        daemon.deliver_inbox_messages().unwrap();
        let second_cmd: crate::shim::protocol::Command = child_channel.recv().unwrap().unwrap();
        match second_cmd {
            crate::shim::protocol::Command::SendMessage { body, .. } => {
                assert!(
                    !body.starts_with("Review packet delivered"),
                    "same packet should not receive another triage wrapper while cooldown is active"
                );
                assert!(body.starts_with("## Completion Packet"));
                assert!(body.contains("\"task_id\":42"));
            }
            other => panic!("expected SendMessage, got {other:?}"),
        }
    }

    #[test]
    fn manager_inbox_defers_low_signal_digest_until_completion_is_acked() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        let root = inbox::inboxes_root(tmp.path());
        daemon
            .config
            .pane_map
            .insert("manager".to_string(), "%123".to_string());

        inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send(
                "architect",
                "manager",
                "Status update: triage queue is unchanged.",
            ),
        )
        .unwrap();
        inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send(
                "architect",
                "manager",
                "Review backlog detected: direct-report work is waiting for your review.",
            ),
        )
        .unwrap();
        inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send(
                "architect",
                "manager",
                "[eng-1] Task #42 passed tests but requires manual review.\nTitle: Inbox routing",
            ),
        )
        .unwrap();

        let (parent_sock, child_sock) = crate::shim::protocol::socketpair().unwrap();
        let parent_channel = crate::shim::protocol::Channel::new(parent_sock);
        let mut child_channel = crate::shim::protocol::Channel::new(child_sock);
        let mut handle = crate::team::daemon::agent_handle::AgentHandle::new(
            "manager".to_string(),
            parent_channel,
            12345,
            "claude".to_string(),
            "claude".to_string(),
            tmp.path().to_path_buf(),
        );
        handle.apply_state_change(crate::shim::protocol::ShimState::Idle);
        daemon.shim_handles.insert("manager".to_string(), handle);
        daemon.states.insert(
            "manager".to_string(),
            crate::team::standup::MemberState::Idle,
        );
        daemon
            .shim_handles
            .get_mut("manager")
            .unwrap()
            .channel
            .set_read_timeout(Some(Duration::from_millis(5)))
            .unwrap();

        daemon.deliver_inbox_messages().unwrap();

        child_channel
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let first_cmd: crate::shim::protocol::Command = child_channel.recv().unwrap().unwrap();
        let completion_message_id = match first_cmd {
            crate::shim::protocol::Command::SendMessage {
                body, message_id, ..
            } => {
                assert!(body.contains("requires manual review"));
                message_id.expect("completion delivery should carry a shim message id")
            }
            other => panic!("expected SendMessage, got {other:?}"),
        };

        child_channel
            .set_read_timeout(Some(Duration::from_millis(25)))
            .unwrap();
        let second_cmd: Result<Option<crate::shim::protocol::Command>, _> = child_channel.recv();
        assert!(
            second_cmd.is_err(),
            "digest should wait until the completion delivery is acknowledged"
        );

        child_channel
            .send(&crate::shim::protocol::Event::MessageDelivered {
                id: completion_message_id,
            })
            .unwrap();
        child_channel
            .send(&crate::shim::protocol::Event::StateChanged {
                from: crate::shim::protocol::ShimState::Working,
                to: crate::shim::protocol::ShimState::Idle,
                summary: "ready".to_string(),
            })
            .unwrap();
        daemon.poll_shim_handles().unwrap();
        daemon.states.insert(
            "manager".to_string(),
            crate::team::standup::MemberState::Idle,
        );

        daemon.deliver_inbox_messages().unwrap();

        child_channel
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let digest_cmd: crate::shim::protocol::Command = child_channel.recv().unwrap().unwrap();
        match digest_cmd {
            crate::shim::protocol::Command::SendMessage { body, .. } => {
                assert!(body.contains("[manager-digest]"));
                assert!(body.contains("review [architect]"));
                assert!(body.contains("status [architect]"));
            }
            other => panic!("expected SendMessage, got {other:?}"),
        }
    }

    #[test]
    fn manager_inbox_suppresses_duplicate_escalations_within_cooldown() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        let root = inbox::inboxes_root(tmp.path());
        daemon
            .config
            .pane_map
            .insert("manager".to_string(), "%123".to_string());

        let escalation = "ESCALATION: Task #42 assigned to eng-1 has unresolvable merge conflicts. Task blocked on board.";
        let recent = inbox::InboxMessage::new_send("architect", "manager", escalation);
        let recent_id = inbox::deliver_to_inbox(&root, &recent).unwrap();
        inbox::mark_delivered(&root, "manager", &recent_id).unwrap();

        inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send("architect", "manager", escalation),
        )
        .unwrap();
        inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send(
                "architect",
                "manager",
                "[eng-1] Task #42 passed tests but requires manual review.\nTitle: Inbox routing",
            ),
        )
        .unwrap();

        let (parent_sock, child_sock) = crate::shim::protocol::socketpair().unwrap();
        let parent_channel = crate::shim::protocol::Channel::new(parent_sock);
        let mut child_channel = crate::shim::protocol::Channel::new(child_sock);
        let mut handle = crate::team::daemon::agent_handle::AgentHandle::new(
            "manager".to_string(),
            parent_channel,
            12345,
            "claude".to_string(),
            "claude".to_string(),
            tmp.path().to_path_buf(),
        );
        handle.apply_state_change(crate::shim::protocol::ShimState::Idle);
        daemon.shim_handles.insert("manager".to_string(), handle);
        daemon.states.insert(
            "manager".to_string(),
            crate::team::standup::MemberState::Idle,
        );

        daemon.deliver_inbox_messages().unwrap();

        child_channel
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let cmd: crate::shim::protocol::Command = child_channel.recv().unwrap().unwrap();
        match cmd {
            crate::shim::protocol::Command::SendMessage { body, .. } => {
                assert!(body.contains("requires manual review"));
            }
            other => panic!("expected SendMessage, got {other:?}"),
        }
        let second: Result<Option<crate::shim::protocol::Command>, _> = child_channel.recv();
        assert!(
            second.is_err(),
            "duplicate escalation should be suppressed instead of redelivered"
        );
    }

    #[test]
    fn deliver_inbox_messages_uses_shim_readiness_not_console_watcher_state() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);

        let root = inbox::inboxes_root(tmp.path());
        let msg = inbox::InboxMessage::new_send("manager", "eng-1", "test assignment");
        let id = inbox::deliver_to_inbox(&root, &msg).unwrap();

        let mut watcher = crate::team::watcher::SessionWatcher::new("%9999999", "eng-1", 300, None);
        watcher.activate();
        daemon.watchers.insert("eng-1".to_string(), watcher);

        let (parent_sock, child_sock) = crate::shim::protocol::socketpair().unwrap();
        let parent_channel = crate::shim::protocol::Channel::new(parent_sock);
        let mut child_channel = crate::shim::protocol::Channel::new(child_sock);
        let mut handle = crate::team::daemon::agent_handle::AgentHandle::new(
            "eng-1".to_string(),
            parent_channel,
            12345,
            "claude".to_string(),
            "claude".to_string(),
            tmp.path().to_path_buf(),
        );
        handle.apply_state_change(crate::shim::protocol::ShimState::Idle);
        daemon.shim_handles.insert("eng-1".to_string(), handle);
        daemon
            .shim_handles
            .get_mut("eng-1")
            .unwrap()
            .channel
            .set_read_timeout(Some(Duration::from_millis(5)))
            .unwrap();

        daemon.deliver_inbox_messages().unwrap();

        let pending = inbox::pending_messages(&root, "eng-1").unwrap();
        assert_eq!(
            pending.len(),
            1,
            "message should stay pending until shim ack"
        );
        assert_eq!(pending[0].id, id);

        child_channel
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let cmd: crate::shim::protocol::Command = child_channel.recv().unwrap().unwrap();
        match cmd {
            crate::shim::protocol::Command::SendMessage {
                from,
                body,
                message_id,
            } => {
                assert_eq!(from, "manager");
                assert_eq!(body, "test assignment");
                assert_eq!(message_id.as_deref(), Some(id.as_str()));
            }
            other => panic!("expected SendMessage, got {other:?}"),
        }

        child_channel
            .send(&crate::shim::protocol::Event::MessageDelivered { id: id.clone() })
            .unwrap();
        daemon.poll_shim_handles().unwrap();

        let pending = inbox::pending_messages(&root, "eng-1").unwrap();
        assert!(
            pending.is_empty(),
            "message should be marked delivered after shim ack"
        );

        let all = inbox::all_messages(&root, "eng-1").unwrap();
        assert!(
            all.iter()
                .any(|(msg, delivered)| msg.id == id && *delivered)
        );
    }

    #[test]
    fn deliver_inbox_messages_uses_daemon_idle_state_when_shim_handle_is_stale() {
        // When the shim handle reports Working but the daemon state says Idle,
        // member_ready_for_delivery returns true and the message should be
        // delivered via the shim channel regardless of handle state.
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);

        let root = inbox::inboxes_root(tmp.path());
        let msg = inbox::InboxMessage::new_send("manager", "eng-1", "test assignment");
        let id = inbox::deliver_to_inbox(&root, &msg).unwrap();

        let (parent_sock, child_sock) = crate::shim::protocol::socketpair().unwrap();
        let parent_channel = crate::shim::protocol::Channel::new(parent_sock);
        let child_channel = crate::shim::protocol::Channel::new(child_sock);
        let mut handle = crate::team::daemon::agent_handle::AgentHandle::new(
            "eng-1".to_string(),
            parent_channel,
            12345,
            "claude".to_string(),
            "claude".to_string(),
            tmp.path().to_path_buf(),
        );
        handle.apply_state_change(crate::shim::protocol::ShimState::Working);
        daemon.shim_handles.insert("eng-1".to_string(), handle);
        daemon
            .states
            .insert("eng-1".to_string(), crate::team::standup::MemberState::Idle);
        daemon
            .shim_handles
            .get_mut("eng-1")
            .unwrap()
            .channel
            .set_read_timeout(Some(Duration::from_millis(5)))
            .unwrap();

        daemon.deliver_inbox_messages().unwrap();

        let pending = inbox::pending_messages(&root, "eng-1").unwrap();
        assert_eq!(
            pending.len(),
            1,
            "message should stay pending until shim ack"
        );
        assert_eq!(pending[0].id, id);

        let mut child_channel = child_channel;
        child_channel
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let cmd: Result<Option<crate::shim::protocol::Command>, _> = child_channel.recv();
        match cmd {
            Ok(Some(crate::shim::protocol::Command::SendMessage {
                from,
                body,
                message_id,
            })) => {
                assert_eq!(from, "manager");
                assert_eq!(body, "test assignment");
                assert_eq!(message_id.as_deref(), Some(id.as_str()));
            }
            Ok(Some(other)) => panic!("expected SendMessage, got {other:?}"),
            Ok(None) => panic!("channel closed unexpectedly"),
            Err(error) => panic!("expected shim delivery, got {error}"),
        }

        child_channel
            .send(&crate::shim::protocol::Event::MessageDelivered { id: id.clone() })
            .unwrap();
        daemon.poll_shim_handles().unwrap();

        let pending = inbox::pending_messages(&root, "eng-1").unwrap();
        assert!(
            pending.is_empty(),
            "message should be marked delivered after shim ack"
        );
        let all = inbox::all_messages(&root, "eng-1").unwrap();
        assert!(all.iter().any(|(m, delivered)| m.id == id && *delivered));
    }

    // --- Delivery to unknown recipient ---

    #[test]
    fn deliver_message_skips_unknown_recipient() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        let result = daemon
            .deliver_message("manager", "nonexistent-role", "hello")
            .unwrap();
        assert_eq!(result, MessageDelivery::SkippedUnknownRecipient);
    }

    // --- Delivery to member without pane falls back to inbox ---

    #[test]
    fn deliver_message_to_member_without_pane_goes_to_inbox() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        daemon.config.members.push(MemberInstance {
            name: "eng-2".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
            ..Default::default()
        });

        let result = daemon
            .deliver_message("manager", "eng-2", "Go fix the bug")
            .unwrap();
        assert_eq!(result, MessageDelivery::InboxQueued);

        let root = inbox::inboxes_root(tmp.path());
        let messages = inbox::pending_messages(&root, "eng-2").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].from, "manager");
        assert!(messages[0].body.contains("Go fix the bug"));
    }

    // --- Queue daemon message uses automation sender ---

    #[test]
    fn queue_daemon_message_to_unknown_skips() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = empty_legacy_daemon(&tmp);
        let result = daemon.queue_daemon_message("nobody", "test msg").unwrap();
        assert_eq!(result, MessageDelivery::SkippedUnknownRecipient);
    }

    // --- Resolve role name ---

    #[test]
    fn resolve_role_name_returns_human_for_human() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = failed_delivery_test_daemon(&tmp);
        assert_eq!(daemon.resolve_role_name("human"), "human");
    }

    #[test]
    fn resolve_role_name_returns_daemon_for_daemon() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = failed_delivery_test_daemon(&tmp);
        assert_eq!(daemon.resolve_role_name("daemon"), "daemon");
    }

    #[test]
    fn resolve_role_name_maps_member_to_role_name() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = failed_delivery_test_daemon(&tmp);
        assert_eq!(daemon.resolve_role_name("eng-1"), "eng");
    }

    #[test]
    fn resolve_role_name_returns_input_for_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = failed_delivery_test_daemon(&tmp);
        assert_eq!(daemon.resolve_role_name("unknown-member"), "unknown-member");
    }

    // --- Pending delivery queue tests (Task #276) ---

    #[test]
    fn pending_queue_buffers_message_when_agent_not_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        let watcher = crate::team::watcher::SessionWatcher::new("%9999999", "eng-1", 300, None);
        assert!(!watcher.is_ready_for_delivery());
        daemon.watchers.insert("eng-1".to_string(), watcher);

        // Insert a shim handle in Starting state so the pending queue path is triggered
        let (parent, _child) = crate::shim::protocol::socketpair().unwrap();
        let channel = crate::shim::protocol::Channel::new(parent);
        let handle = crate::team::daemon::agent_handle::AgentHandle::new(
            "eng-1".into(),
            channel,
            999,
            "codex".into(),
            "codex".into(),
            std::path::PathBuf::from("/tmp/test"),
        );
        daemon.shim_handles.insert("eng-1".to_string(), handle);

        let result = daemon
            .deliver_message("manager", "eng-1", "task assignment")
            .unwrap();

        assert_eq!(
            result,
            MessageDelivery::DeferredPending,
            "message to starting agent must be deferred to pending queue"
        );
        let queue = daemon.pending_delivery_queue.get("eng-1").unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].from, "manager");
        assert_eq!(queue[0].body, "task assignment");
        assert!(
            queue[0].message_id.is_some(),
            "deferred shim delivery should preserve a message id"
        );
    }

    #[test]
    fn drain_pending_queue_delivers_when_agent_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);

        daemon
            .pending_delivery_queue
            .entry("eng-1".to_string())
            .or_default()
            .push(PendingMessage {
                from: "manager".to_string(),
                body: "queued assignment".to_string(),
                message_id: None,
                queued_at: Instant::now(),
            });

        let mut watcher = crate::team::watcher::SessionWatcher::new("%9999999", "eng-1", 300, None);
        watcher.confirm_ready();
        daemon.watchers.insert("eng-1".to_string(), watcher);

        daemon.drain_pending_queue("eng-1").unwrap();

        assert!(
            daemon
                .pending_delivery_queue
                .get("eng-1")
                .map(|q| q.is_empty())
                .unwrap_or(true),
            "pending queue must be empty after drain"
        );

        let root = inbox::inboxes_root(tmp.path());
        let messages = inbox::pending_messages(&root, "eng-1").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].body, "queued assignment");
    }

    #[test]
    fn drain_pending_queue_noop_when_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        daemon.drain_pending_queue("eng-1").unwrap();
        assert!(
            daemon
                .pending_delivery_queue
                .get("eng-1")
                .map(|q| q.is_empty())
                .unwrap_or(true)
        );
    }

    #[test]
    fn multiple_messages_queued_and_drained_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);

        let watcher = crate::team::watcher::SessionWatcher::new("%9999999", "eng-1", 300, None);
        daemon.watchers.insert("eng-1".to_string(), watcher);

        // Insert a shim handle in Starting state so the pending queue path is triggered
        let (parent, _child) = crate::shim::protocol::socketpair().unwrap();
        let channel = crate::shim::protocol::Channel::new(parent);
        let handle = crate::team::daemon::agent_handle::AgentHandle::new(
            "eng-1".into(),
            channel,
            999,
            "codex".into(),
            "codex".into(),
            std::path::PathBuf::from("/tmp/test"),
        );
        daemon.shim_handles.insert("eng-1".to_string(), handle);

        for i in 1..=3u32 {
            let result = daemon
                .deliver_message("manager", "eng-1", &format!("msg-{i}"))
                .unwrap();
            assert_eq!(result, MessageDelivery::DeferredPending);
        }

        let queue = daemon.pending_delivery_queue.get("eng-1").unwrap();
        assert_eq!(queue.len(), 3);
        assert_eq!(queue[0].body, "msg-1");
        assert_eq!(queue[1].body, "msg-2");
        assert_eq!(queue[2].body, "msg-3");

        daemon.watchers.get_mut("eng-1").unwrap().confirm_ready();
        // Remove the shim handle so drain falls through to inbox delivery
        daemon.shim_handles.remove("eng-1");
        daemon.drain_pending_queue("eng-1").unwrap();

        assert!(
            daemon
                .pending_delivery_queue
                .get("eng-1")
                .map(|q| q.is_empty())
                .unwrap_or(true)
        );

        let root = inbox::inboxes_root(tmp.path());
        let inbox_msgs = inbox::pending_messages(&root, "eng-1").unwrap();
        assert_eq!(inbox_msgs.len(), 3, "all queued messages must be delivered");
        let mut bodies: Vec<&str> = inbox_msgs.iter().map(|m| m.body.as_str()).collect();
        bodies.sort();
        assert_eq!(bodies, vec!["msg-1", "msg-2", "msg-3"]);
    }

    // --- Full pending queue lifecycle test (#289) ---

    #[test]
    fn pending_queue_full_lifecycle_buffer_transition_drain_verify() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);

        let watcher = crate::team::watcher::SessionWatcher::new("%9999999", "eng-1", 300, None);
        assert!(!watcher.is_ready_for_delivery());
        daemon.watchers.insert("eng-1".to_string(), watcher);

        // Insert a shim handle in Starting state so the pending queue path is triggered
        let (parent, _child) = crate::shim::protocol::socketpair().unwrap();
        let channel = crate::shim::protocol::Channel::new(parent);
        let handle = crate::team::daemon::agent_handle::AgentHandle::new(
            "eng-1".into(),
            channel,
            999,
            "codex".into(),
            "codex".into(),
            std::path::PathBuf::from("/tmp/test"),
        );
        daemon.shim_handles.insert("eng-1".to_string(), handle);

        let result = daemon
            .deliver_message("manager", "eng-1", "Task #42: implement feature")
            .unwrap();
        assert_eq!(
            result,
            MessageDelivery::DeferredPending,
            "message to starting agent must be deferred"
        );
        let queue = daemon.pending_delivery_queue.get("eng-1").unwrap();
        assert_eq!(
            queue.len(),
            1,
            "pending queue must contain exactly one message"
        );
        assert_eq!(queue[0].from, "manager");
        assert_eq!(queue[0].body, "Task #42: implement feature");

        daemon.watchers.get_mut("eng-1").unwrap().confirm_ready();
        assert!(
            daemon
                .watchers
                .get("eng-1")
                .unwrap()
                .is_ready_for_delivery()
        );

        // Remove shim handle so drain falls through to inbox delivery
        daemon.shim_handles.remove("eng-1");
        daemon.drain_pending_queue("eng-1").unwrap();

        assert!(
            daemon
                .pending_delivery_queue
                .get("eng-1")
                .map(|q| q.is_empty())
                .unwrap_or(true),
            "pending queue must be empty after drain"
        );

        let root = inbox::inboxes_root(tmp.path());
        let inbox_msgs = inbox::pending_messages(&root, "eng-1").unwrap();
        assert_eq!(
            inbox_msgs.len(),
            1,
            "message must arrive in inbox after drain"
        );
        assert_eq!(inbox_msgs[0].body, "Task #42: implement feature");
        assert_eq!(inbox_msgs[0].from, "manager");
    }

    // -- Shim delivery tests --

    #[test]
    fn shim_delivery_sends_via_channel_when_ready() {
        let tmp = tempfile::tempdir().unwrap();
        inbox::init_inbox(&inbox::inboxes_root(tmp.path()), "eng-1").unwrap();

        let mut daemon = empty_legacy_daemon(&tmp);
        daemon.config.team_config.use_shim = true;

        // Create a shim handle in idle state
        let (parent, child) = crate::shim::protocol::socketpair().unwrap();
        let channel = crate::shim::protocol::Channel::new(parent);
        let mut handle = crate::team::daemon::agent_handle::AgentHandle::new(
            "eng-1".into(),
            channel,
            999,
            "claude".into(),
            "claude".into(),
            std::path::PathBuf::from("/tmp/test"),
        );
        handle.apply_state_change(crate::shim::protocol::ShimState::Idle);
        daemon.shim_handles.insert("eng-1".to_string(), handle);

        let result = daemon.deliver_message("manager", "eng-1", "do the thing");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), MessageDelivery::LivePane);

        // Verify the command arrived on the child side
        let cmd: crate::shim::protocol::Command = crate::shim::protocol::Channel::new(child)
            .recv()
            .unwrap()
            .unwrap();
        match cmd {
            crate::shim::protocol::Command::SendMessage {
                from,
                body,
                message_id,
            } => {
                assert_eq!(from, "manager");
                assert_eq!(body, "do the thing");
                assert!(
                    message_id.is_some(),
                    "live shim delivery should carry a message id"
                );
            }
            _ => panic!("expected SendMessage"),
        }

        // Shim-managed agents: delivery does not force handle or daemon state.
        // The shim classifier is the single source of truth for agent state.
        assert_eq!(
            daemon.shim_handles["eng-1"].state,
            crate::shim::protocol::ShimState::Idle,
            "delivery should not force handle state to Working"
        );
        assert_ne!(daemon.states.get("eng-1"), Some(&MemberState::Working));
    }

    #[test]
    fn shim_delivery_defers_when_not_ready() {
        let tmp = tempfile::tempdir().unwrap();
        inbox::init_inbox(&inbox::inboxes_root(tmp.path()), "eng-1").unwrap();

        let mut daemon = empty_legacy_daemon(&tmp);
        daemon.config.team_config.use_shim = true;

        // Create a shim handle still in Starting state
        let (parent, _child) = crate::shim::protocol::socketpair().unwrap();
        let channel = crate::shim::protocol::Channel::new(parent);
        let handle = crate::team::daemon::agent_handle::AgentHandle::new(
            "eng-1".into(),
            channel,
            999,
            "claude".into(),
            "claude".into(),
            std::path::PathBuf::from("/tmp/test"),
        );
        daemon.shim_handles.insert("eng-1".to_string(), handle);

        let result = daemon.deliver_message("manager", "eng-1", "wait for me");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), MessageDelivery::DeferredPending);

        let queue = daemon.pending_delivery_queue.get("eng-1").unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].body, "wait for me");
    }

    #[test]
    fn shim_delivery_used_regardless_of_use_shim_flag() {
        let tmp = tempfile::tempdir().unwrap();
        inbox::init_inbox(&inbox::inboxes_root(tmp.path()), "eng-1").unwrap();

        let mut daemon = empty_legacy_daemon(&tmp);
        // use_shim defaults to false — shim delivery still attempted if handle exists

        let (parent, _child) = crate::shim::protocol::socketpair().unwrap();
        let channel = crate::shim::protocol::Channel::new(parent);
        let mut handle = crate::team::daemon::agent_handle::AgentHandle::new(
            "eng-1".into(),
            channel,
            999,
            "claude".into(),
            "claude".into(),
            std::path::PathBuf::from("/tmp/test"),
        );
        handle.apply_state_change(crate::shim::protocol::ShimState::Idle);
        daemon.shim_handles.insert("eng-1".to_string(), handle);

        // Shim delivery is always attempted when a handle exists, regardless of use_shim flag
        let result = daemon.deliver_message("manager", "eng-1", "hello");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), MessageDelivery::LivePane);
    }

    #[test]
    fn shim_delivery_diverts_nudges_to_orchestrator_log() {
        let tmp = tempfile::tempdir().unwrap();
        inbox::init_inbox(&inbox::inboxes_root(tmp.path()), "eng-1").unwrap();

        let mut daemon = empty_legacy_daemon(&tmp);
        daemon.config.team_config.workflow_mode = WorkflowMode::Hybrid;
        daemon.config.team_config.roles = vec![
            RoleDef {
                name: "manager".to_string(),
                role_type: RoleType::Manager,
                agent: Some("claude".to_string()),
                auth_mode: None,
                auth_env: vec![],
                instances: 1,
                prompt: None,
                talks_to: vec![],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                barrier_group: None,
                use_worktrees: false,
                ..Default::default()
            },
            RoleDef {
                name: "engineer".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("claude".to_string()),
                auth_mode: None,
                auth_env: vec![],
                instances: 1,
                prompt: None,
                talks_to: vec![],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                barrier_group: None,
                use_worktrees: false,
                ..Default::default()
            },
        ];
        daemon.config.members = vec![MemberInstance {
            name: "eng-1".to_string(),
            role_name: "engineer".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
            ..Default::default()
        }];
        daemon
            .config
            .pane_map
            .insert("eng-1".to_string(), "%999".to_string());

        let (parent, child) = crate::shim::protocol::socketpair().unwrap();
        let channel = crate::shim::protocol::Channel::new(parent);
        let mut handle = crate::team::daemon::agent_handle::AgentHandle::new(
            "eng-1".into(),
            channel,
            999,
            "claude".into(),
            "claude".into(),
            std::path::PathBuf::from("/tmp/test"),
        );
        handle.apply_state_change(crate::shim::protocol::ShimState::Idle);
        daemon.shim_handles.insert("eng-1".to_string(), handle);

        let result = daemon.deliver_message(
            "daemon",
            "eng-1",
            "Idle nudge: you have been idle past your configured timeout.",
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), MessageDelivery::OrchestratorLogged);

        let mut receiver = crate::shim::protocol::Channel::new(child);
        receiver
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        assert!(
            receiver.recv::<crate::shim::protocol::Command>().is_err(),
            "nudge should never be injected into the agent shim"
        );

        let log = std::fs::read_to_string(crate::team::orchestrator_log_path(tmp.path())).unwrap();
        assert!(log.contains("notification isolation: diverted nudge"));
        assert!(log.contains("eng-1"));
    }

    #[test]
    fn deliver_inbox_messages_answers_status_queries_from_cached_state() {
        let tmp = tempfile::tempdir().unwrap();
        inbox::init_inbox(&inbox::inboxes_root(tmp.path()), "eng-1").unwrap();

        let mut daemon = empty_legacy_daemon(&tmp);
        daemon.config.team_config.workflow_mode = WorkflowMode::Hybrid;
        daemon.config.team_config.roles = vec![
            RoleDef {
                name: "manager".to_string(),
                role_type: RoleType::Manager,
                agent: Some("claude".to_string()),
                auth_mode: None,
                auth_env: vec![],
                instances: 1,
                prompt: None,
                talks_to: vec![],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                barrier_group: None,
                use_worktrees: false,
                ..Default::default()
            },
            RoleDef {
                name: "engineer".to_string(),
                role_type: RoleType::Engineer,
                agent: Some("claude".to_string()),
                auth_mode: None,
                auth_env: vec![],
                instances: 1,
                prompt: None,
                talks_to: vec![],
                channel: None,
                channel_config: None,
                nudge_interval_secs: None,
                receives_standup: None,
                standup_interval_secs: None,
                owns: Vec::new(),
                barrier_group: None,
                use_worktrees: false,
                ..Default::default()
            },
        ];
        daemon.config.members = vec![MemberInstance {
            name: "eng-1".to_string(),
            role_name: "engineer".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
            ..Default::default()
        }];
        daemon
            .config
            .pane_map
            .insert("eng-1".to_string(), "%999".to_string());
        daemon
            .states
            .insert("eng-1".to_string(), crate::team::standup::MemberState::Idle);

        let (parent, child) = crate::shim::protocol::socketpair().unwrap();
        let channel = crate::shim::protocol::Channel::new(parent);
        let mut handle = crate::team::daemon::agent_handle::AgentHandle::new(
            "eng-1".into(),
            channel,
            999,
            "claude".into(),
            "claude".into(),
            std::path::PathBuf::from("/tmp/test"),
        );
        handle.apply_state_change(crate::shim::protocol::ShimState::Idle);
        daemon.shim_handles.insert("eng-1".to_string(), handle);

        let root = inbox::inboxes_root(tmp.path());
        let msg = inbox::InboxMessage::new_send("manager", "eng-1", "status?");
        let id = inbox::deliver_to_inbox(&root, &msg).unwrap();
        assert!(daemon.config.team_config.can_talk("manager", "engineer"));
        assert_eq!(
            daemon.orchestrator_only_reason("eng-1", "status?"),
            Some(OrchestratorOnlyReason::StatusQuery)
        );

        daemon.deliver_inbox_messages().unwrap();

        let pending = inbox::pending_messages(&root, "eng-1").unwrap();
        assert!(pending.is_empty());
        let all = inbox::all_messages(&root, "eng-1").unwrap();
        assert!(
            all.iter()
                .any(|(message, delivered)| message.id == id && *delivered)
        );

        let mut receiver = crate::shim::protocol::Channel::new(child);
        receiver
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        assert!(
            receiver.recv::<crate::shim::protocol::Command>().is_err(),
            "status query should be answered from cached state, not injected"
        );

        let log = std::fs::read_to_string(crate::team::orchestrator_log_path(tmp.path())).unwrap();
        assert!(
            log.contains("answered status query from manager about eng-1"),
            "log contents: {log}"
        );
        assert!(log.contains("idle"));
        assert!(log.contains("watcher"));
    }

    #[test]
    fn deliver_message_diverts_standup_requests_to_orchestrator_log() {
        let tmp = tempfile::tempdir().unwrap();
        inbox::init_inbox(&inbox::inboxes_root(tmp.path()), "eng-1").unwrap();

        let mut daemon = empty_legacy_daemon(&tmp);
        daemon.config.team_config.workflow_mode = WorkflowMode::Hybrid;
        daemon.config.members = vec![MemberInstance {
            name: "eng-1".to_string(),
            role_name: "engineer".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
            ..Default::default()
        }];
        daemon
            .config
            .pane_map
            .insert("eng-1".to_string(), "%999".to_string());

        let (parent, child) = crate::shim::protocol::socketpair().unwrap();
        let channel = crate::shim::protocol::Channel::new(parent);
        let mut handle = crate::team::daemon::agent_handle::AgentHandle::new(
            "eng-1".into(),
            channel,
            999,
            "claude".into(),
            "claude".into(),
            std::path::PathBuf::from("/tmp/test"),
        );
        handle.apply_state_change(crate::shim::protocol::ShimState::Idle);
        daemon.shim_handles.insert("eng-1".to_string(), handle);

        let result = daemon.deliver_message("manager", "eng-1", "standup?");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), MessageDelivery::OrchestratorLogged);

        let mut receiver = crate::shim::protocol::Channel::new(child);
        receiver
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        assert!(
            receiver.recv::<crate::shim::protocol::Command>().is_err(),
            "standup request should stay out of the agent PTY"
        );

        let log = std::fs::read_to_string(crate::team::orchestrator_log_path(tmp.path())).unwrap();
        assert!(log.contains("answered standup request from manager about eng-1"));
    }

    #[test]
    fn deliver_message_diverts_non_actionable_engineer_chatter() {
        let tmp = tempfile::tempdir().unwrap();
        inbox::init_inbox(&inbox::inboxes_root(tmp.path()), "eng-1").unwrap();

        let mut daemon = empty_legacy_daemon(&tmp);
        daemon.config.team_config.workflow_mode = WorkflowMode::Hybrid;
        daemon.config.members = vec![MemberInstance {
            name: "eng-1".to_string(),
            role_name: "engineer".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
            ..Default::default()
        }];
        daemon
            .config
            .pane_map
            .insert("eng-1".to_string(), "%999".to_string());

        let (parent, child) = crate::shim::protocol::socketpair().unwrap();
        let channel = crate::shim::protocol::Channel::new(parent);
        let mut handle = crate::team::daemon::agent_handle::AgentHandle::new(
            "eng-1".into(),
            channel,
            999,
            "claude".into(),
            "claude".into(),
            std::path::PathBuf::from("/tmp/test"),
        );
        handle.apply_state_change(crate::shim::protocol::ShimState::Idle);
        daemon.shim_handles.insert("eng-1".to_string(), handle);

        let result = daemon.deliver_message(
            "manager",
            "eng-1",
            "Rollup: review backlog is healthy and no action is required right now.",
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), MessageDelivery::OrchestratorLogged);

        let mut receiver = crate::shim::protocol::Channel::new(child);
        receiver
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        assert!(
            receiver.recv::<crate::shim::protocol::Command>().is_err(),
            "non-actionable chatter should stay out of the engineer PTY"
        );

        let log = std::fs::read_to_string(crate::team::orchestrator_log_path(tmp.path())).unwrap();
        assert!(log.contains("diverted non-actionable engineer notice"));
    }

    #[test]
    fn deliver_inbox_diverts_stale_review_normalization_from_engineer_context() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        daemon.config.team_config.workflow_mode = WorkflowMode::Hybrid;
        let mut receiver = install_idle_test_shim(&mut daemon, &tmp, "eng-1");
        receiver
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();

        let root = inbox::inboxes_root(tmp.path());
        let body = "Stale review normalization: task #42 has been stuck in review for 7200s.\n\
                    Task: already merged upstream\n\
                    Reason: branch has no commits ahead of main.\n\
                    Next step: normalize review backlog.";
        let first_id = inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send("manager", "eng-1", body),
        )
        .unwrap();
        let second_id = inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send("manager", "eng-1", body),
        )
        .unwrap();

        daemon.deliver_inbox_messages().unwrap();

        assert!(
            receiver.recv::<crate::shim::protocol::Command>().is_err(),
            "stale review normalization chatter should not enter engineer shim context"
        );
        assert!(inbox::pending_messages(&root, "eng-1").unwrap().is_empty());
        let all = inbox::all_messages(&root, "eng-1").unwrap();
        assert!(
            all.iter()
                .any(|(message, delivered)| message.id == first_id && *delivered),
            "filtered first message should remain visible in raw inbox history"
        );
        assert!(
            all.iter()
                .any(|(message, delivered)| message.id == second_id && *delivered),
            "filtered duplicate should remain visible in raw inbox history"
        );

        let log = std::fs::read_to_string(crate::team::orchestrator_log_path(tmp.path())).unwrap();
        assert!(log.contains("diverted non-actionable engineer notice"));
        assert!(log.contains("Stale review normalization"));
    }

    #[test]
    fn deliver_inbox_keeps_stale_review_normalization_visible_to_manager() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = failed_delivery_test_daemon(&tmp);
        daemon
            .config
            .pane_map
            .insert("manager".to_string(), "%123".to_string());
        let mut receiver = install_idle_test_shim(&mut daemon, &tmp, "manager");
        receiver
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        let root = inbox::inboxes_root(tmp.path());
        let body = "Stale review normalization: task #42 has been stuck in review for 7200s.\n\
                    Task: already merged upstream\n\
                    Reason: branch has no commits ahead of main.\n\
                    Next step: normalize review backlog.";
        inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send("architect", "manager", body),
        )
        .unwrap();

        daemon.deliver_inbox_messages().unwrap();

        let cmd = receiver
            .recv::<crate::shim::protocol::Command>()
            .unwrap()
            .unwrap();
        match cmd {
            crate::shim::protocol::Command::SendMessage {
                body: delivered, ..
            } => {
                assert!(delivered.contains("Stale review normalization"));
                assert!(delivered.contains("normalize review backlog"));
            }
            other => panic!("expected SendMessage, got {other:?}"),
        }
    }

    #[test]
    fn deliver_inbox_suppresses_stale_done_task_notice_after_merge() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "routing-stale-done");
        git_ok(&repo, &["branch", "eng-1-2/683"]);
        write_board_task_file(&repo, 683, "merged-task", "done", None, &[], None);

        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(vec![
                architect_member("architect"),
                manager_member("manager", Some("architect")),
            ])
            .pane_map(HashMap::from([(
                "manager".to_string(),
                "%manager".to_string(),
            )]))
            .build();
        daemon.config.team_config.workflow_mode = WorkflowMode::Hybrid;
        let mut receiver = install_idle_test_shim(&mut daemon, &tmp, "manager");
        receiver
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();

        let body = "Branch/task mismatch detected for task #683. Current branch: \
                    'eng-main/eng-1-3'. Expected branch: 'eng-1-2/683'. \
                    Automatic recovery is paused.";
        let root = inbox::inboxes_root(&repo);
        let first_id = inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send("architect", "manager", body),
        )
        .unwrap();
        let second_id = inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send("architect", "manager", body),
        )
        .unwrap();

        daemon.deliver_inbox_messages().unwrap();

        assert!(
            receiver.recv::<crate::shim::protocol::Command>().is_err(),
            "merged done-task notices should stay out of manager context"
        );
        assert!(
            inbox::pending_messages(&root, "manager")
                .unwrap()
                .is_empty()
        );
        let all = inbox::all_messages(&root, "manager").unwrap();
        assert!(
            all.iter()
                .any(|(message, delivered)| message.id == first_id && *delivered)
        );
        assert!(
            all.iter()
                .any(|(message, delivered)| message.id == second_id && *delivered)
        );

        let events =
            std::fs::read_to_string(repo.join(".batty").join("team_config").join("events.jsonl"))
                .unwrap();
        assert_eq!(events.matches("stale_done_notice_suppressed").count(), 1);

        let log = std::fs::read_to_string(crate::team::orchestrator_log_path(&repo)).unwrap();
        assert!(log.contains("stale done notice suppressed"));
        assert!(log.contains("task #683"));
    }

    #[test]
    fn deliver_inbox_suppresses_reconciliation_instead_of_done_task_notice() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "routing-stale-done-instead-of");
        git_ok(&repo, &["branch", "eng-1-2/683"]);
        write_board_task_file(&repo, 683, "merged-task", "done", None, &[], None);

        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(vec![
                architect_member("architect"),
                manager_member("manager", Some("architect")),
            ])
            .pane_map(HashMap::from([(
                "manager".to_string(),
                "%manager".to_string(),
            )]))
            .build();
        daemon.config.team_config.workflow_mode = WorkflowMode::Hybrid;
        let mut receiver = install_idle_test_shim(&mut daemon, &tmp, "manager");
        receiver
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();

        let body = "Reconciliation alert: eng-1-3 is claimed on task #683 but the worktree is on \
                    'eng-main/eng-1-3' instead of 'eng-1-2/683'. Batty will refuse new \
                    assignment until the branch is corrected manually.";
        let root = inbox::inboxes_root(&repo);
        inbox::deliver_to_inbox(
            &root,
            &inbox::InboxMessage::new_send("architect", "manager", body),
        )
        .unwrap();

        daemon.deliver_inbox_messages().unwrap();

        assert!(
            receiver.recv::<crate::shim::protocol::Command>().is_err(),
            "done-task reconciliation alerts using instead-of wording should be suppressed"
        );
        assert!(
            inbox::pending_messages(&root, "manager")
                .unwrap()
                .is_empty()
        );
        let events =
            std::fs::read_to_string(repo.join(".batty").join("team_config").join("events.jsonl"))
                .unwrap();
        assert!(events.contains("stale_done_notice_suppressed"));
    }

    #[test]
    fn deliver_message_uses_configured_trunk_for_done_task_proof() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "routing-stale-done-mainline");
        git_ok(&repo, &["checkout", "-b", "mainline"]);
        std::fs::write(repo.join("src").join("mainline.rs"), "pub fn trunk() {}\n").unwrap();
        git_ok(&repo, &["add", "."]);
        git_ok(&repo, &["commit", "-m", "mainline trunk proof"]);
        git_ok(&repo, &["branch", "eng-1-2/685"]);
        git_ok(&repo, &["checkout", "main"]);
        write_board_task_file(&repo, 685, "merged-mainline-task", "done", None, &[], None);

        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(vec![
                architect_member("architect"),
                manager_member("manager", Some("architect")),
            ])
            .pane_map(HashMap::from([(
                "manager".to_string(),
                "%manager".to_string(),
            )]))
            .build();
        daemon.config.team_config.workflow_mode = WorkflowMode::Hybrid;
        daemon.config.team_config.trunk_branch = "mainline".to_string();
        let mut receiver = install_idle_test_shim(&mut daemon, &tmp, "manager");
        receiver
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();

        let body = "Review urgency: task #685 has been in review for 7200s.\n\
                    Expected branch: 'eng-1-2/685'.\n\
                    Next step: merge it, request rework, or escalate immediately.";
        let result = daemon
            .deliver_message("architect", "manager", body)
            .unwrap();

        assert_eq!(result, MessageDelivery::OrchestratorLogged);
        assert!(
            receiver.recv::<crate::shim::protocol::Command>().is_err(),
            "configured-trunk done-task notices should stay out of manager context"
        );

        let events =
            std::fs::read_to_string(repo.join(".batty").join("team_config").join("events.jsonl"))
                .unwrap();
        assert!(events.contains("stale_done_notice_suppressed"));
    }

    #[test]
    fn deliver_message_suppresses_reconciliation_should_be_on_done_task() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "routing-stale-reconciliation");
        git_ok(&repo, &["branch", "eng-1-2/683"]);
        write_board_task_file(&repo, 683, "merged-task", "done", None, &[], None);

        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(vec![
                architect_member("architect"),
                manager_member("manager", Some("architect")),
            ])
            .pane_map(HashMap::from([(
                "architect".to_string(),
                "%architect".to_string(),
            )]))
            .build();
        daemon.config.team_config.workflow_mode = WorkflowMode::Hybrid;
        let mut receiver = install_idle_test_shim(&mut daemon, &tmp, "architect");
        receiver
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();

        let body = "Reconciliation alert: eng-1-3 is claimed on task #683 but should be on \
                    eng-1-2/683. Batty will refuse new assignment until corrected.";
        let result = daemon
            .deliver_message("manager", "architect", body)
            .unwrap();

        assert_eq!(result, MessageDelivery::OrchestratorLogged);
        assert!(
            receiver.recv::<crate::shim::protocol::Command>().is_err(),
            "stale reconciliation alerts should stay out of architect context"
        );

        let events =
            std::fs::read_to_string(repo.join(".batty").join("team_config").join("events.jsonl"))
                .unwrap();
        assert!(events.contains("stale_done_notice_suppressed"));

        let log = std::fs::read_to_string(crate::team::orchestrator_log_path(&repo)).unwrap();
        assert!(log.contains("stale done notice suppressed"));
        assert!(log.contains("eng-1-2/683"));
    }

    #[test]
    fn deliver_message_keeps_unmerged_done_task_notice_visible() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "routing-unmerged-done");
        git_ok(&repo, &["checkout", "-b", "eng-1-2/684"]);
        std::fs::write(
            repo.join("src").join("unmerged.rs"),
            "pub fn unmerged() {}\n",
        )
        .unwrap();
        git_ok(&repo, &["add", "."]);
        git_ok(&repo, &["commit", "-m", "unmerged task work"]);
        git_ok(&repo, &["checkout", "main"]);
        write_board_task_file(&repo, 684, "unmerged-task", "done", None, &[], None);

        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(vec![
                architect_member("architect"),
                manager_member("manager", Some("architect")),
            ])
            .pane_map(HashMap::from([(
                "manager".to_string(),
                "%manager".to_string(),
            )]))
            .build();
        daemon.config.team_config.workflow_mode = WorkflowMode::Hybrid;
        let mut receiver = install_idle_test_shim(&mut daemon, &tmp, "manager");
        receiver
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        let body = "Branch/task mismatch detected for task #684. Current branch: \
                    'eng-main/eng-1-3'. Expected branch: 'eng-1-2/684'. \
                    Automatic recovery is paused.";
        let result = daemon
            .deliver_message("architect", "manager", body)
            .unwrap();

        assert_eq!(result, MessageDelivery::LivePane);
        let cmd = receiver
            .recv::<crate::shim::protocol::Command>()
            .unwrap()
            .unwrap();
        match cmd {
            crate::shim::protocol::Command::SendMessage {
                body: delivered, ..
            } => assert!(delivered.contains("Branch/task mismatch detected for task #684")),
            other => panic!("expected SendMessage, got {other:?}"),
        }
    }

    #[test]
    fn deliver_message_keeps_engineer_retry_feedback_inline() {
        let tmp = tempfile::tempdir().unwrap();
        inbox::init_inbox(&inbox::inboxes_root(tmp.path()), "eng-1").unwrap();

        let mut daemon = empty_legacy_daemon(&tmp);
        daemon.config.team_config.workflow_mode = WorkflowMode::Hybrid;
        daemon.config.members = vec![MemberInstance {
            name: "eng-1".to_string(),
            role_name: "engineer".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
            ..Default::default()
        }];
        daemon
            .config
            .pane_map
            .insert("eng-1".to_string(), "%999".to_string());

        let (parent, child) = crate::shim::protocol::socketpair().unwrap();
        let channel = crate::shim::protocol::Channel::new(parent);
        let mut handle = crate::team::daemon::agent_handle::AgentHandle::new(
            "eng-1".into(),
            channel,
            999,
            "claude".into(),
            "claude".into(),
            std::path::PathBuf::from("/tmp/test"),
        );
        handle.apply_state_change(crate::shim::protocol::ShimState::Idle);
        daemon.shim_handles.insert("eng-1".to_string(), handle);

        let body = "Verification rejected this completion because there are no commits ahead of main.\nFix attempt 1/3.\nFix these failures, then report completion again.";
        let result = daemon.deliver_message("batty", "eng-1", body);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), MessageDelivery::LivePane);

        let mut receiver = crate::shim::protocol::Channel::new(child);
        receiver
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let cmd = receiver.recv::<crate::shim::protocol::Command>().unwrap();
        match cmd {
            Some(crate::shim::protocol::Command::SendMessage {
                from,
                body: delivered,
                ..
            }) => {
                assert_eq!(from, "batty");
                assert_eq!(delivered, body);
            }
            other => panic!("expected SendMessage, got {other:?}"),
        }
    }

    #[test]
    fn deliver_message_keeps_engineer_rework_inline() {
        let tmp = tempfile::tempdir().unwrap();
        inbox::init_inbox(&inbox::inboxes_root(tmp.path()), "eng-1").unwrap();

        let mut daemon = failed_delivery_test_daemon(&tmp);
        let mut receiver = install_idle_test_shim(&mut daemon, &tmp, "eng-1");
        receiver
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        let body =
            "Task #42: rework required.\nFix the failing routing test and report completion again.";
        let result = daemon.deliver_message("manager", "eng-1", body);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), MessageDelivery::LivePane);

        let cmd = receiver
            .recv::<crate::shim::protocol::Command>()
            .unwrap()
            .unwrap();
        match cmd {
            crate::shim::protocol::Command::SendMessage {
                from,
                body: delivered,
                ..
            } => {
                assert_eq!(from, "manager");
                assert_eq!(delivered, body);
            }
            other => panic!("expected SendMessage, got {other:?}"),
        }
    }

    // ── expire_stale_pending_messages tests ──

    #[test]
    fn expire_stale_pending_messages_noop_when_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        daemon.config.team_config.pending_queue_max_age_secs = 0;

        daemon
            .pending_delivery_queue
            .entry("eng-1".to_string())
            .or_default()
            .push(PendingMessage {
                from: "architect".into(),
                body: "hello".into(),
                message_id: None,
                queued_at: Instant::now() - Duration::from_secs(9999),
            });

        daemon.expire_stale_pending_messages().unwrap();

        // Should still be in the queue since expiry is disabled
        assert!(daemon.pending_delivery_queue.contains_key("eng-1"));
    }

    #[test]
    fn expire_stale_pending_messages_keeps_fresh_messages() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        daemon.config.team_config.pending_queue_max_age_secs = 600;

        daemon
            .pending_delivery_queue
            .entry("eng-1".to_string())
            .or_default()
            .push(PendingMessage {
                from: "architect".into(),
                body: "recent message".into(),
                message_id: None,
                queued_at: Instant::now(),
            });

        daemon.expire_stale_pending_messages().unwrap();

        assert!(
            daemon.pending_delivery_queue.contains_key("eng-1"),
            "fresh messages should remain in pending queue"
        );
    }

    #[test]
    fn expire_stale_pending_messages_expires_old_to_inbox() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        let inbox_root = crate::team::inbox::inboxes_root(tmp.path());
        crate::team::inbox::init_inbox(&inbox_root, "eng-1").unwrap();

        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        daemon.config.team_config.pending_queue_max_age_secs = 60;

        daemon
            .pending_delivery_queue
            .entry("eng-1".to_string())
            .or_default()
            .push(PendingMessage {
                from: "architect".into(),
                body: "stale message".into(),
                message_id: None,
                queued_at: Instant::now() - Duration::from_secs(120),
            });

        daemon.expire_stale_pending_messages().unwrap();

        // Pending queue should be empty
        assert!(
            !daemon.pending_delivery_queue.contains_key("eng-1"),
            "expired message should be removed from pending queue"
        );

        // Message should have been delivered to inbox
        let messages = crate::team::inbox::pending_messages(&inbox_root, "eng-1").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].body, "stale message");
        assert_eq!(messages[0].from, "architect");
    }

    #[test]
    fn expire_stale_pending_messages_mixed_ages() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        let inbox_root = crate::team::inbox::inboxes_root(tmp.path());
        crate::team::inbox::init_inbox(&inbox_root, "eng-1").unwrap();

        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        daemon.config.team_config.pending_queue_max_age_secs = 60;

        let queue = daemon
            .pending_delivery_queue
            .entry("eng-1".to_string())
            .or_default();
        queue.push(PendingMessage {
            from: "architect".into(),
            body: "old message".into(),
            message_id: None,
            queued_at: Instant::now() - Duration::from_secs(120),
        });
        queue.push(PendingMessage {
            from: "manager".into(),
            body: "new message".into(),
            message_id: None,
            queued_at: Instant::now(),
        });

        daemon.expire_stale_pending_messages().unwrap();

        // Only the fresh message should remain
        assert!(daemon.pending_delivery_queue.contains_key("eng-1"));
        let remaining = &daemon.pending_delivery_queue["eng-1"];
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].body, "new message");

        // Old message should be in inbox
        let messages = crate::team::inbox::pending_messages(&inbox_root, "eng-1").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].body, "old message");
    }

    #[test]
    fn expire_stale_pending_messages_digests_many_from_same_sender() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        let inbox_root = crate::team::inbox::inboxes_root(tmp.path());
        crate::team::inbox::init_inbox(&inbox_root, "manager").unwrap();

        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        daemon.config.team_config.pending_queue_max_age_secs = 60;

        let queue = daemon
            .pending_delivery_queue
            .entry("manager".to_string())
            .or_default();
        // 5 messages from architect (above digest threshold of 3)
        for i in 0..5 {
            queue.push(PendingMessage {
                from: "architect".into(),
                body: format!("escalation message {i}"),
                message_id: None,
                queued_at: Instant::now() - Duration::from_secs(120),
            });
        }
        // 2 messages from daemon (below threshold — delivered individually)
        for i in 0..2 {
            queue.push(PendingMessage {
                from: "daemon".into(),
                body: format!("daemon alert {i}"),
                message_id: None,
                queued_at: Instant::now() - Duration::from_secs(120),
            });
        }

        daemon.expire_stale_pending_messages().unwrap();

        assert!(!daemon.pending_delivery_queue.contains_key("manager"));

        let messages = crate::team::inbox::pending_messages(&inbox_root, "manager").unwrap();
        // Should get: 1 digest from architect + 2 individual from daemon = 3 messages
        assert_eq!(
            messages.len(),
            3,
            "expected 1 digest + 2 individual messages, got: {:?}",
            messages
                .iter()
                .map(|m| (&m.from, &m.body))
                .collect::<Vec<_>>()
        );

        let digest = messages
            .iter()
            .find(|m| m.body.contains("[digest]"))
            .expect("should have a digest message");
        assert_eq!(digest.from, "architect");
        assert!(digest.body.contains("5 messages from architect"));
        assert!(digest.body.contains("escalation message"));

        let daemon_msgs: Vec<_> = messages.iter().filter(|m| m.from == "daemon").collect();
        assert_eq!(daemon_msgs.len(), 2);
    }
}
