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
    #[error("git command not found or failed to execute: {0}")]
    Exec(#[from] std::io::Error),
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
    #[error("kanban-md not found or failed to execute: {0}")]
    Exec(#[from] std::io::Error),
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
}
