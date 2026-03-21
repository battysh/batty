#![cfg_attr(not(test), allow(dead_code))]

use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum BoardError {
    #[error("transient board error: {message}")]
    Transient { message: String, stderr: String },
    #[error("permanent board error: {message}")]
    Permanent { message: String, stderr: String },
    #[error("kanban-md not found or failed to execute: {0}")]
    Exec(#[from] std::io::Error),
}

impl BoardError {
    pub fn is_transient(&self) -> bool {
        matches!(self, BoardError::Transient { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardOutput {
    pub stdout: String,
    pub stderr: String,
}

pub fn run_board(board_dir: &Path, args: &[&str]) -> Result<BoardOutput, BoardError> {
    run_board_with_program("kanban-md", board_dir, args)
}

pub fn init(board_dir: &Path) -> Result<BoardOutput, BoardError> {
    run_board(board_dir, &["init"])
}

pub fn move_task(
    board_dir: &Path,
    task_id: &str,
    status: &str,
    claim: Option<&str>,
) -> Result<(), BoardError> {
    let mut args = vec!["move".to_string(), task_id.to_string(), status.to_string()];
    if let Some(claim) = claim {
        args.push("--claim".to_string());
        args.push(claim.to_string());
    }
    run_board_owned(board_dir, &args).map(|_| ())
}

pub fn edit_task(board_dir: &Path, task_id: &str, block_reason: &str) -> Result<(), BoardError> {
    match run_board(board_dir, &["edit", task_id, "--block", block_reason]) {
        Ok(_) => Ok(()),
        Err(BoardError::Permanent { stderr, .. }) if claim_required_for_edit(&stderr) => {
            let claim = show_task(board_dir, task_id)
                .ok()
                .and_then(|task| extract_claimed_by(&task));
            if let Some(claim) = claim.as_deref() {
                run_board(
                    board_dir,
                    &["edit", task_id, "--block", block_reason, "--claim", claim],
                )
                .map(|_| ())
            } else {
                Err(BoardError::Permanent {
                    message: format!("failed to determine claim owner for blocked task #{task_id}"),
                    stderr,
                })
            }
        }
        Err(error) => Err(error),
    }
}

pub fn pick_task(
    board_dir: &Path,
    claim: &str,
    move_to: &str,
) -> Result<Option<String>, BoardError> {
    match run_board(board_dir, &["pick", "--claim", claim, "--move", move_to]) {
        Ok(output) => Ok(extract_task_id(&output.stdout, "Picked and moved task #")
            .or_else(|| extract_task_id(&output.stdout, "Picked task #"))),
        Err(BoardError::Permanent { stderr, .. }) if is_empty_pick(&stderr) => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn show_task(board_dir: &Path, task_id: &str) -> Result<String, BoardError> {
    run_board(board_dir, &["show", task_id]).map(|output| output.stdout)
}

pub fn list_tasks(board_dir: &Path, status: Option<&str>) -> Result<String, BoardError> {
    let mut args = vec!["list".to_string()];
    if let Some(status) = status {
        args.push("--status".to_string());
        args.push(status.to_string());
    }
    run_board_owned(board_dir, &args).map(|output| output.stdout)
}

pub fn create_task(
    board_dir: &Path,
    title: &str,
    body: &str,
    priority: Option<&str>,
    tags: Option<&str>,
    depends_on: Option<&str>,
) -> Result<String, BoardError> {
    let mut args = vec![
        "create".to_string(),
        title.to_string(),
        "--body".to_string(),
        body.to_string(),
    ];
    if let Some(priority) = priority {
        args.push("--priority".to_string());
        args.push(priority.to_string());
    }
    if let Some(tags) = tags {
        args.push("--tags".to_string());
        args.push(tags.to_string());
    }
    if let Some(depends_on) = depends_on {
        args.push("--depends-on".to_string());
        args.push(depends_on.to_string());
    }

    let output = run_board_owned(board_dir, &args)?;
    extract_task_id(&output.stdout, "Created task #").ok_or_else(|| BoardError::Permanent {
        message: format!(
            "failed to parse task ID from create output: {}",
            output.stdout.lines().next().unwrap_or_default()
        ),
        stderr: output.stderr,
    })
}

fn run_board_owned(board_dir: &Path, args: &[String]) -> Result<BoardOutput, BoardError> {
    let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    run_board(board_dir, &arg_refs)
}

fn run_board_with_program(
    program: &str,
    board_dir: &Path,
    args: &[&str],
) -> Result<BoardOutput, BoardError> {
    let current_dir = board_dir
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let output = Command::new(program)
        .current_dir(current_dir)
        .args(build_board_args(board_dir, args))
        .output()?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if output.status.success() {
        return Ok(BoardOutput { stdout, stderr });
    }

    let status = output.status.code().map_or_else(
        || "terminated by signal".to_string(),
        |code| format!("exit code {code}"),
    );
    let message = format!("`{program} {}` failed with {status}", args.join(" "));
    Err(classify_failure(message, stderr))
}

fn build_board_args(board_dir: &Path, args: &[&str]) -> Vec<OsString> {
    let mut assembled = args.iter().map(OsString::from).collect::<Vec<_>>();
    assembled.push(OsString::from("--dir"));
    assembled.push(board_dir.as_os_str().to_owned());
    assembled
}

fn classify_failure(message: String, stderr: String) -> BoardError {
    if is_transient_stderr(&stderr) {
        BoardError::Transient { message, stderr }
    } else {
        BoardError::Permanent { message, stderr }
    }
}

fn is_transient_stderr(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    contains_word(&lower, "lock")
        || lower.contains("resource temporarily unavailable")
        || lower.contains("no such file or directory")
        || lower.contains("i/o error")
        || lower.contains("try again")
}

fn is_empty_pick(stderr: &str) -> bool {
    stderr
        .to_ascii_lowercase()
        .contains("no unblocked, unclaimed tasks found")
}

fn claim_required_for_edit(stderr: &str) -> bool {
    stderr.to_ascii_lowercase().contains("is claimed by")
}

fn contains_word(text: &str, needle: &str) -> bool {
    text.split(|ch: char| !ch.is_ascii_alphabetic())
        .any(|word| word == needle)
}

fn extract_claimed_by(task_output: &str) -> Option<String> {
    for line in task_output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Claimed by:") {
            let claim = rest.trim();
            if claim == "--" {
                return None;
            }
            let claim = claim.split(" (").next().unwrap_or(claim).trim();
            if !claim.is_empty() {
                return Some(claim.to_string());
            }
        }
    }
    None
}

fn extract_task_id(text: &str, prefix: &str) -> Option<String> {
    let start = text.find(prefix)? + prefix.len();
    let digits = text[start..]
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    if digits.is_empty() {
        None
    } else {
        Some(digits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
    use std::sync::{LazyLock, Mutex};

    use tempfile::TempDir;

    static CWD_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn with_process_cwd<T>(cwd: &Path, action: impl FnOnce() -> T) -> T {
        let _guard = CWD_LOCK.lock().unwrap();
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(cwd).unwrap();
        let result = catch_unwind(AssertUnwindSafe(action));
        std::env::set_current_dir(original).unwrap();
        match result {
            Ok(value) => value,
            Err(panic) => resume_unwind(panic),
        }
    }

    fn with_live_board_cwd<T>(action: impl FnOnce() -> T) -> T {
        let live_repo = TempDir::new().unwrap();
        let live_board_dir = live_repo.path().join(".batty/team_config/board");
        fs::create_dir_all(live_board_dir.parent().unwrap()).unwrap();
        init(&live_board_dir).unwrap();

        let nested_cwd = live_repo.path().join("nested/project");
        fs::create_dir_all(&nested_cwd).unwrap();
        with_process_cwd(&nested_cwd, action)
    }

    #[test]
    fn classifies_transient_errors() {
        let error = classify_failure(
            "board command failed".to_string(),
            "resource temporarily unavailable".to_string(),
        );
        assert!(matches!(error, BoardError::Transient { .. }));
    }

    #[test]
    fn lock_detection_uses_whole_words() {
        assert!(is_transient_stderr("board lock is busy"));
        assert!(!is_transient_stderr("no unblocked, unclaimed tasks found"));
    }

    #[test]
    fn classifies_permanent_errors() {
        let error = classify_failure(
            "board command failed".to_string(),
            "unknown command \"wat\" for \"kanban-md\"".to_string(),
        );
        assert!(matches!(error, BoardError::Permanent { .. }));
    }

    #[test]
    fn board_error_reports_transience() {
        let transient = BoardError::Transient {
            message: "temporary".to_string(),
            stderr: "lock busy".to_string(),
        };
        let permanent = BoardError::Permanent {
            message: "permanent".to_string(),
            stderr: "bad args".to_string(),
        };
        assert!(transient.is_transient());
        assert!(!permanent.is_transient());
    }

    #[test]
    fn missing_program_returns_exec_error() {
        let temp = TempDir::new().unwrap();
        let error =
            run_board_with_program("__batty_missing_kanban__", temp.path(), &["list"]).unwrap_err();
        assert!(matches!(error, BoardError::Exec(_)));
    }

    #[test]
    fn build_board_args_appends_dir_flag() {
        let args = build_board_args(Path::new("/tmp/test board"), &["move", "60", "review"]);
        let rendered = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            rendered,
            vec!["move", "60", "review", "--dir", "/tmp/test board"]
        );
    }

    #[test]
    fn run_board_uses_board_parent_as_cwd() {
        let temp = TempDir::new().unwrap();
        let board_dir = temp.path().join("board");
        let script_path = temp.path().join("fake-kanban.sh");
        let output_path = temp.path().join("cwd.txt");
        let script = format!("#!/bin/sh\npwd > \"{}\"\n", output_path.display());
        fs::write(&script_path, script).unwrap();
        #[cfg(unix)]
        {
            let mut permissions = fs::metadata(&script_path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&script_path, permissions).unwrap();
        }

        run_board_with_program(script_path.to_str().unwrap(), &board_dir, &["list"]).unwrap();

        let cwd = fs::canonicalize(fs::read_to_string(&output_path).unwrap().trim()).unwrap();
        let expected = fs::canonicalize(temp.path()).unwrap();
        assert_eq!(cwd, expected);
    }

    #[test]
    fn extracts_claim_owner_from_show_output() {
        let output = "Claimed by:  eng-1-2 (since 2026-03-21 01:11)";
        assert_eq!(extract_claimed_by(output).as_deref(), Some("eng-1-2"));
        assert_eq!(extract_claimed_by("Claimed by:  --"), None);
    }

    #[test]
    fn init_create_show_round_trip_when_kanban_available() {
        if Command::new("kanban-md").arg("--version").output().is_err() {
            return;
        }

        with_live_board_cwd(|| {
            let temp = TempDir::new().unwrap();
            let board_dir = temp.path().join("board");

            let init_output = init(&board_dir).unwrap();
            assert!(board_dir.is_dir());
            assert!(init_output.stdout.contains("Initialized board"));

            let task_id = create_task(
                &board_dir,
                "Test task",
                "Body text",
                Some("high"),
                Some("phase-8,wave-1"),
                None,
            )
            .unwrap();
            assert_eq!(task_id, "1");

            let show = show_task(&board_dir, &task_id).unwrap();
            assert!(show.contains("Task #1: Test task"));
            assert!(show.contains("Body text"));

            let list = list_tasks(&board_dir, Some("backlog")).unwrap();
            assert!(list.contains("Test task"));

            let picked = pick_task(&board_dir, "eng-1-2", "in-progress").unwrap();
            assert_eq!(picked.as_deref(), Some("1"));

            move_task(&board_dir, &task_id, "review", Some("eng-1-2")).unwrap();
            let review = show_task(&board_dir, &task_id).unwrap();
            assert!(review.contains("Status:      review"));

            edit_task(&board_dir, &task_id, "needs manager input").unwrap();
            let task_file = fs::read_to_string(board_dir.join("tasks/001-test-task.md")).unwrap();
            assert!(task_file.contains("block_reason: needs manager input"));
        });
    }

    #[test]
    fn pick_task_returns_none_when_board_is_empty() {
        if Command::new("kanban-md").arg("--version").output().is_err() {
            return;
        }

        with_live_board_cwd(|| {
            let temp = TempDir::new().unwrap();
            let board_dir = temp.path().join("board");
            init(&board_dir).unwrap();

            let picked = pick_task(&board_dir, "eng-1-2", "in-progress").unwrap();
            assert_eq!(picked, None);
        });
    }
}
