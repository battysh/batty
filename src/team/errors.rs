use thiserror::Error;

#[derive(Debug, Error)]
pub enum GitError {
    #[error("transient git error: {message}")]
    Transient { message: String, stderr: String },
    #[error("permanent git error: {message}")]
    Permanent { message: String, stderr: String },
    #[error("git rebase failed for '{branch}': {stderr}")]
    RebaseFailed { branch: String, stderr: String },
    #[error("git merge failed for '{branch}': {stderr}")]
    MergeFailed { branch: String, stderr: String },
    #[error("git rev-parse failed for '{spec}': {stderr}")]
    RevParseFailed { spec: String, stderr: String },
    #[error("invalid git rev-list count for '{range}': {output}")]
    InvalidRevListCount { range: String, output: String },
    #[error("failed to execute git command `{command}`: {source}")]
    Exec {
        command: String,
        #[source]
        source: std::io::Error,
    },
}

impl GitError {
    pub fn is_transient(&self) -> bool {
        matches!(self, GitError::Transient { .. })
    }
}

#[derive(Debug, Error)]
pub enum BoardError {
    #[error("transient board error: {message}")]
    Transient { message: String, stderr: String },
    #[error("permanent board error: {message}")]
    Permanent { message: String, stderr: String },
    #[error("task #{id} not found")]
    TaskNotFound { id: String },
    #[error("task file missing YAML frontmatter: {detail}")]
    InvalidFrontmatter { detail: String },
    #[error("failed to determine claim owner for blocked task #{task_id}")]
    ClaimOwnerUnknown { task_id: String, stderr: String },
    #[error("failed to execute board command `{command}`: {source}")]
    Exec {
        command: String,
        #[source]
        source: std::io::Error,
    },
}

impl BoardError {
    pub fn is_transient(&self) -> bool {
        matches!(self, BoardError::Transient { .. })
    }
}

#[derive(Debug, Error)]
pub enum TmuxError {
    #[error("failed to execute tmux command `{command}`: {source}")]
    Exec {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("tmux command `{command}` failed{target_suffix}: {stderr}")]
    CommandFailed {
        command: String,
        target: Option<String>,
        stderr: String,
        target_suffix: String,
    },
    #[error("tmux session '{session}' already exists")]
    SessionExists { session: String },
    #[error("tmux session '{session}' not found")]
    SessionNotFound { session: String },
    #[error("tmux returned empty pane id for target '{target}'")]
    EmptyPaneId { target: String },
    #[error("tmux returned empty {field} for target '{target}'")]
    EmptyField { target: String, field: &'static str },
}

impl TmuxError {
    pub fn command_failed(command: impl Into<String>, target: Option<&str>, stderr: &str) -> Self {
        let target = target.map(ToOwned::to_owned);
        let target_suffix = target
            .as_deref()
            .map(|value| format!(" for '{value}'"))
            .unwrap_or_default();
        Self::CommandFailed {
            command: command.into(),
            target,
            stderr: stderr.to_string(),
            target_suffix,
        }
    }

    pub fn exec(command: impl Into<String>, source: std::io::Error) -> Self {
        Self::Exec {
            command: command.into(),
            source,
        }
    }
}

#[derive(Debug, Error)]
pub enum DeliveryError {
    #[error("unsupported delivery channel type '{channel_type}'")]
    UnsupportedChannel { channel_type: String },
    #[error("failed to execute delivery provider '{provider}': {source}")]
    ProviderExec {
        provider: String,
        #[source]
        source: std::io::Error,
    },
    #[error("channel delivery failed for '{recipient}': {detail}")]
    ChannelSend { recipient: String, detail: String },
    #[error("live pane delivery failed for '{recipient}' via pane '{pane_id}': {detail}")]
    PaneInject {
        recipient: String,
        pane_id: String,
        detail: String,
    },
    #[error("failed to queue inbox delivery for '{recipient}': {detail}")]
    InboxQueue { recipient: String, detail: String },
}

impl DeliveryError {
    pub fn is_transient(&self) -> bool {
        match self {
            Self::UnsupportedChannel { .. } => false,
            Self::ProviderExec { source, .. } => matches!(
                source.kind(),
                std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::Interrupted
                    | std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::NotConnected
            ),
            Self::ChannelSend { detail, .. } | Self::PaneInject { detail, .. } => {
                detail_is_transient(detail)
            }
            Self::InboxQueue { .. } => false,
        }
    }
}

fn detail_is_transient(detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    [
        "429",
        "too many requests",
        "timeout",
        "timed out",
        "temporary",
        "temporarily unavailable",
        "connection reset",
        "connection aborted",
        "try again",
        "retry after",
        "network",
    ]
    .iter()
    .any(|needle| detail.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_error_marks_only_transient_variants_retryable() {
        assert!(
            GitError::Transient {
                message: "lock".to_string(),
                stderr: "lock".to_string(),
            }
            .is_transient()
        );
        assert!(
            !GitError::Permanent {
                message: "fatal".to_string(),
                stderr: "fatal".to_string(),
            }
            .is_transient()
        );
        assert!(
            !GitError::RebaseFailed {
                branch: "topic".to_string(),
                stderr: "conflict".to_string(),
            }
            .is_transient()
        );
    }

    #[test]
    fn board_error_marks_only_transient_variants_retryable() {
        assert!(
            BoardError::Transient {
                message: "lock".to_string(),
                stderr: "lock".to_string(),
            }
            .is_transient()
        );
        assert!(
            !BoardError::TaskNotFound {
                id: "123".to_string()
            }
            .is_transient()
        );
    }

    #[test]
    fn tmux_command_failed_formats_target_suffix() {
        let error = TmuxError::command_failed("send-keys", Some("%1"), "pane missing");
        assert!(error.to_string().contains("for '%1'"));
    }

    #[test]
    fn delivery_error_marks_transient_channel_failures_retryable() {
        assert!(
            DeliveryError::ChannelSend {
                recipient: "human".to_string(),
                detail: "429 too many requests".to_string(),
            }
            .is_transient()
        );
        assert!(
            !DeliveryError::ChannelSend {
                recipient: "human".to_string(),
                detail: "chat not found".to_string(),
            }
            .is_transient()
        );
    }

    // --- Error path and recovery tests (Task #265) ---

    #[test]
    fn delivery_error_unsupported_channel_is_never_transient() {
        let error = DeliveryError::UnsupportedChannel {
            channel_type: "smoke_signal".to_string(),
        };
        assert!(!error.is_transient());
        assert!(error.to_string().contains("smoke_signal"));
    }

    #[test]
    fn delivery_error_provider_exec_timeout_is_transient() {
        let error = DeliveryError::ProviderExec {
            provider: "telegram".to_string(),
            source: std::io::Error::new(std::io::ErrorKind::TimedOut, "connection timed out"),
        };
        assert!(error.is_transient());
    }

    #[test]
    fn delivery_error_provider_exec_not_found_is_permanent() {
        let error = DeliveryError::ProviderExec {
            provider: "telegram".to_string(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "binary not found"),
        };
        assert!(!error.is_transient());
    }

    #[test]
    fn delivery_error_provider_exec_interrupted_is_transient() {
        let error = DeliveryError::ProviderExec {
            provider: "telegram".to_string(),
            source: std::io::Error::new(std::io::ErrorKind::Interrupted, "signal received"),
        };
        assert!(error.is_transient());
    }

    #[test]
    fn delivery_error_provider_exec_connection_reset_is_transient() {
        let error = DeliveryError::ProviderExec {
            provider: "telegram".to_string(),
            source: std::io::Error::new(std::io::ErrorKind::ConnectionReset, "peer reset"),
        };
        assert!(error.is_transient());
    }

    #[test]
    fn delivery_error_pane_inject_transient_detail() {
        let error = DeliveryError::PaneInject {
            recipient: "eng-1".to_string(),
            pane_id: "%5".to_string(),
            detail: "connection reset by peer".to_string(),
        };
        assert!(error.is_transient());
        assert!(error.to_string().contains("%5"));
    }

    #[test]
    fn delivery_error_pane_inject_permanent_detail() {
        let error = DeliveryError::PaneInject {
            recipient: "eng-1".to_string(),
            pane_id: "%5".to_string(),
            detail: "pane not found".to_string(),
        };
        assert!(!error.is_transient());
    }

    #[test]
    fn delivery_error_inbox_queue_is_never_transient() {
        let error = DeliveryError::InboxQueue {
            recipient: "eng-1".to_string(),
            detail: "disk full".to_string(),
        };
        assert!(!error.is_transient());
        assert!(error.to_string().contains("eng-1"));
    }

    #[test]
    fn delivery_error_channel_send_timeout_is_transient() {
        let error = DeliveryError::ChannelSend {
            recipient: "human".to_string(),
            detail: "request timed out waiting for response".to_string(),
        };
        assert!(error.is_transient());
    }

    #[test]
    fn delivery_error_channel_send_network_is_transient() {
        let error = DeliveryError::ChannelSend {
            recipient: "human".to_string(),
            detail: "network unreachable".to_string(),
        };
        assert!(error.is_transient());
    }

    #[test]
    fn delivery_error_channel_send_retry_after_is_transient() {
        let error = DeliveryError::ChannelSend {
            recipient: "human".to_string(),
            detail: "retry after 30 seconds".to_string(),
        };
        assert!(error.is_transient());
    }

    #[test]
    fn git_error_exec_display_includes_command() {
        let error = GitError::Exec {
            command: "git -C /repo merge main".to_string(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "git not found"),
        };
        assert!(error.to_string().contains("git -C /repo merge main"));
        assert!(!error.is_transient());
    }

    #[test]
    fn git_error_rebase_failed_not_transient() {
        let error = GitError::RebaseFailed {
            branch: "feature-x".to_string(),
            stderr: "CONFLICT (content): Merge conflict in src/main.rs".to_string(),
        };
        assert!(!error.is_transient());
        assert!(error.to_string().contains("feature-x"));
    }

    #[test]
    fn git_error_merge_failed_not_transient() {
        let error = GitError::MergeFailed {
            branch: "topic".to_string(),
            stderr: "Automatic merge failed".to_string(),
        };
        assert!(!error.is_transient());
        assert!(error.to_string().contains("topic"));
    }

    #[test]
    fn git_error_rev_parse_failed_not_transient() {
        let error = GitError::RevParseFailed {
            spec: "HEAD~5".to_string(),
            stderr: "unknown revision".to_string(),
        };
        assert!(!error.is_transient());
        assert!(error.to_string().contains("HEAD~5"));
    }

    #[test]
    fn git_error_invalid_rev_list_count_not_transient() {
        let error = GitError::InvalidRevListCount {
            range: "main..feature".to_string(),
            output: "not-a-number".to_string(),
        };
        assert!(!error.is_transient());
        assert!(error.to_string().contains("main..feature"));
    }

    #[test]
    fn board_error_permanent_not_transient() {
        let error = BoardError::Permanent {
            message: "unknown command".to_string(),
            stderr: "bad args".to_string(),
        };
        assert!(!error.is_transient());
    }

    #[test]
    fn board_error_exec_not_transient() {
        let error = BoardError::Exec {
            command: "kanban-md list".to_string(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "not found"),
        };
        assert!(!error.is_transient());
        assert!(error.to_string().contains("kanban-md list"));
    }

    #[test]
    fn board_error_invalid_frontmatter_not_transient() {
        let error = BoardError::InvalidFrontmatter {
            detail: "missing status field".to_string(),
        };
        assert!(!error.is_transient());
        assert!(error.to_string().contains("missing status field"));
    }

    #[test]
    fn board_error_claim_owner_unknown_not_transient() {
        let error = BoardError::ClaimOwnerUnknown {
            task_id: "42".to_string(),
            stderr: "is claimed by unknown".to_string(),
        };
        assert!(!error.is_transient());
        assert!(error.to_string().contains("42"));
    }

    #[test]
    fn tmux_error_session_exists_format() {
        let error = TmuxError::SessionExists {
            session: "batty-test".to_string(),
        };
        assert!(error.to_string().contains("batty-test"));
        assert!(error.to_string().contains("already exists"));
    }

    #[test]
    fn tmux_error_session_not_found_format() {
        let error = TmuxError::SessionNotFound {
            session: "batty-test".to_string(),
        };
        assert!(error.to_string().contains("batty-test"));
        assert!(error.to_string().contains("not found"));
    }

    #[test]
    fn tmux_error_empty_pane_id_format() {
        let error = TmuxError::EmptyPaneId {
            target: "batty-session:0".to_string(),
        };
        assert!(error.to_string().contains("batty-session:0"));
        assert!(error.to_string().contains("empty pane id"));
    }

    #[test]
    fn tmux_error_empty_field_format() {
        let error = TmuxError::EmptyField {
            target: "%5".to_string(),
            field: "pane_pid",
        };
        assert!(error.to_string().contains("%5"));
        assert!(error.to_string().contains("pane_pid"));
    }

    #[test]
    fn tmux_error_command_failed_without_target() {
        let error = TmuxError::command_failed("list-sessions", None, "server not found");
        let msg = error.to_string();
        assert!(msg.contains("list-sessions"));
        assert!(msg.contains("server not found"));
        assert!(!msg.contains("for '"));
    }

    #[test]
    fn tmux_error_exec_format() {
        let error = TmuxError::exec(
            "tmux new-session",
            std::io::Error::new(std::io::ErrorKind::NotFound, "tmux not found"),
        );
        assert!(error.to_string().contains("tmux new-session"));
    }

    #[test]
    fn detail_is_transient_covers_all_keywords() {
        assert!(detail_is_transient("HTTP 429 rate limit"));
        assert!(detail_is_transient("Too Many Requests"));
        assert!(detail_is_transient("request timeout"));
        assert!(detail_is_transient("connection timed out"));
        assert!(detail_is_transient("temporary failure"));
        assert!(detail_is_transient("temporarily unavailable"));
        assert!(detail_is_transient("connection reset by peer"));
        assert!(detail_is_transient("connection aborted"));
        assert!(detail_is_transient("please try again later"));
        assert!(detail_is_transient("Retry After: 30"));
        assert!(detail_is_transient("network error"));
        // Permanent errors
        assert!(!detail_is_transient("chat not found"));
        assert!(!detail_is_transient("invalid token"));
        assert!(!detail_is_transient("forbidden"));
        assert!(!detail_is_transient(""));
    }
}
