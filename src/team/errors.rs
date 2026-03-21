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
        assert!(GitError::Transient {
            message: "lock".to_string(),
            stderr: "lock".to_string(),
        }
        .is_transient());
        assert!(!GitError::Permanent {
            message: "fatal".to_string(),
            stderr: "fatal".to_string(),
        }
        .is_transient());
        assert!(!GitError::RebaseFailed {
            branch: "topic".to_string(),
            stderr: "conflict".to_string(),
        }
        .is_transient());
    }

    #[test]
    fn board_error_marks_only_transient_variants_retryable() {
        assert!(BoardError::Transient {
            message: "lock".to_string(),
            stderr: "lock".to_string(),
        }
        .is_transient());
        assert!(!BoardError::TaskNotFound {
            id: "123".to_string()
        }
        .is_transient());
    }

    #[test]
    fn tmux_command_failed_formats_target_suffix() {
        let error = TmuxError::command_failed("send-keys", Some("%1"), "pane missing");
        assert!(error.to_string().contains("for '%1'"));
    }

    #[test]
    fn delivery_error_marks_transient_channel_failures_retryable() {
        assert!(DeliveryError::ChannelSend {
            recipient: "human".to_string(),
            detail: "429 too many requests".to_string(),
        }
        .is_transient());
        assert!(!DeliveryError::ChannelSend {
            recipient: "human".to_string(),
            detail: "chat not found".to_string(),
        }
        .is_transient());
    }
}
