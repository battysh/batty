//! Tiered inbox queue implementation (ticket #658).
//!
//! Splits each member's inbox into four Maildir sub-queues by priority:
//!
//! ```text
//! .batty/inboxes/<member>/
//!   priority/{new,cur,tmp}/    # escalations, blockers (always deliver)
//!   work/{new,cur,tmp}/        # task assignments, review requests
//!   content/{new,cur,tmp}/     # completion packets, standups, status
//!   telemetry/{new,cur,tmp}/   # nudges, heartbeats, recovery notices
//! ```
//!
//! Each tier has its own TTL and deduplication semantics so that stale
//! telemetry cannot drown out actionable priority messages.
//!
//! The module is additive: the flat `.batty/inboxes/<member>/{new,cur,tmp}/`
//! layout continues to work unchanged when
//! `workflow_policy.tiered_inboxes` is `false` (the default). When the flag
//! is `true`, writes go to the tier subdirectories; reads dual-read from
//! both locations so in-flight messages are never stranded during a
//! migration.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use maildir::Maildir;

use crate::team::inbox::{self, InboxMessage, MessageCategory};

/// A queue tier. Mapped 1:1 to a subdirectory under the member's inbox root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum QueueTier {
    /// Escalations, blockers — always delivered, never collapsed.
    Priority,
    /// Task assignments, review requests — collapsible by task id.
    Work,
    /// Completion packets, standups, status updates — keep latest per task.
    Content,
    /// Nudges, heartbeats, recovery notices — aggressively collapsed.
    Telemetry,
}

impl QueueTier {
    /// All tiers, in priority order (highest first).
    pub const ALL: [QueueTier; 4] = [
        QueueTier::Priority,
        QueueTier::Work,
        QueueTier::Content,
        QueueTier::Telemetry,
    ];

    /// Subdirectory name for the tier (used as Maildir base).
    pub fn subdir(&self) -> &'static str {
        match self {
            QueueTier::Priority => "priority",
            QueueTier::Work => "work",
            QueueTier::Content => "content",
            QueueTier::Telemetry => "telemetry",
        }
    }

    /// Human-readable label for prompts / status output.
    pub fn label(&self) -> &'static str {
        match self {
            QueueTier::Priority => "PRIORITY",
            QueueTier::Work => "WORK",
            QueueTier::Content => "CONTENT",
            QueueTier::Telemetry => "TELEMETRY",
        }
    }
}

impl std::fmt::Display for QueueTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.subdir())
    }
}

/// Map a `MessageCategory` to the tier it should be routed to.
pub fn category_to_tier(category: MessageCategory) -> QueueTier {
    match category {
        MessageCategory::Escalation | MessageCategory::Blocker => QueueTier::Priority,
        MessageCategory::ReviewRequest => QueueTier::Work,
        MessageCategory::Status => QueueTier::Content,
        MessageCategory::Nudge => QueueTier::Telemetry,
    }
}

/// Per-tier TTL configuration. Pending messages older than the tier's
/// `max_age` are expired on the next sweep.
///
/// Defaults match the design doc (planning/inbox-control-plane-design.md):
/// priority 1h, work 30m, content 15m, telemetry 5m.
#[derive(Debug, Clone, Copy)]
pub struct TieredTtlConfig {
    pub priority: Duration,
    pub work: Duration,
    pub content: Duration,
    pub telemetry: Duration,
}

impl TieredTtlConfig {
    /// TTL for a specific tier.
    pub fn for_tier(&self, tier: QueueTier) -> Duration {
        match tier {
            QueueTier::Priority => self.priority,
            QueueTier::Work => self.work,
            QueueTier::Content => self.content,
            QueueTier::Telemetry => self.telemetry,
        }
    }
}

impl Default for TieredTtlConfig {
    fn default() -> Self {
        Self {
            priority: Duration::from_secs(3600),
            work: Duration::from_secs(1800),
            content: Duration::from_secs(900),
            telemetry: Duration::from_secs(300),
        }
    }
}

/// Summary of an expiry sweep across all tiers.
#[derive(Debug, Clone, Default)]
pub struct ExpiredCounts {
    pub priority: usize,
    pub work: usize,
    pub content: usize,
    pub telemetry: usize,
}

impl ExpiredCounts {
    pub fn total(&self) -> usize {
        self.priority + self.work + self.content + self.telemetry
    }
}

/// Path to the tiered Maildir for a member + tier.
fn tier_path(inboxes_root: &Path, member: &str, tier: QueueTier) -> PathBuf {
    inboxes_root.join(member).join(tier.subdir())
}

/// Maildir handle for a (member, tier) pair.
fn tier_maildir(inboxes_root: &Path, member: &str, tier: QueueTier) -> Maildir {
    Maildir::from(tier_path(inboxes_root, member, tier))
}

/// Initialize all tier subdirectories for a member (idempotent).
pub fn init_tiered_inbox(inboxes_root: &Path, member: &str) -> Result<()> {
    for tier in QueueTier::ALL {
        let md = tier_maildir(inboxes_root, member, tier);
        md.create_dirs().with_context(|| {
            format!(
                "failed to create tiered inbox dirs for '{member}/{}'",
                tier.subdir()
            )
        })?;
    }
    Ok(())
}

/// Deliver a message to the tier implied by its category.
///
/// If `tier` is `None`, classify the message via `inbox::classify_message`
/// and route to `category_to_tier(category)`. Returns `(tier, maildir_id)`.
pub fn deliver_to_tiered_inbox(
    inboxes_root: &Path,
    msg: &InboxMessage,
    tier: Option<QueueTier>,
) -> Result<(QueueTier, String)> {
    let tier = tier.unwrap_or_else(|| category_to_tier(inbox::classify_message(msg)));
    let md = tier_maildir(inboxes_root, &msg.to, tier);
    md.create_dirs().with_context(|| {
        format!(
            "failed to create tiered inbox dirs for '{}/{}'",
            msg.to,
            tier.subdir()
        )
    })?;
    let data = msg.to_json_bytes()?;
    let id = md.store_new(&data).with_context(|| {
        format!(
            "failed to store message in tiered inbox for '{}/{}'",
            msg.to,
            tier.subdir()
        )
    })?;
    Ok((tier, id))
}

/// Read pending (undelivered) messages for a single tier.
///
/// Returns an empty vector if the tier directory does not exist yet
/// (so callers can safely invoke this before migration).
pub fn pending_messages_for_tier(
    inboxes_root: &Path,
    member: &str,
    tier: QueueTier,
) -> Result<Vec<InboxMessage>> {
    let path = tier_path(inboxes_root, member, tier);
    if !path.join("new").is_dir() {
        return Ok(Vec::new());
    }
    let md = tier_maildir(inboxes_root, member, tier);
    let mut messages = Vec::new();
    for entry in md.list_new() {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(member, tier = %tier, error = %e, "skipping unreadable tiered inbox entry");
                continue;
            }
        };
        let id = entry.id().to_string();
        let data = match std::fs::read(entry.path()) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(member, tier = %tier, id = %id, error = %e, "failed to read tiered inbox message");
                continue;
            }
        };
        match InboxMessage::from_json_bytes(&data, &id) {
            Ok(msg) => messages.push(msg),
            Err(e) => {
                tracing::warn!(member, tier = %tier, id = %id, error = %e, "skipping malformed tiered inbox message");
            }
        }
    }
    messages.sort_by_key(|m| m.timestamp);
    Ok(messages)
}

/// Pending messages grouped by tier (highest priority first).
///
/// Each tier's messages are FIFO-sorted (oldest first). Tiers that do not
/// exist on disk are represented by empty vectors.
pub fn pending_messages_by_tier(
    inboxes_root: &Path,
    member: &str,
) -> Result<Vec<(QueueTier, Vec<InboxMessage>)>> {
    let mut out = Vec::with_capacity(QueueTier::ALL.len());
    for tier in QueueTier::ALL {
        let msgs = pending_messages_for_tier(inboxes_root, member, tier)?;
        out.push((tier, msgs));
    }
    Ok(out)
}

/// Mark a specific tiered message as delivered (new/ → cur/).
pub fn mark_tiered_delivered(
    inboxes_root: &Path,
    member: &str,
    tier: QueueTier,
    id: &str,
) -> Result<()> {
    let md = tier_maildir(inboxes_root, member, tier);
    md.move_new_to_cur(id).with_context(|| {
        format!(
            "failed to mark tiered message '{id}' as delivered for '{member}/{}'",
            tier.subdir()
        )
    })?;
    Ok(())
}

/// Expire pending messages per tier using the provided TTL config.
///
/// Per-tier sweep: priority 1h, work 30m, content 15m, telemetry 5m by
/// default. Expired messages are moved to `cur/` (same as `mark_delivered`)
/// so they stop being injected into agent prompts but remain on disk for
/// audit until the delivered-message purger cleans them up.
pub fn expire_tiered_queues(
    inboxes_root: &Path,
    member: &str,
    ttls: &TieredTtlConfig,
) -> Result<ExpiredCounts> {
    let mut counts = ExpiredCounts::default();
    for tier in QueueTier::ALL {
        let max_age = ttls.for_tier(tier);
        let pending = pending_messages_for_tier(inboxes_root, member, tier)?;
        let mut expired = 0usize;
        for message in pending {
            if message.age() > max_age {
                mark_tiered_delivered(inboxes_root, member, tier, &message.id)?;
                expired += 1;
            }
        }
        match tier {
            QueueTier::Priority => counts.priority = expired,
            QueueTier::Work => counts.work = expired,
            QueueTier::Content => counts.content = expired,
            QueueTier::Telemetry => counts.telemetry = expired,
        }
    }
    Ok(counts)
}

/// Count pending messages in each tier.
///
/// Returns (priority, work, content, telemetry) counts. Cheap: does not
/// parse message bodies, just counts files in `new/` directories.
pub fn tiered_pending_counts(inboxes_root: &Path, member: &str) -> Result<[(QueueTier, usize); 4]> {
    let mut out = [
        (QueueTier::Priority, 0),
        (QueueTier::Work, 0),
        (QueueTier::Content, 0),
        (QueueTier::Telemetry, 0),
    ];
    for (i, tier) in QueueTier::ALL.iter().enumerate() {
        let new_dir = tier_path(inboxes_root, member, *tier).join("new");
        if !new_dir.is_dir() {
            continue;
        }
        let mut count = 0usize;
        if let Ok(entries) = std::fs::read_dir(&new_dir) {
            for entry in entries.flatten() {
                if let Ok(ft) = entry.file_type() {
                    if ft.is_file() {
                        count += 1;
                    }
                }
            }
        }
        out[i].1 = count;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Feature-flag-aware helpers
// ---------------------------------------------------------------------------

/// Deliver a message via either the flat or tiered layout depending on the
/// feature flag. Returns the maildir message id; the tier (if any) is
/// discarded to keep the signature compatible with
/// `inbox::deliver_to_inbox`.
pub fn deliver_flag_aware(inboxes_root: &Path, msg: &InboxMessage, tiered: bool) -> Result<String> {
    if tiered {
        deliver_to_tiered_inbox(inboxes_root, msg, None).map(|(_, id)| id)
    } else {
        inbox::deliver_to_inbox(inboxes_root, msg)
    }
}

/// Count pending messages across the flat `new/` queue and every tier
/// subdirectory. Cheap — just counts files, does not parse bodies.
pub fn pending_message_count_union(inboxes_root: &Path, member: &str) -> Result<usize> {
    let flat = inbox::pending_message_count(inboxes_root, member)?;
    let tier_total: usize = tiered_pending_counts(inboxes_root, member)?
        .iter()
        .map(|(_, n)| *n)
        .sum();
    Ok(flat + tier_total)
}

/// Read pending (undelivered) messages from both the flat `new/` queue
/// and every tier subdirectory, merged in FIFO timestamp order.
///
/// Safe to call regardless of the `tiered_inboxes` flag: tier
/// subdirectories that do not exist return empty vectors. This lets the
/// read path survive a flag flip without stranding in-flight flat
/// messages.
pub fn pending_messages_union(inboxes_root: &Path, member: &str) -> Result<Vec<InboxMessage>> {
    let mut out = inbox::pending_messages(inboxes_root, member)?;
    for tier in QueueTier::ALL {
        out.extend(pending_messages_for_tier(inboxes_root, member, tier)?);
    }
    out.sort_by_key(|m| m.timestamp);
    Ok(out)
}

/// Expire stale pending messages in both layouts.
///
/// - The flat queue uses `flat_max_age` (same as
///   `inbox::expire_stale_pending_messages`).
/// - Tier queues use per-tier TTLs from `ttls` when `tiered` is true.
pub fn expire_flag_aware(
    inboxes_root: &Path,
    member: &str,
    flat_max_age: Duration,
    ttls: &TieredTtlConfig,
    tiered: bool,
) -> Result<(Vec<InboxMessage>, Option<ExpiredCounts>)> {
    let flat = inbox::expire_stale_pending_messages(inboxes_root, member, flat_max_age)?;
    let tiered_counts = if tiered {
        Some(expire_tiered_queues(inboxes_root, member, ttls)?)
    } else {
        None
    };
    Ok((flat, tiered_counts))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::inbox::{InboxMessage, MessageCategory};
    use tempfile::TempDir;

    fn root() -> TempDir {
        TempDir::new().unwrap()
    }

    #[test]
    fn category_to_tier_maps_all_variants() {
        assert_eq!(
            category_to_tier(MessageCategory::Escalation),
            QueueTier::Priority
        );
        assert_eq!(
            category_to_tier(MessageCategory::Blocker),
            QueueTier::Priority
        );
        assert_eq!(
            category_to_tier(MessageCategory::ReviewRequest),
            QueueTier::Work
        );
        assert_eq!(
            category_to_tier(MessageCategory::Status),
            QueueTier::Content
        );
        assert_eq!(
            category_to_tier(MessageCategory::Nudge),
            QueueTier::Telemetry
        );
    }

    #[test]
    fn queue_tier_subdir_names_are_stable() {
        assert_eq!(QueueTier::Priority.subdir(), "priority");
        assert_eq!(QueueTier::Work.subdir(), "work");
        assert_eq!(QueueTier::Content.subdir(), "content");
        assert_eq!(QueueTier::Telemetry.subdir(), "telemetry");
    }

    #[test]
    fn init_creates_all_tier_dirs() {
        let tmp = root();
        init_tiered_inbox(tmp.path(), "eng-1").unwrap();
        for tier in QueueTier::ALL {
            let base = tmp.path().join("eng-1").join(tier.subdir());
            assert!(base.join("new").is_dir(), "{} new missing", tier.subdir());
            assert!(base.join("cur").is_dir(), "{} cur missing", tier.subdir());
            assert!(base.join("tmp").is_dir(), "{} tmp missing", tier.subdir());
        }
    }

    #[test]
    fn deliver_auto_classifies_escalation_to_priority() {
        let tmp = root();
        let msg = InboxMessage::new_send(
            "manager",
            "eng-1",
            "escalating stuck task #10: engineer has been idle for 45m",
        );
        let (tier, _id) = deliver_to_tiered_inbox(tmp.path(), &msg, None).unwrap();
        assert_eq!(tier, QueueTier::Priority);

        let pending = pending_messages_for_tier(tmp.path(), "eng-1", QueueTier::Priority).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].body, msg.body);
    }

    #[test]
    fn deliver_explicit_tier_override_ignores_category() {
        let tmp = root();
        // Body classifies as Nudge → Telemetry, but we override.
        let msg = InboxMessage::new_send("architect", "eng-1", "review nudge: take a look");
        let (tier, _id) =
            deliver_to_tiered_inbox(tmp.path(), &msg, Some(QueueTier::Priority)).unwrap();
        assert_eq!(tier, QueueTier::Priority);
    }

    #[test]
    fn pending_messages_for_tier_empty_when_no_dir() {
        let tmp = root();
        let pending = pending_messages_for_tier(tmp.path(), "eng-1", QueueTier::Work).unwrap();
        assert!(pending.is_empty());
    }

    #[test]
    fn pending_messages_by_tier_returns_four_tiers() {
        let tmp = root();
        init_tiered_inbox(tmp.path(), "eng-1").unwrap();
        let byt = pending_messages_by_tier(tmp.path(), "eng-1").unwrap();
        assert_eq!(byt.len(), 4);
        assert_eq!(byt[0].0, QueueTier::Priority);
        assert_eq!(byt[3].0, QueueTier::Telemetry);
    }

    #[test]
    fn expire_respects_per_tier_ttls() {
        let tmp = root();
        // Priority has long TTL (kept), telemetry has zero TTL (expired).
        let ttls = TieredTtlConfig {
            priority: Duration::from_secs(3600),
            work: Duration::from_secs(3600),
            content: Duration::from_secs(3600),
            telemetry: Duration::from_secs(1),
        };

        // Write messages with explicit past timestamps so `age()` is deterministic.
        let mut prio = InboxMessage::new_send("manager", "eng-1", "escalation: stuck task #5");
        prio.timestamp = prio.timestamp.saturating_sub(60);
        let mut nudge = InboxMessage::new_send("architect", "eng-1", "review nudge: PR #3");
        nudge.timestamp = nudge.timestamp.saturating_sub(60);
        deliver_to_tiered_inbox(tmp.path(), &prio, None).unwrap();
        deliver_to_tiered_inbox(tmp.path(), &nudge, None).unwrap();

        let counts = expire_tiered_queues(tmp.path(), "eng-1", &ttls).unwrap();
        assert_eq!(counts.priority, 0, "priority TTL is 1h, should not expire");
        assert_eq!(counts.telemetry, 1, "telemetry TTL is 1s, should expire");
        assert_eq!(counts.total(), 1);

        // Priority message still pending after sweep.
        let remaining =
            pending_messages_for_tier(tmp.path(), "eng-1", QueueTier::Priority).unwrap();
        assert_eq!(remaining.len(), 1);
    }

    #[test]
    fn tiered_pending_counts_counts_per_tier() {
        let tmp = root();
        let esc = InboxMessage::new_send("manager", "eng-1", "escalation: task blocked");
        let review =
            InboxMessage::new_send("architect", "eng-1", "task ready for review: please verify");
        let status = InboxMessage::new_send("eng-2", "eng-1", "status update: task #4 done");
        let nudge1 = InboxMessage::new_send("architect", "eng-1", "idle nudge: anything blocking?");
        let nudge2 = InboxMessage::new_send("architect", "eng-1", "review nudge: PR waiting");
        for m in [&esc, &review, &status, &nudge1, &nudge2] {
            deliver_to_tiered_inbox(tmp.path(), m, None).unwrap();
        }

        let counts = tiered_pending_counts(tmp.path(), "eng-1").unwrap();
        let mut map = std::collections::HashMap::new();
        for (tier, n) in counts {
            map.insert(tier, n);
        }
        assert_eq!(map[&QueueTier::Priority], 1);
        assert_eq!(map[&QueueTier::Work], 1);
        assert_eq!(map[&QueueTier::Content], 1);
        assert_eq!(map[&QueueTier::Telemetry], 2);
    }

    #[test]
    fn expire_zero_pending_returns_zero_counts() {
        let tmp = root();
        let ttls = TieredTtlConfig::default();
        let counts = expire_tiered_queues(tmp.path(), "eng-1", &ttls).unwrap();
        assert_eq!(counts.total(), 0);
    }

    #[test]
    fn mark_tiered_delivered_moves_new_to_cur() {
        let tmp = root();
        let msg = InboxMessage::new_send("manager", "eng-1", "escalation: stuck");
        let (tier, id) = deliver_to_tiered_inbox(tmp.path(), &msg, None).unwrap();
        assert_eq!(tier, QueueTier::Priority);

        mark_tiered_delivered(tmp.path(), "eng-1", tier, &id).unwrap();

        let pending = pending_messages_for_tier(tmp.path(), "eng-1", tier).unwrap();
        assert!(pending.is_empty(), "message should be moved out of new/");
        let cur = tmp.path().join("eng-1").join("priority").join("cur");
        assert!(cur.is_dir());
        let count = std::fs::read_dir(&cur).unwrap().count();
        assert_eq!(count, 1, "cur/ should contain the delivered message");
    }

    #[test]
    fn ttl_config_per_tier_lookup() {
        let ttls = TieredTtlConfig::default();
        assert_eq!(ttls.for_tier(QueueTier::Priority).as_secs(), 3600);
        assert_eq!(ttls.for_tier(QueueTier::Work).as_secs(), 1800);
        assert_eq!(ttls.for_tier(QueueTier::Content).as_secs(), 900);
        assert_eq!(ttls.for_tier(QueueTier::Telemetry).as_secs(), 300);
    }

    #[test]
    fn deliver_flag_aware_uses_flat_when_disabled() {
        let tmp = root();
        let msg = InboxMessage::new_send("manager", "eng-1", "escalation: test");
        deliver_flag_aware(tmp.path(), &msg, false).unwrap();

        // Flat layout received the message.
        let flat = inbox::pending_messages(tmp.path(), "eng-1").unwrap();
        assert_eq!(flat.len(), 1);

        // Tier subdirs did not.
        for tier in QueueTier::ALL {
            let t = pending_messages_for_tier(tmp.path(), "eng-1", tier).unwrap();
            assert!(t.is_empty(), "tier {} should be empty", tier.subdir());
        }
    }

    #[test]
    fn deliver_flag_aware_uses_tiered_when_enabled() {
        let tmp = root();
        let msg = InboxMessage::new_send("manager", "eng-1", "escalation: urgent");
        deliver_flag_aware(tmp.path(), &msg, true).unwrap();

        // Flat layout stays empty.
        let flat = inbox::pending_messages(tmp.path(), "eng-1").unwrap();
        assert!(flat.is_empty());

        // Priority tier received the message.
        let prio = pending_messages_for_tier(tmp.path(), "eng-1", QueueTier::Priority).unwrap();
        assert_eq!(prio.len(), 1);
    }

    #[test]
    fn pending_messages_union_merges_flat_and_tiered() {
        let tmp = root();
        // Write one flat + one tiered, each with distinct timestamps.
        let mut flat_msg =
            InboxMessage::new_send("manager", "eng-1", "status update: task #1 progress");
        flat_msg.timestamp = 1_700_000_000;
        inbox::deliver_to_inbox(tmp.path(), &flat_msg).unwrap();

        let mut tiered_msg = InboxMessage::new_send("manager", "eng-1", "escalation: stuck for 1h");
        tiered_msg.timestamp = 1_700_000_060;
        deliver_to_tiered_inbox(tmp.path(), &tiered_msg, None).unwrap();

        let union = pending_messages_union(tmp.path(), "eng-1").unwrap();
        assert_eq!(union.len(), 2);
        // FIFO order preserved across layouts.
        assert_eq!(union[0].timestamp, 1_700_000_000);
        assert_eq!(union[1].timestamp, 1_700_000_060);
    }

    #[test]
    fn expire_flag_aware_flat_only_when_disabled() {
        let tmp = root();
        // Flat message with old timestamp.
        let mut flat_msg = InboxMessage::new_send("manager", "eng-1", "nudge");
        flat_msg.timestamp = 1; // very old
        inbox::deliver_to_inbox(tmp.path(), &flat_msg).unwrap();

        // Tier message, also old, but tiered flag is off so it's not swept.
        let mut tiered_msg = InboxMessage::new_send("manager", "eng-1", "escalation: x");
        tiered_msg.timestamp = 1;
        deliver_to_tiered_inbox(tmp.path(), &tiered_msg, None).unwrap();

        let ttls = TieredTtlConfig::default();
        let (flat_expired, tier_counts) =
            expire_flag_aware(tmp.path(), "eng-1", Duration::from_secs(60), &ttls, false).unwrap();

        assert_eq!(flat_expired.len(), 1);
        assert!(tier_counts.is_none());

        // Tier message still pending (not swept).
        let prio = pending_messages_for_tier(tmp.path(), "eng-1", QueueTier::Priority).unwrap();
        assert_eq!(prio.len(), 1);
    }

    #[test]
    fn expire_flag_aware_sweeps_both_when_enabled() {
        let tmp = root();
        let ttls = TieredTtlConfig {
            priority: Duration::from_secs(1),
            work: Duration::from_secs(1),
            content: Duration::from_secs(1),
            telemetry: Duration::from_secs(1),
        };

        // Use explicit past timestamps so `age()` is deterministic
        // regardless of clock granularity.
        let mut flat = InboxMessage::new_send("manager", "eng-1", "nudge old");
        flat.timestamp = flat.timestamp.saturating_sub(60);
        let mut tiered = InboxMessage::new_send("manager", "eng-1", "escalation: stuck");
        tiered.timestamp = tiered.timestamp.saturating_sub(60);
        inbox::deliver_to_inbox(tmp.path(), &flat).unwrap();
        deliver_to_tiered_inbox(tmp.path(), &tiered, None).unwrap();

        let (flat_expired, tier_counts) =
            expire_flag_aware(tmp.path(), "eng-1", Duration::from_secs(1), &ttls, true).unwrap();

        assert_eq!(flat_expired.len(), 1);
        let counts = tier_counts.expect("tier counts should be returned when flag enabled");
        assert_eq!(counts.priority, 1);
        assert_eq!(counts.total(), 1);
    }

    #[test]
    fn queue_tier_method_on_message_category() {
        // Sanity-check the queue_tier() method we added to MessageCategory.
        assert_eq!(MessageCategory::Escalation.queue_tier(), "priority");
        assert_eq!(MessageCategory::Blocker.queue_tier(), "priority");
        assert_eq!(MessageCategory::ReviewRequest.queue_tier(), "work");
        assert_eq!(MessageCategory::Status.queue_tier(), "content");
        assert_eq!(MessageCategory::Nudge.queue_tier(), "telemetry");
    }
}
