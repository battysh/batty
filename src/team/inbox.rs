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
