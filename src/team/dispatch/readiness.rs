//! Pane CWD correction and workspace readiness checks.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use tracing::{debug, warn};

use super::super::super::events::TeamEvent;
use super::super::*;

impl TeamDaemon {
    pub(in super::super) fn ensure_member_pane_cwd(
        &mut self,
        member_name: &str,
        pane_id: &str,
        expected_dir: &Path,
    ) -> Result<()> {
        let current_path = PathBuf::from(tmux::pane_current_path(pane_id)?);
        let normalized_expected = normalized_assignment_dir(expected_dir);
        if normalized_assignment_dir(&current_path) == normalized_expected {
            return Ok(());
        }

        // Codex agents run from {worktree}/.batty/codex-context/{member_name} by
        // design.  Accept that path as a valid CWD so we don't fail assignments
        // when the agent is already running in the correct codex context directory.
        let codex_context_dir = expected_dir
            .join(".batty")
            .join("codex-context")
            .join(member_name);
        if normalized_assignment_dir(&current_path) == normalized_assignment_dir(&codex_context_dir)
        {
            return Ok(());
        }

        warn!(
            member = %member_name,
            pane = %pane_id,
            current = %current_path.display(),
            expected = %expected_dir.display(),
            "correcting pane cwd before agent interaction"
        );

        // Brief delay before sending cd — pane may not be in a ready state after
        // daemon resume (shell prompt not yet drawn).
        std::thread::sleep(Duration::from_millis(100));

        let command = format!(
            "cd '{}'",
            shell_single_quote(expected_dir.to_string_lossy().as_ref())
        );
        tmux::send_keys(pane_id, &command, true)?;

        // Retry verification up to 3 times — the pane may report the old path
        // if checked too quickly after the cd command.
        const CWD_VERIFY_RETRIES: u32 = 3;
        const CWD_VERIFY_DELAY_MS: u64 = 200;
        let mut last_corrected_path = PathBuf::new();

        for attempt in 1..=CWD_VERIFY_RETRIES {
            std::thread::sleep(Duration::from_millis(CWD_VERIFY_DELAY_MS));

            last_corrected_path = PathBuf::from(tmux::pane_current_path(pane_id)?);
            let normalized_corrected = normalized_assignment_dir(&last_corrected_path);
            if normalized_corrected == normalized_expected
                || normalized_corrected == normalized_assignment_dir(&codex_context_dir)
            {
                self.emit_event(TeamEvent::cwd_corrected(
                    member_name,
                    &expected_dir.display().to_string(),
                ));
                return Ok(());
            }

            if attempt < CWD_VERIFY_RETRIES {
                debug!(
                    member = %member_name,
                    attempt,
                    actual = %last_corrected_path.display(),
                    expected = %expected_dir.display(),
                    "cwd correction not yet confirmed, retrying"
                );
            }
        }

        // All retries exhausted — log warning but don't block the assignment.
        warn!(
            member = %member_name,
            pane = %pane_id,
            expected = %expected_dir.display(),
            actual = %last_corrected_path.display(),
            "pane cwd correction failed after {CWD_VERIFY_RETRIES} retries"
        );
        Ok(())
    }
}

pub(in super::super) fn normalized_assignment_dir(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn shell_single_quote(value: &str) -> String {
    value.replace('\'', "'\"'\"'")
}
