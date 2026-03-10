//! Maildir-based inbox messaging system.
//!
//! Each team member gets a Maildir at `.batty/inboxes/<member>/` with
//! `new/`, `cur/`, `tmp/` subdirectories. Messages are JSON blobs stored
//! atomically via the `maildir` crate.
//!
//! - `new/` — undelivered messages (daemon picks these up)
//! - `cur/` — delivered messages (moved here after tmux injection)
//! - `tmp/` — atomic write staging (managed by `maildir` crate)

use std::path::{Path, PathBuf};

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
}
