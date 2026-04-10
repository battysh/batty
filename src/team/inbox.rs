//! Maildir-based inbox messaging system.
//!
//! Each team member gets a Maildir at `.batty/inboxes/<member>/` with
//! `new/`, `cur/`, `tmp/` subdirectories. Messages are JSON blobs stored
//! atomically via the `maildir` crate.
//!
//! - `new/` — undelivered messages (daemon picks these up)
//! - `cur/` — delivered messages (moved here after tmux injection)
//! - `tmp/` — atomic write staging (managed by `maildir` crate)

use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use maildir::Maildir;
use serde::{Deserialize, Serialize};

/// A message stored in a member's inbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxMessage {
    /// Unique message ID (assigned by maildir filename, not serialized in body).
    #[serde(skip)]
    pub id: String,
    /// Sender name (e.g., "human", "architect", "manager-1").
    pub from: String,
    /// Recipient name.
    pub to: String,
    /// Message body text.
    pub body: String,
    /// Message type: "send" or "assign".
    pub msg_type: MessageType,
    /// Unix timestamp (seconds since epoch).
    pub timestamp: u64,
}

/// Type of inbox message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MessageType {
    Send,
    Assign,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InboxPurgeSummary {
    pub roles: usize,
    pub messages: usize,
}

impl InboxMessage {
    /// Create a new send-type message.
    pub fn new_send(from: &str, to: &str, body: &str) -> Self {
        Self {
            id: String::new(),
            from: from.to_string(),
            to: to.to_string(),
            body: body.to_string(),
            msg_type: MessageType::Send,
            timestamp: now_unix(),
        }
    }

    /// Create a new assign-type message.
    pub fn new_assign(from: &str, to: &str, task: &str) -> Self {
        Self {
            id: String::new(),
            from: from.to_string(),
            to: to.to_string(),
            body: task.to_string(),
            msg_type: MessageType::Assign,
            timestamp: now_unix(),
        }
    }

    /// Serialize to JSON bytes for storage.
    pub fn to_json_bytes(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).context("failed to serialize inbox message")
    }

    /// Deserialize from JSON bytes read from a maildir file.
    pub fn from_json_bytes(data: &[u8], id: &str) -> Result<Self> {
        let mut msg: Self =
            serde_json::from_slice(data).context("failed to deserialize inbox message")?;
        msg.id = id.to_string();
        Ok(msg)
    }

    /// Return how long the message has been in the inbox.
    pub fn age(&self) -> Duration {
        Duration::from_secs(now_unix().saturating_sub(self.timestamp))
    }
}

/// Resolve the inboxes root directory: `.batty/inboxes/`.
pub fn inboxes_root(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("inboxes")
}

/// Get the Maildir for a specific member.
fn member_maildir(inboxes_root: &Path, member: &str) -> Maildir {
    Maildir::from(inboxes_root.join(member))
}

/// Initialize a member's inbox (create `new/`, `cur/`, `tmp/` dirs).
pub fn init_inbox(inboxes_root: &Path, member: &str) -> Result<()> {
    let md = member_maildir(inboxes_root, member);
    md.create_dirs()
        .with_context(|| format!("failed to create inbox dirs for '{member}'"))?;
    Ok(())
}

/// Deliver a message to a member's inbox.
///
/// The message is atomically written to `new/` via the maildir crate
/// (write to `tmp/`, rename to `new/`). Returns the maildir message ID.
pub fn deliver_to_inbox(inboxes_root: &Path, msg: &InboxMessage) -> Result<String> {
    let md = member_maildir(inboxes_root, &msg.to);
    // Ensure dirs exist (idempotent)
    md.create_dirs()
        .with_context(|| format!("failed to create inbox dirs for '{}'", msg.to))?;
    let data = msg.to_json_bytes()?;
    let id = md
        .store_new(&data)
        .with_context(|| format!("failed to store message in inbox for '{}'", msg.to))?;
    Ok(id)
}

/// Read recent messages for a member across both pending and delivered states.
pub fn read_recent_messages(
    inboxes_root: &Path,
    member: &str,
    max_age: Duration,
) -> Result<Vec<InboxMessage>> {
    let cutoff = now_unix().saturating_sub(max_age.as_secs());
    let mut messages: Vec<InboxMessage> = all_messages(inboxes_root, member)?
        .into_iter()
        .map(|(message, _)| message)
        .filter(|message| message.timestamp >= cutoff)
        .collect();
    messages.sort_by_key(|message| message.timestamp);
    Ok(messages)
}

/// Produce a stable signature for duplicate detection.
pub fn message_signature(body: &str) -> u64 {
    let normalized = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let preview: String = normalized.chars().take(200).collect();
    let mut hasher = DefaultHasher::new();
    preview.hash(&mut hasher);
    hasher.finish()
}

/// Return the most recent duplicate message seen within the provided window.
pub fn find_recent_duplicate(
    inboxes_root: &Path,
    member: &str,
    new_msg: &InboxMessage,
    max_age: Duration,
) -> Result<Option<InboxMessage>> {
    let signature = message_signature(&new_msg.body);
    let duplicate = read_recent_messages(inboxes_root, member, max_age)?
        .into_iter()
        .rev()
        .find(|existing| {
            existing.from == new_msg.from
                && existing.msg_type == new_msg.msg_type
                && message_signature(&existing.body) == signature
        });
    Ok(duplicate)
}

/// Expire pending messages older than the provided age by marking them delivered.
pub fn expire_stale_pending_messages(
    inboxes_root: &Path,
    member: &str,
    max_age: Duration,
) -> Result<Vec<InboxMessage>> {
    let mut expired = Vec::new();
    for message in pending_messages(inboxes_root, member)? {
        if message.age() > max_age {
            mark_delivered(inboxes_root, member, &message.id)?;
            expired.push(message);
        }
    }
    Ok(expired)
}

/// List all pending (undelivered) messages in a member's inbox.
///
/// These are messages in `new/` that haven't been delivered to the agent yet.
pub fn pending_messages(inboxes_root: &Path, member: &str) -> Result<Vec<InboxMessage>> {
    let md = member_maildir(inboxes_root, member);
    let mut messages = Vec::new();

    for entry in md.list_new() {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(member, error = %e, "skipping unreadable inbox entry");
                continue;
            }
        };
        let id = entry.id().to_string();
        let data = match std::fs::read(entry.path()) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(member, id = %id, error = %e, "failed to read inbox message");
                continue;
            }
        };
        match InboxMessage::from_json_bytes(&data, &id) {
            Ok(msg) => messages.push(msg),
            Err(e) => {
                tracing::warn!(member, id = %id, error = %e, "skipping malformed inbox message");
            }
        }
    }

    // Sort by timestamp (oldest first for FIFO delivery)
    messages.sort_by_key(|m| m.timestamp);
    Ok(messages)
}

/// Count undelivered messages in `new/` for a member.
pub fn pending_message_count(inboxes_root: &Path, member: &str) -> Result<usize> {
    let new_dir = inboxes_root.join(member).join("new");
    if !new_dir.is_dir() {
        return Ok(0);
    }

    let mut count = 0usize;
    for entry in std::fs::read_dir(&new_dir)
        .with_context(|| format!("failed to read {}", new_dir.display()))?
    {
        let entry = entry.with_context(|| format!("failed to read {}", new_dir.display()))?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?;
        if file_type.is_file() {
            count += 1;
        }
    }

    Ok(count)
}

/// Mark a message as delivered (move from `new/` to `cur/`).
pub fn mark_delivered(inboxes_root: &Path, member: &str, id: &str) -> Result<()> {
    let md = member_maildir(inboxes_root, member);
    md.move_new_to_cur(id)
        .with_context(|| format!("failed to mark message '{id}' as delivered for '{member}'"))?;
    Ok(())
}

/// List all messages (both pending and delivered) for a member.
pub fn all_messages(inboxes_root: &Path, member: &str) -> Result<Vec<(InboxMessage, bool)>> {
    let md = member_maildir(inboxes_root, member);
    let mut messages = Vec::new();

    // new/ = pending (not yet delivered)
    for entry in md.list_new() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let id = entry.id().to_string();
        let data = match std::fs::read(entry.path()) {
            Ok(d) => d,
            Err(_) => continue,
        };
        if let Ok(msg) = InboxMessage::from_json_bytes(&data, &id) {
            messages.push((msg, false)); // false = not delivered
        }
    }

    // cur/ = delivered
    for entry in md.list_cur() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let id = entry.id().to_string();
        let data = match std::fs::read(entry.path()) {
            Ok(d) => d,
            Err(_) => continue,
        };
        if let Ok(msg) = InboxMessage::from_json_bytes(&data, &id) {
            messages.push((msg, true)); // true = delivered
        }
    }

    messages.sort_by_key(|(m, _)| m.timestamp);
    Ok(messages)
}

/// Delete a message from a member's inbox (from either new/ or cur/).
pub fn delete_message(inboxes_root: &Path, member: &str, id: &str) -> Result<()> {
    let md = member_maildir(inboxes_root, member);
    md.delete(id)
        .with_context(|| format!("failed to delete message '{id}' from '{member}' inbox"))?;
    Ok(())
}

/// Purge delivered messages from a member inbox.
pub fn purge_delivered_messages(
    inboxes_root: &Path,
    member: &str,
    before: Option<u64>,
    purge_all: bool,
) -> Result<usize> {
    let cur_dir = inboxes_root.join(member).join("cur");
    if !cur_dir.is_dir() {
        return Ok(0);
    }

    let mut removed = 0usize;
    for entry in
        fs::read_dir(&cur_dir).with_context(|| format!("failed to read {}", cur_dir.display()))?
    {
        let entry = entry.with_context(|| format!("failed to read {}", cur_dir.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if !file_type.is_file() {
            continue;
        }

        let should_delete = if purge_all {
            true
        } else if let Some(cutoff) = before {
            let data = match fs::read(&path) {
                Ok(data) => data,
                Err(_) => continue,
            };
            let Some(id) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            match InboxMessage::from_json_bytes(&data, id) {
                Ok(message) => message.timestamp < cutoff,
                Err(_) => false,
            }
        } else {
            false
        };

        if should_delete {
            fs::remove_file(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
            removed += 1;
        }
    }

    Ok(removed)
}

/// Purge delivered messages from every member inbox under `.batty/inboxes/`.
pub fn purge_delivered_messages_for_all(
    inboxes_root: &Path,
    before: Option<u64>,
    purge_all: bool,
) -> Result<InboxPurgeSummary> {
    if !inboxes_root.is_dir() {
        return Ok(InboxPurgeSummary {
            roles: 0,
            messages: 0,
        });
    }

    let mut roles = 0usize;
    let mut messages = 0usize;
    for entry in fs::read_dir(inboxes_root)
        .with_context(|| format!("failed to read {}", inboxes_root.display()))?
    {
        let entry = entry.with_context(|| format!("failed to read {}", inboxes_root.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if !file_type.is_dir() {
            continue;
        }

        let Some(member) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        roles += 1;
        messages += purge_delivered_messages(inboxes_root, member, before, purge_all)?;
    }

    Ok(InboxPurgeSummary { roles, messages })
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ---------------------------------------------------------------------------
// Message classification and digest
// ---------------------------------------------------------------------------

/// Category of an inbox message, used for priority sorting and collapsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MessageCategory {
    /// Escalation — highest priority, always shown individually.
    Escalation = 0,
    /// Review request or review-related message.
    ReviewRequest = 1,
    /// Blocker report from an engineer.
    Blocker = 2,
    /// Status update for a task.
    Status = 3,
    /// Idle or review nudge — lowest priority.
    Nudge = 4,
}

/// Classify a message body into a category.
pub fn classify_message(msg: &InboxMessage) -> MessageCategory {
    let body = msg.body.trim().to_ascii_lowercase();

    // Escalation detection
    if body.contains("escalat")
        || body.contains("task_escalated")
        || (body.contains("blocker") && body.contains("escalat"))
    {
        return MessageCategory::Escalation;
    }

    // Nudge detection (before blocker — nudges that mention "blocker" as instructions
    // are still nudges)
    if body.contains("idle nudge:")
        || body.starts_with("review nudge:")
        || body.contains("if you are idle, take action now")
        || body.contains("you have been idle past your configured timeout")
    {
        return MessageCategory::Nudge;
    }

    // Blocker detection
    if body.contains("blocked on") || body.contains("blocker:") || body.starts_with("blocked:") {
        return MessageCategory::Blocker;
    }

    // Review request detection
    if body.contains("ready for review")
        || body.contains("awaiting manual review")
        || body.contains("requires manual review")
        || body.contains("review request")
        || body.contains("ready_for_review")
        || body.starts_with("review:")
    {
        return MessageCategory::ReviewRequest;
    }

    // Status update detection
    if body.contains("status update")
        || body.contains("progress update")
        || body.contains("completion packet")
        || body.starts_with("status:")
    {
        return MessageCategory::Status;
    }

    // Default: treat as status (middle priority)
    MessageCategory::Status
}

/// Extract a task ID reference from a message body, if present.
/// Looks for patterns like "#42", "Task #42", "task_id: 42".
fn extract_task_id(body: &str) -> Option<String> {
    // Try "#N" pattern
    let body_lower = body.to_ascii_lowercase();
    if let Some(pos) = body_lower.find('#') {
        let after = &body[pos + 1..];
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            return Some(digits);
        }
    }
    // Try "task_id: N" or "task_id\":N"
    if let Some(pos) = body_lower.find("task_id") {
        let after = &body[pos + 7..];
        let digits: String = after
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if !digits.is_empty() {
            return Some(digits);
        }
    }
    None
}

/// A single entry in the digested inbox view.
#[derive(Debug, Clone)]
pub struct DigestEntry {
    /// The representative message (latest in the group).
    pub message: InboxMessage,
    /// Whether the representative message was delivered.
    pub delivered: bool,
    /// Category of this entry.
    pub category: MessageCategory,
    /// How many raw messages this entry represents (1 = no collapsing).
    pub collapsed_count: usize,
}

/// Digest a list of inbox messages: collapse nudges per sender, status
/// updates per task (keep latest), and priority-sort the result.
///
/// Returns `(digest_entries, raw_count)` where `raw_count` is the original
/// message count before collapsing.
pub fn digest_messages(messages: &[(InboxMessage, bool)]) -> (Vec<DigestEntry>, usize) {
    use std::collections::HashMap;

    let raw_count = messages.len();
    if messages.is_empty() {
        return (Vec::new(), 0);
    }

    // Group messages by (category, grouping key).
    // - Nudges group by sender ("from").
    // - Status updates group by task ID (if extractable), else by sender.
    // - Everything else stays individual.
    let mut groups: HashMap<(MessageCategory, String), Vec<(usize, MessageCategory)>> =
        HashMap::new();

    let classified: Vec<MessageCategory> = messages
        .iter()
        .map(|(msg, _)| classify_message(msg))
        .collect();

    for (idx, cat) in classified.iter().enumerate() {
        let (msg, _) = &messages[idx];
        let key = match cat {
            MessageCategory::Nudge => {
                // Group nudges by sender
                format!("nudge:{}", msg.from)
            }
            MessageCategory::Status => {
                // Group status by task ID if available, else by sender
                match extract_task_id(&msg.body) {
                    Some(tid) => format!("status:task#{tid}"),
                    None => format!("status:from:{}", msg.from),
                }
            }
            // Escalations, review requests, blockers stay individual
            _ => format!("individual:{idx}"),
        };
        groups.entry((*cat, key)).or_default().push((idx, *cat));
    }

    // Build digest entries: for each group, keep only the latest message.
    let mut entries: Vec<DigestEntry> = Vec::new();
    for ((_cat, _key), indices) in &groups {
        let count = indices.len();
        // Latest = highest timestamp (messages are sorted by timestamp, so last index)
        let Some(&(latest_idx, category)) = indices
            .iter()
            .max_by_key(|(idx, _)| messages[*idx].0.timestamp)
        else {
            continue;
        };
        let (msg, delivered) = &messages[latest_idx];
        entries.push(DigestEntry {
            message: msg.clone(),
            delivered: *delivered,
            category,
            collapsed_count: count,
        });
    }

    // Priority sort: by category (asc = escalation first), then by timestamp (desc = newest first)
    entries.sort_by(|a, b| {
        a.category
            .cmp(&b.category)
            .then_with(|| b.message.timestamp.cmp(&a.message.timestamp))
    });

    (entries, raw_count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbox_message_send_roundtrip() {
        let msg = InboxMessage::new_send("human", "architect", "hello world");
        assert_eq!(msg.from, "human");
        assert_eq!(msg.to, "architect");
        assert_eq!(msg.body, "hello world");
        assert_eq!(msg.msg_type, MessageType::Send);
        assert!(msg.timestamp > 0);

        let bytes = msg.to_json_bytes().unwrap();
        let parsed = InboxMessage::from_json_bytes(&bytes, "test-id").unwrap();
        assert_eq!(parsed.id, "test-id");
        assert_eq!(parsed.from, "human");
        assert_eq!(parsed.to, "architect");
        assert_eq!(parsed.body, "hello world");
    }

    #[test]
    fn inbox_message_assign_roundtrip() {
        let msg = InboxMessage::new_assign("black-lead", "eng-1-1", "fix the auth bug");
        assert_eq!(msg.msg_type, MessageType::Assign);
        assert_eq!(msg.from, "black-lead");
        assert_eq!(msg.to, "eng-1-1");
        assert_eq!(msg.body, "fix the auth bug");

        let bytes = msg.to_json_bytes().unwrap();
        let parsed = InboxMessage::from_json_bytes(&bytes, "assign-id").unwrap();
        assert_eq!(parsed.msg_type, MessageType::Assign);
    }

    #[test]
    fn inbox_message_age_uses_timestamp() {
        let mut msg = InboxMessage::new_send("human", "architect", "hello world");
        msg.timestamp = now_unix().saturating_sub(60);

        assert!(msg.age() >= Duration::from_secs(60));
    }

    #[test]
    fn init_inbox_creates_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init_inbox(root, "architect").unwrap();

        assert!(root.join("architect").join("new").is_dir());
        assert!(root.join("architect").join("cur").is_dir());
        assert!(root.join("architect").join("tmp").is_dir());
    }

    #[test]
    fn init_inbox_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init_inbox(root, "architect").unwrap();
        init_inbox(root, "architect").unwrap(); // should not error
    }

    #[test]
    fn deliver_and_read_pending() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init_inbox(root, "architect").unwrap();

        let msg = InboxMessage::new_send("human", "architect", "hello");
        let id = deliver_to_inbox(root, &msg).unwrap();
        assert!(!id.is_empty());

        let pending = pending_messages(root, "architect").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].from, "human");
        assert_eq!(pending[0].body, "hello");
        assert_eq!(pending[0].id, id);
    }

    #[test]
    fn deliver_creates_dirs_automatically() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // Don't call init_inbox — deliver_to_inbox should create dirs
        let msg = InboxMessage::new_send("human", "manager", "hi");
        let id = deliver_to_inbox(root, &msg).unwrap();
        assert!(!id.is_empty());

        let pending = pending_messages(root, "manager").unwrap();
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn read_recent_messages_filters_old_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init_inbox(root, "manager").unwrap();

        let mut old = InboxMessage::new_send("eng-1", "manager", "old");
        old.timestamp = now_unix().saturating_sub(601);
        deliver_to_inbox(root, &old).unwrap();

        let mut recent = InboxMessage::new_send("eng-2", "manager", "recent");
        recent.timestamp = now_unix().saturating_sub(60);
        let recent_id = deliver_to_inbox(root, &recent).unwrap();
        mark_delivered(root, "manager", &recent_id).unwrap();

        let messages = read_recent_messages(root, "manager", Duration::from_secs(300)).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].body, "recent");
    }

    #[test]
    fn message_signature_normalizes_whitespace() {
        let compact = "Task #42 failed after retries";
        let noisy = "Task   #42\nfailed   after   retries";

        assert_eq!(message_signature(compact), message_signature(noisy));
    }

    #[test]
    fn find_recent_duplicate_matches_same_sender_and_body() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init_inbox(root, "manager").unwrap();

        let mut existing = InboxMessage::new_send("eng-1", "manager", "status update");
        existing.timestamp = now_unix().saturating_sub(30);
        let existing_id = deliver_to_inbox(root, &existing).unwrap();
        mark_delivered(root, "manager", &existing_id).unwrap();

        let candidate = InboxMessage::new_send("eng-1", "manager", "status   update");
        let duplicate =
            find_recent_duplicate(root, "manager", &candidate, Duration::from_secs(300)).unwrap();

        assert!(duplicate.is_some());
        assert_eq!(duplicate.unwrap().from, "eng-1");
    }

    #[test]
    fn find_recent_duplicate_ignores_old_or_different_sender_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init_inbox(root, "manager").unwrap();

        let mut old = InboxMessage::new_send("eng-1", "manager", "status update");
        old.timestamp = now_unix().saturating_sub(601);
        deliver_to_inbox(root, &old).unwrap();

        let recent_other_sender = InboxMessage::new_send("eng-2", "manager", "status update");
        deliver_to_inbox(root, &recent_other_sender).unwrap();

        let candidate = InboxMessage::new_send("eng-1", "manager", "status update");
        let duplicate =
            find_recent_duplicate(root, "manager", &candidate, Duration::from_secs(300)).unwrap();

        assert!(duplicate.is_none());
    }

    #[test]
    fn mark_delivered_moves_to_cur() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init_inbox(root, "eng-1").unwrap();

        let msg = InboxMessage::new_send("manager", "eng-1", "do this");
        let id = deliver_to_inbox(root, &msg).unwrap();

        // Before: in new/
        assert_eq!(pending_messages(root, "eng-1").unwrap().len(), 1);

        mark_delivered(root, "eng-1", &id).unwrap();

        // After: not in new/ anymore
        assert_eq!(pending_messages(root, "eng-1").unwrap().len(), 0);

        // But visible in all_messages as delivered
        let all = all_messages(root, "eng-1").unwrap();
        assert_eq!(all.len(), 1);
        assert!(all[0].1); // delivered = true
    }

    #[test]
    fn pending_message_count_tracks_new_messages_only() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init_inbox(root, "eng-1").unwrap();

        let msg1 = InboxMessage::new_send("manager", "eng-1", "first");
        let msg2 = InboxMessage::new_send("manager", "eng-1", "second");
        let id1 = deliver_to_inbox(root, &msg1).unwrap();
        deliver_to_inbox(root, &msg2).unwrap();

        assert_eq!(pending_message_count(root, "eng-1").unwrap(), 2);

        mark_delivered(root, "eng-1", &id1).unwrap();
        assert_eq!(pending_message_count(root, "eng-1").unwrap(), 1);
    }

    #[test]
    fn multiple_messages_ordered_by_timestamp() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init_inbox(root, "arch").unwrap();

        // Deliver messages with different timestamps
        let mut msg1 = InboxMessage::new_send("human", "arch", "first");
        msg1.timestamp = 1000;
        let mut msg2 = InboxMessage::new_send("human", "arch", "second");
        msg2.timestamp = 2000;
        let mut msg3 = InboxMessage::new_send("human", "arch", "third");
        msg3.timestamp = 1500;

        deliver_to_inbox(root, &msg1).unwrap();
        deliver_to_inbox(root, &msg2).unwrap();
        deliver_to_inbox(root, &msg3).unwrap();

        let pending = pending_messages(root, "arch").unwrap();
        assert_eq!(pending.len(), 3);
        assert_eq!(pending[0].body, "first");
        assert_eq!(pending[1].body, "third");
        assert_eq!(pending[2].body, "second");
    }

    #[test]
    fn all_messages_combines_new_and_cur() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init_inbox(root, "mgr").unwrap();

        let msg1 = InboxMessage::new_send("arch", "mgr", "directive");
        let id1 = deliver_to_inbox(root, &msg1).unwrap();
        let msg2 = InboxMessage::new_send("eng-1", "mgr", "done");
        deliver_to_inbox(root, &msg2).unwrap();

        // Deliver first, leave second pending
        mark_delivered(root, "mgr", &id1).unwrap();

        let all = all_messages(root, "mgr").unwrap();
        assert_eq!(all.len(), 2);

        let delivered: Vec<_> = all.iter().filter(|(_, d)| *d).collect();
        let pending: Vec<_> = all.iter().filter(|(_, d)| !*d).collect();
        assert_eq!(delivered.len(), 1);
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn delete_message_removes_from_inbox() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init_inbox(root, "eng").unwrap();

        let msg = InboxMessage::new_send("mgr", "eng", "task");
        let id = deliver_to_inbox(root, &msg).unwrap();

        assert_eq!(pending_messages(root, "eng").unwrap().len(), 1);
        delete_message(root, "eng", &id).unwrap();
        assert_eq!(pending_messages(root, "eng").unwrap().len(), 0);
    }

    #[test]
    fn expire_stale_pending_messages_marks_old_entries_delivered() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init_inbox(root, "manager").unwrap();

        let mut old = InboxMessage::new_send("eng-1", "manager", "old");
        old.timestamp = now_unix().saturating_sub(900);
        let old_id = deliver_to_inbox(root, &old).unwrap();

        let mut fresh = InboxMessage::new_send("eng-2", "manager", "fresh");
        fresh.timestamp = now_unix().saturating_sub(30);
        deliver_to_inbox(root, &fresh).unwrap();

        let expired =
            expire_stale_pending_messages(root, "manager", Duration::from_secs(600)).unwrap();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].id, old_id);

        let pending = pending_messages(root, "manager").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].body, "fresh");

        let all = all_messages(root, "manager").unwrap();
        assert_eq!(all.len(), 2);
        assert!(
            all.iter()
                .any(|(message, delivered)| message.body == "old" && *delivered)
        );
    }

    #[test]
    fn pending_messages_empty_inbox() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init_inbox(root, "empty").unwrap();

        let pending = pending_messages(root, "empty").unwrap();
        assert!(pending.is_empty());
    }

    #[test]
    fn inboxes_root_path() {
        let root = std::path::Path::new("/tmp/project");
        assert_eq!(
            inboxes_root(root),
            PathBuf::from("/tmp/project/.batty/inboxes")
        );
    }

    #[test]
    fn malformed_json_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init_inbox(root, "bad").unwrap();

        // Write a non-JSON file directly into new/
        let new_dir = root.join("bad").join("new");
        std::fs::write(new_dir.join("1234567890.bad.localhost"), "not json").unwrap();

        // Should not panic, just skip the bad entry
        let pending = pending_messages(root, "bad").unwrap();
        assert!(pending.is_empty());
    }

    #[test]
    fn purge_delivered_messages_before_timestamp_only_removes_older_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init_inbox(root, "eng").unwrap();

        let mut old_msg = InboxMessage::new_send("mgr", "eng", "old");
        old_msg.timestamp = 10;
        let old_id = deliver_to_inbox(root, &old_msg).unwrap();
        mark_delivered(root, "eng", &old_id).unwrap();

        let mut new_msg = InboxMessage::new_send("mgr", "eng", "new");
        new_msg.timestamp = 20;
        let new_id = deliver_to_inbox(root, &new_msg).unwrap();
        mark_delivered(root, "eng", &new_id).unwrap();

        let removed = purge_delivered_messages(root, "eng", Some(15), false).unwrap();
        assert_eq!(removed, 1);

        let remaining = all_messages(root, "eng").unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].0.id, new_id);
        assert!(remaining[0].1);
    }

    #[test]
    fn purge_delivered_messages_all_removes_every_cur_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init_inbox(root, "eng").unwrap();

        for body in ["one", "two"] {
            let msg = InboxMessage::new_send("mgr", "eng", body);
            let id = deliver_to_inbox(root, &msg).unwrap();
            mark_delivered(root, "eng", &id).unwrap();
        }

        let removed = purge_delivered_messages(root, "eng", None, true).unwrap();
        assert_eq!(removed, 2);
        assert!(all_messages(root, "eng").unwrap().is_empty());
    }

    #[test]
    fn purge_delivered_messages_for_all_scans_every_member_inbox() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init_inbox(root, "eng-1").unwrap();
        init_inbox(root, "eng-2").unwrap();

        let msg1 = InboxMessage::new_send("mgr", "eng-1", "first");
        let id1 = deliver_to_inbox(root, &msg1).unwrap();
        mark_delivered(root, "eng-1", &id1).unwrap();

        let msg2 = InboxMessage::new_send("mgr", "eng-2", "second");
        let id2 = deliver_to_inbox(root, &msg2).unwrap();
        mark_delivered(root, "eng-2", &id2).unwrap();

        let summary = purge_delivered_messages_for_all(root, None, true).unwrap();
        assert_eq!(
            summary,
            InboxPurgeSummary {
                roles: 2,
                messages: 2
            }
        );
        assert!(all_messages(root, "eng-1").unwrap().is_empty());
        assert!(all_messages(root, "eng-2").unwrap().is_empty());
    }

    // ---- Message classification tests ----

    fn make_msg(from: &str, to: &str, body: &str, ts: u64) -> InboxMessage {
        let mut msg = InboxMessage::new_send(from, to, body);
        msg.timestamp = ts;
        msg
    }

    #[test]
    fn classify_idle_nudge() {
        let msg = make_msg(
            "daemon",
            "eng-1",
            "Idle nudge: you have been idle past your configured timeout. Move forward.",
            100,
        );
        assert_eq!(classify_message(&msg), MessageCategory::Nudge);
    }

    #[test]
    fn classify_review_nudge() {
        let msg = make_msg(
            "daemon",
            "manager",
            "Review nudge: task #42 awaiting review",
            100,
        );
        assert_eq!(classify_message(&msg), MessageCategory::Nudge);
    }

    #[test]
    fn classify_escalation() {
        let msg = make_msg(
            "eng-1",
            "manager",
            "Task #42 escalated: build failures",
            100,
        );
        assert_eq!(classify_message(&msg), MessageCategory::Escalation);
    }

    #[test]
    fn classify_blocker() {
        let msg = make_msg("eng-1", "manager", "Blocked on #42: missing API key", 100);
        assert_eq!(classify_message(&msg), MessageCategory::Blocker);
    }

    #[test]
    fn classify_review_request() {
        let msg = make_msg("eng-1", "manager", "Task #42 ready for review", 100);
        assert_eq!(classify_message(&msg), MessageCategory::ReviewRequest);
    }

    #[test]
    fn classify_manual_review_notice_as_review_request() {
        let msg = make_msg(
            "eng-1",
            "manager",
            "[eng-1] Task #42 passed tests but requires manual review.\nTitle: Inbox routing",
            100,
        );
        assert_eq!(classify_message(&msg), MessageCategory::ReviewRequest);
    }

    #[test]
    fn classify_status_update() {
        let msg = make_msg(
            "eng-1",
            "manager",
            "Status update on task #42: tests passing",
            100,
        );
        assert_eq!(classify_message(&msg), MessageCategory::Status);
    }

    #[test]
    fn classify_generic_message_as_status() {
        let msg = make_msg("eng-1", "manager", "Hello, just checking in", 100);
        assert_eq!(classify_message(&msg), MessageCategory::Status);
    }

    #[test]
    fn classify_nudge_with_idle_action_text() {
        let msg = make_msg("daemon", "eng-1", "If you are idle, take action NOW", 100);
        assert_eq!(classify_message(&msg), MessageCategory::Nudge);
    }

    // ---- extract_task_id tests ----

    #[test]
    fn extract_task_id_hash_pattern() {
        assert_eq!(extract_task_id("Task #42 is done"), Some("42".to_string()));
    }

    #[test]
    fn extract_task_id_from_json() {
        assert_eq!(
            extract_task_id(r#"{"task_id": 99, "status": "done"}"#),
            Some("99".to_string())
        );
    }

    #[test]
    fn extract_task_id_none_when_missing() {
        assert_eq!(extract_task_id("no task reference here"), None);
    }

    // ---- digest_messages tests ----

    #[test]
    fn digest_empty_messages() {
        let (entries, raw) = digest_messages(&[]);
        assert!(entries.is_empty());
        assert_eq!(raw, 0);
    }

    #[test]
    fn digest_collapses_nudges_per_sender() {
        let msgs: Vec<(InboxMessage, bool)> = vec![
            (
                make_msg("daemon", "eng-1", "Idle nudge: move forward", 100),
                true,
            ),
            (
                make_msg("daemon", "eng-1", "Idle nudge: move forward", 200),
                true,
            ),
            (
                make_msg("daemon", "eng-1", "Idle nudge: move forward", 300),
                true,
            ),
        ];

        let (entries, raw_count) = digest_messages(&msgs);
        assert_eq!(raw_count, 3);
        assert_eq!(
            entries.len(),
            1,
            "3 nudges from same sender should collapse to 1"
        );
        assert_eq!(entries[0].collapsed_count, 3);
        assert_eq!(entries[0].message.timestamp, 300, "should keep latest");
        assert_eq!(entries[0].category, MessageCategory::Nudge);
    }

    #[test]
    fn digest_keeps_nudges_separate_per_sender() {
        // Manager inbox with nudges from different sources
        let msgs: Vec<(InboxMessage, bool)> = vec![
            (
                make_msg("daemon", "manager", "Idle nudge: eng-1 is idle", 100),
                true,
            ),
            (
                make_msg(
                    "architect",
                    "manager",
                    "Review nudge: task #42 awaiting review",
                    200,
                ),
                true,
            ),
        ];

        let (entries, _) = digest_messages(&msgs);
        assert_eq!(
            entries.len(),
            2,
            "nudges from different senders stay separate"
        );
    }

    #[test]
    fn digest_collapses_status_updates_per_task() {
        let msgs: Vec<(InboxMessage, bool)> = vec![
            (
                make_msg(
                    "eng-1",
                    "manager",
                    "Status update on task #42: compiling",
                    100,
                ),
                true,
            ),
            (
                make_msg(
                    "eng-1",
                    "manager",
                    "Status update on task #42: tests passing",
                    200,
                ),
                true,
            ),
            (
                make_msg("eng-1", "manager", "Status update on task #42: done", 300),
                true,
            ),
        ];

        let (entries, raw_count) = digest_messages(&msgs);
        assert_eq!(raw_count, 3);
        assert_eq!(
            entries.len(),
            1,
            "3 status updates for same task should collapse"
        );
        assert_eq!(entries[0].collapsed_count, 3);
        assert_eq!(entries[0].message.timestamp, 300, "should keep latest");
    }

    #[test]
    fn digest_keeps_status_separate_per_task() {
        let msgs: Vec<(InboxMessage, bool)> = vec![
            (
                make_msg("eng-1", "manager", "Status update on task #42: done", 100),
                true,
            ),
            (
                make_msg(
                    "eng-1",
                    "manager",
                    "Status update on task #99: compiling",
                    200,
                ),
                true,
            ),
        ];

        let (entries, _) = digest_messages(&msgs);
        assert_eq!(entries.len(), 2, "status for different tasks stay separate");
    }

    #[test]
    fn digest_never_collapses_escalations() {
        let msgs: Vec<(InboxMessage, bool)> = vec![
            (
                make_msg(
                    "eng-1",
                    "manager",
                    "Task #42 escalated: build failures",
                    100,
                ),
                false,
            ),
            (
                make_msg("eng-2", "manager", "Task #42 escalated: tests broken", 200),
                false,
            ),
        ];

        let (entries, _) = digest_messages(&msgs);
        assert_eq!(entries.len(), 2, "escalations are never collapsed");
        assert_eq!(entries[0].category, MessageCategory::Escalation);
        assert_eq!(entries[1].category, MessageCategory::Escalation);
    }

    #[test]
    fn digest_priority_sorts_escalations_first_nudges_last() {
        let msgs: Vec<(InboxMessage, bool)> = vec![
            (
                make_msg("daemon", "manager", "Idle nudge: move forward", 400),
                true,
            ),
            (
                make_msg("eng-1", "manager", "Status update on task #42: done", 300),
                true,
            ),
            (
                make_msg("eng-1", "manager", "Blocked on #99: missing key", 200),
                true,
            ),
            (
                make_msg("eng-2", "manager", "Task #50 escalated: critical", 100),
                false,
            ),
            (
                make_msg("eng-1", "manager", "Task #42 ready for review", 350),
                true,
            ),
        ];

        let (entries, _) = digest_messages(&msgs);

        let categories: Vec<MessageCategory> = entries.iter().map(|e| e.category).collect();
        // Verify ordering: Escalation < ReviewRequest < Blocker < Status < Nudge
        for i in 1..categories.len() {
            assert!(
                categories[i - 1] <= categories[i],
                "category at {} ({:?}) should come before or equal category at {} ({:?})",
                i - 1,
                categories[i - 1],
                i,
                categories[i]
            );
        }
        assert_eq!(categories[0], MessageCategory::Escalation);
        assert_eq!(*categories.last().unwrap(), MessageCategory::Nudge);
    }

    #[test]
    fn digest_mixed_scenario_achieves_significant_reduction() {
        // Simulate a typical noisy session: 5 nudges (same eng), 4 status updates (same task),
        // 1 escalation, 1 review request, 1 blocker = 12 raw messages
        let mut msgs: Vec<(InboxMessage, bool)> = Vec::new();
        for i in 0..5 {
            msgs.push((
                make_msg("daemon", "eng-1", "Idle nudge: move forward", 100 + i),
                true,
            ));
        }
        for i in 0..4 {
            msgs.push((
                make_msg(
                    "eng-1",
                    "manager",
                    &format!("Status update on task #42: step {i}"),
                    200 + i,
                ),
                true,
            ));
        }
        msgs.push((
            make_msg(
                "eng-2",
                "manager",
                "Task #99 escalated: critical failure",
                300,
            ),
            false,
        ));
        msgs.push((
            make_msg("eng-1", "manager", "Task #42 ready for review", 350),
            true,
        ));
        msgs.push((
            make_msg("eng-3", "manager", "Blocked on #55: need credentials", 320),
            true,
        ));

        let (entries, raw_count) = digest_messages(&msgs);
        assert_eq!(raw_count, 12);
        // Expected: 1 escalation + 1 review + 1 blocker + 1 status(collapsed 4) + 1 nudge(collapsed 5) = 5 entries
        assert_eq!(entries.len(), 5);
        // Reduction: 12 -> 5 = 58% reduction, close to the 60% target
        let reduction_pct = ((raw_count - entries.len()) as f64 / raw_count as f64) * 100.0;
        assert!(
            reduction_pct >= 50.0,
            "Expected 50%+ reduction, got {reduction_pct:.0}%"
        );
    }

    #[test]
    fn digest_preserves_delivered_status_of_latest() {
        let msgs: Vec<(InboxMessage, bool)> = vec![
            (make_msg("daemon", "eng-1", "Idle nudge: old", 100), true),
            (
                make_msg("daemon", "eng-1", "Idle nudge: latest", 200),
                false,
            ),
        ];

        let (entries, _) = digest_messages(&msgs);
        assert_eq!(entries.len(), 1);
        assert!(
            !entries[0].delivered,
            "should use delivered status of latest message"
        );
    }

    fn production_unwrap_expect_count(source: &str) -> usize {
        let prod = if let Some(pos) = source.find("\n#[cfg(test)]\nmod tests") {
            &source[..pos]
        } else {
            source
        };
        prod.lines()
            .filter(|line| {
                let trimmed = line.trim();
                !trimmed.starts_with("#[cfg(test)]")
                    && (trimmed.contains(".unwrap(") || trimmed.contains(".expect("))
            })
            .count()
    }

    #[test]
    fn production_inbox_has_no_unwrap_or_expect_calls() {
        let src = include_str!("inbox.rs");
        assert_eq!(
            production_unwrap_expect_count(src),
            0,
            "production inbox.rs should avoid unwrap/expect"
        );
    }
}
