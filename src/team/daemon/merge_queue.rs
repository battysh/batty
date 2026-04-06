use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;

use super::TeamDaemon;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MergeRequest {
    pub task_id: u32,
    pub engineer: String,
    pub branch: String,
    pub worktree_dir: PathBuf,
    pub queued_at: Instant,
    pub test_passed: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MergeQueueOutcome {
    Success,
    Conflict,
    Reverted,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MergeQueueEvent {
    pub task_id: u32,
    pub engineer: String,
    pub outcome: MergeQueueOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MergeQueueLastResult {
    task_id: u32,
    outcome: MergeQueueOutcome,
    finished_at: Instant,
}

#[derive(Debug, Default)]
pub(crate) struct MergeQueue {
    queue: VecDeque<MergeRequest>,
    active: Option<MergeRequest>,
    last_result: Option<MergeQueueLastResult>,
    last_reported_status: Option<String>,
}

impl MergeQueue {
    pub(crate) fn enqueue(&mut self, request: MergeRequest) {
        self.queue.push_back(request);
    }

    pub(crate) fn queued_len(&self) -> usize {
        self.queue.len()
    }

    pub(crate) fn active_task_id(&self) -> Option<u32> {
        self.active.as_ref().map(|request| request.task_id)
    }

    pub(crate) fn process_next<F>(&mut self, mut processor: F) -> Result<Option<MergeQueueEvent>>
    where
        F: FnMut(&MergeRequest) -> Result<MergeQueueOutcome>,
    {
        if self.active.is_some() {
            return Ok(None);
        }

        let Some(request) = self.queue.pop_front() else {
            return Ok(None);
        };

        self.active = Some(request.clone());
        let outcome = match processor(&request) {
            Ok(outcome) => outcome,
            Err(error) => {
                self.active = None;
                return Err(error);
            }
        };
        self.active = None;
        self.last_result = Some(MergeQueueLastResult {
            task_id: request.task_id,
            outcome: outcome.clone(),
            finished_at: Instant::now(),
        });

        Ok(Some(MergeQueueEvent {
            task_id: request.task_id,
            engineer: request.engineer,
            outcome,
        }))
    }

    fn status_line(&self) -> Option<String> {
        if self.queue.is_empty() && self.active.is_none() && self.last_result.is_none() {
            return None;
        }

        let queued = self.queue.len();
        let merging = self
            .active
            .as_ref()
            .map(|request| format!("#{} ({})", request.task_id, request.branch))
            .unwrap_or_else(|| "idle".to_string());
        let last = self
            .last_result
            .as_ref()
            .map(|result| {
                format!(
                    "#{} {} {}s ago",
                    result.task_id,
                    match result.outcome {
                        MergeQueueOutcome::Success => "merged",
                        MergeQueueOutcome::Conflict => "conflicted",
                        MergeQueueOutcome::Reverted => "reverted",
                        MergeQueueOutcome::Failed => "failed",
                    },
                    result.finished_at.elapsed().as_secs()
                )
            })
            .unwrap_or_else(|| "none".to_string());

        Some(format!(
            "[merge] queued: {queued} | merging: {merging} | last: {last}"
        ))
    }

    pub(crate) fn take_status_update(&mut self) -> Option<String> {
        let status = self.status_line()?;
        if self.last_reported_status.as_deref() == Some(status.as_str()) {
            return None;
        }
        self.last_reported_status = Some(status.clone());
        Some(status)
    }
}

impl TeamDaemon {
    pub(super) fn process_merge_queue(&mut self) -> Result<()> {
        if let Some(status) = self.merge_queue.take_status_update() {
            self.record_orchestrator_action(status);
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn enqueue_merge_request(&mut self, request: MergeRequest) {
        self.merge_queue.enqueue(request);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(task_id: u32) -> MergeRequest {
        MergeRequest {
            task_id,
            engineer: "eng-1".to_string(),
            branch: format!("eng-1/task-{task_id}"),
            worktree_dir: PathBuf::from("/tmp/worktree"),
            queued_at: Instant::now(),
            test_passed: true,
        }
    }

    #[test]
    fn process_next_runs_requests_in_fifo_order() {
        let mut queue = MergeQueue::default();
        queue.enqueue(request(41));
        queue.enqueue(request(42));

        let first = queue
            .process_next(|request| {
                assert_eq!(request.task_id, 41);
                Ok(MergeQueueOutcome::Success)
            })
            .unwrap()
            .unwrap();
        let second = queue
            .process_next(|request| {
                assert_eq!(request.task_id, 42);
                Ok(MergeQueueOutcome::Conflict)
            })
            .unwrap()
            .unwrap();

        assert_eq!(first.task_id, 41);
        assert_eq!(second.task_id, 42);
        assert_eq!(queue.queued_len(), 0);
        assert_eq!(queue.active_task_id(), None);
    }

    #[test]
    fn take_status_update_reports_queue_state_changes() {
        let mut queue = MergeQueue::default();
        queue.enqueue(request(41));

        let initial = queue.take_status_update().unwrap();
        assert!(initial.contains("[merge] queued: 1"));
        assert!(queue.take_status_update().is_none());

        queue
            .process_next(|_| Ok(MergeQueueOutcome::Success))
            .unwrap();
        let updated = queue.take_status_update().unwrap();
        assert!(updated.contains("last: #41 merged"));
    }

    #[test]
    fn processor_errors_leave_active_request_cleared() {
        let mut queue = MergeQueue::default();
        queue.enqueue(request(99));

        let error = queue
            .process_next(|_| anyhow::bail!("merge execution failed"))
            .unwrap_err();

        assert!(error.to_string().contains("merge execution failed"));
        assert_eq!(queue.active_task_id(), None);
        assert_eq!(queue.queued_len(), 0);
    }
}
