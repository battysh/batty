use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crate::shim::protocol::Command;

use super::agent_handle::AgentHandle;

const ACK_TIMEOUT_SECS: u64 = 5;
const MESSAGE_EXPIRY_SECS: u64 = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutboxStatus {
    Pending,
    Sent,
    Acked,
    Failed,
    Expired,
}

#[derive(Debug)]
pub(crate) struct OutboxEntry {
    pub(crate) id: String,
    pub(crate) target: String,
    pub(crate) command: Command,
    pub(crate) created_at: Instant,
    pub(crate) last_attempt: Option<Instant>,
    pub(crate) attempts: u32,
    pub(crate) status: OutboxStatus,
}

impl OutboxEntry {
    pub(crate) fn new(id: impl Into<String>, target: impl Into<String>, command: Command) -> Self {
        Self {
            id: id.into(),
            target: target.into(),
            command,
            created_at: Instant::now(),
            last_attempt: None,
            attempts: 0,
            status: OutboxStatus::Pending,
        }
    }

    fn ack_timed_out(&self, now: Instant) -> bool {
        self.status == OutboxStatus::Sent
            && self.last_attempt.is_some_and(|attempt| {
                now.duration_since(attempt) >= Duration::from_secs(ACK_TIMEOUT_SECS)
            })
    }

    fn retry_backoff_elapsed(&self, now: Instant, base_backoff_secs: u64) -> bool {
        match self.last_attempt {
            None => true,
            Some(last_attempt) => {
                let exponent = self.attempts.min(31);
                let multiplier = 1u64 << exponent;
                let backoff_secs = base_backoff_secs.saturating_mul(multiplier);
                now.duration_since(last_attempt) >= Duration::from_secs(backoff_secs)
            }
        }
    }

    fn is_expired(&self, now: Instant, expiry_secs: u64) -> bool {
        now.duration_since(self.created_at) >= Duration::from_secs(expiry_secs)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OutboxResult {
    Sent {
        id: String,
        target: String,
        attempt: u32,
    },
    RetryScheduled {
        id: String,
        target: String,
        attempt: u32,
    },
    Acked {
        id: String,
        target: String,
        attempts: u32,
    },
    Failed {
        id: String,
        target: String,
        attempts: u32,
        reason: String,
    },
    Expired {
        id: String,
        target: String,
    },
}

#[derive(Debug)]
pub(crate) struct DeadLetterEntry {
    pub(crate) original_message: OutboxEntry,
    pub(crate) failure_reason: String,
    pub(crate) failed_at: Instant,
}

#[derive(Debug)]
pub(crate) struct DeadLetterQueue {
    entries: VecDeque<DeadLetterEntry>,
    max_size: usize,
}

impl DeadLetterQueue {
    pub(crate) fn new(max_size: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            max_size,
        }
    }

    pub(crate) fn push(&mut self, original_message: OutboxEntry, failure_reason: String) {
        if self.entries.len() >= self.max_size {
            self.entries.pop_front();
        }
        self.entries.push_back(DeadLetterEntry {
            original_message,
            failure_reason,
            failed_at: Instant::now(),
        });
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

impl Default for DeadLetterQueue {
    fn default() -> Self {
        Self::new(100)
    }
}

#[derive(Debug)]
pub(crate) struct Outbox {
    messages: VecDeque<OutboxEntry>,
    dead_letters: DeadLetterQueue,
    max_retries: u32,
    retry_backoff_secs: u64,
    ack_timeout_secs: u64,
    message_expiry_secs: u64,
}

impl Default for Outbox {
    fn default() -> Self {
        Self {
            messages: VecDeque::new(),
            dead_letters: DeadLetterQueue::default(),
            max_retries: 3,
            retry_backoff_secs: 5,
            ack_timeout_secs: ACK_TIMEOUT_SECS,
            message_expiry_secs: MESSAGE_EXPIRY_SECS,
        }
    }
}

impl Outbox {
    pub(crate) fn enqueue(&mut self, entry: OutboxEntry) {
        self.messages.push_back(entry);
    }

    pub(crate) fn acknowledge(&mut self, message_id: &str) -> Option<OutboxResult> {
        let entry = self
            .messages
            .iter_mut()
            .find(|entry| entry.id == message_id && entry.status == OutboxStatus::Sent)?;
        entry.status = OutboxStatus::Acked;
        Some(OutboxResult::Acked {
            id: entry.id.clone(),
            target: entry.target.clone(),
            attempts: entry.attempts,
        })
    }

    pub(crate) fn mark_delivery_failed(
        &mut self,
        message_id: &str,
        reason: impl Into<String>,
    ) -> Option<OutboxResult> {
        let reason = reason.into();
        let index = self
            .messages
            .iter()
            .position(|entry| entry.id == message_id)?;
        let mut entry = self.messages.remove(index)?;
        entry.status = OutboxStatus::Failed;
        let result = OutboxResult::Failed {
            id: entry.id.clone(),
            target: entry.target.clone(),
            attempts: entry.attempts,
            reason: reason.clone(),
        };
        self.dead_letters.push(entry, reason);
        Some(result)
    }

    pub(crate) fn process(
        &mut self,
        shim_handles: &mut HashMap<String, AgentHandle>,
    ) -> Vec<OutboxResult> {
        let now = Instant::now();
        let mut results = Vec::new();

        for entry in &mut self.messages {
            if matches!(
                entry.status,
                OutboxStatus::Acked | OutboxStatus::Failed | OutboxStatus::Expired
            ) {
                continue;
            }

            if entry.is_expired(now, self.message_expiry_secs) {
                entry.status = OutboxStatus::Expired;
                results.push(OutboxResult::Expired {
                    id: entry.id.clone(),
                    target: entry.target.clone(),
                });
                continue;
            }

            if entry.ack_timed_out(now) {
                if entry.attempts >= self.max_retries {
                    entry.status = OutboxStatus::Failed;
                    results.push(OutboxResult::Failed {
                        id: entry.id.clone(),
                        target: entry.target.clone(),
                        attempts: entry.attempts,
                        reason: format!("timed out waiting {}s for ACK", self.ack_timeout_secs),
                    });
                } else {
                    entry.status = OutboxStatus::Pending;
                    results.push(OutboxResult::RetryScheduled {
                        id: entry.id.clone(),
                        target: entry.target.clone(),
                        attempt: entry.attempts + 1,
                    });
                }
            }

            if entry.status != OutboxStatus::Pending
                || !entry.retry_backoff_elapsed(now, self.retry_backoff_secs)
            {
                continue;
            }

            let Some(handle) = shim_handles.get_mut(&entry.target) else {
                continue;
            };

            entry.attempts = entry.attempts.saturating_add(1);
            entry.last_attempt = Some(now);
            match handle.channel.send(&entry.command) {
                Ok(()) => {
                    entry.status = OutboxStatus::Sent;
                    results.push(OutboxResult::Sent {
                        id: entry.id.clone(),
                        target: entry.target.clone(),
                        attempt: entry.attempts,
                    });
                }
                Err(error) => {
                    if entry.attempts >= self.max_retries {
                        entry.status = OutboxStatus::Failed;
                        results.push(OutboxResult::Failed {
                            id: entry.id.clone(),
                            target: entry.target.clone(),
                            attempts: entry.attempts,
                            reason: error.to_string(),
                        });
                    } else {
                        entry.status = OutboxStatus::Pending;
                        results.push(OutboxResult::RetryScheduled {
                            id: entry.id.clone(),
                            target: entry.target.clone(),
                            attempt: entry.attempts + 1,
                        });
                    }
                }
            }
        }

        let mut kept = VecDeque::with_capacity(self.messages.len());
        while let Some(entry) = self.messages.pop_front() {
            match entry.status {
                OutboxStatus::Failed => {
                    let reason = format!("delivery failed after {} attempt(s)", entry.attempts);
                    self.dead_letters.push(entry, reason);
                }
                OutboxStatus::Expired => {}
                OutboxStatus::Acked => {}
                OutboxStatus::Pending | OutboxStatus::Sent => kept.push_back(entry),
            }
        }
        self.messages = kept;

        results
    }

    pub(crate) fn len(&self) -> usize {
        self.messages.len()
    }

    pub(crate) fn dead_letter_len(&self) -> usize {
        self.dead_letters.len()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::shim::protocol::{Channel, Command, socketpair};

    use super::*;

    fn send_message_command(id: &str) -> Command {
        Command::SendMessage {
            from: "manager".into(),
            body: "do the work".into(),
            message_id: Some(id.into()),
        }
    }

    fn make_handle(member: &str) -> (AgentHandle, Channel) {
        let (parent, child) = socketpair().unwrap();
        let handle = AgentHandle::new(
            member.to_string(),
            Channel::new(parent),
            42,
            "claude".into(),
            "claude".into(),
            PathBuf::from("/tmp"),
        );
        (handle, Channel::new(child))
    }

    #[test]
    fn process_sends_pending_message() {
        let (handle, mut receiver) = make_handle("eng-1");
        let mut handles = HashMap::from([("eng-1".to_string(), handle)]);
        let mut outbox = Outbox::default();
        outbox.enqueue(OutboxEntry::new(
            "msg-1",
            "eng-1",
            send_message_command("msg-1"),
        ));

        let results = outbox.process(&mut handles);
        assert_eq!(
            results,
            vec![OutboxResult::Sent {
                id: "msg-1".into(),
                target: "eng-1".into(),
                attempt: 1,
            }]
        );

        let delivered: Command = receiver.recv::<Command>().unwrap().unwrap();
        match delivered {
            Command::SendMessage { message_id, .. } => {
                assert_eq!(message_id.as_deref(), Some("msg-1"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert_eq!(outbox.len(), 1);
    }

    #[test]
    fn acknowledge_removes_entry_on_next_process() {
        let (handle, _receiver) = make_handle("eng-1");
        let mut handles = HashMap::from([("eng-1".to_string(), handle)]);
        let mut outbox = Outbox::default();
        outbox.enqueue(OutboxEntry::new(
            "msg-1",
            "eng-1",
            send_message_command("msg-1"),
        ));

        let _ = outbox.process(&mut handles);
        let ack = outbox.acknowledge("msg-1").unwrap();
        assert_eq!(
            ack,
            OutboxResult::Acked {
                id: "msg-1".into(),
                target: "eng-1".into(),
                attempts: 1,
            }
        );

        let results = outbox.process(&mut handles);
        assert!(results.is_empty());
        assert_eq!(outbox.len(), 0);
    }

    #[test]
    fn sent_message_requeues_after_ack_timeout() {
        let (handle, _receiver) = make_handle("eng-1");
        let mut handles = HashMap::from([("eng-1".to_string(), handle)]);
        let mut outbox = Outbox::default();
        let mut entry = OutboxEntry::new("msg-1", "eng-1", send_message_command("msg-1"));
        entry.status = OutboxStatus::Sent;
        entry.attempts = 1;
        entry.last_attempt = Some(Instant::now() - Duration::from_secs(ACK_TIMEOUT_SECS + 1));
        outbox.enqueue(entry);

        let results = outbox.process(&mut handles);
        assert_eq!(
            results,
            vec![OutboxResult::RetryScheduled {
                id: "msg-1".into(),
                target: "eng-1".into(),
                attempt: 2,
            }]
        );
        assert_eq!(outbox.len(), 1);
    }

    #[test]
    fn expired_message_is_dropped() {
        let mut outbox = Outbox::default();
        let mut entry = OutboxEntry::new("msg-1", "eng-1", send_message_command("msg-1"));
        entry.created_at = Instant::now() - Duration::from_secs(MESSAGE_EXPIRY_SECS + 1);
        outbox.enqueue(entry);

        let results = outbox.process(&mut HashMap::new());
        assert_eq!(
            results,
            vec![OutboxResult::Expired {
                id: "msg-1".into(),
                target: "eng-1".into(),
            }]
        );
        assert_eq!(outbox.len(), 0);
    }

    #[test]
    fn explicit_delivery_failure_moves_to_dead_letter_queue() {
        let mut outbox = Outbox::default();
        outbox.enqueue(OutboxEntry::new(
            "msg-1",
            "eng-1",
            send_message_command("msg-1"),
        ));

        let result = outbox
            .mark_delivery_failed("msg-1", "shim rejected delivery")
            .unwrap();
        assert_eq!(
            result,
            OutboxResult::Failed {
                id: "msg-1".into(),
                target: "eng-1".into(),
                attempts: 0,
                reason: "shim rejected delivery".into(),
            }
        );
        assert_eq!(outbox.dead_letter_len(), 1);
        assert_eq!(outbox.len(), 0);
    }
}
