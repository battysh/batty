#![allow(dead_code)]

use std::ffi::OsString;
use std::path::Path;
use std::process::Command;
use tracing::{info, warn};

pub use super::errors::BoardError;

/// YAML frontmatter fields that batty adds but kanban-md doesn't know about.
/// kanban-md move/pick rewrites frontmatter and drops these, so we preserve
/// them around any operation that modifies status.
const SCHEDULING_FIELDS: &[&str] = &["scheduled_for", "cron_schedule", "cron_last_run"];

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
    // Preserve scheduling fields that kanban-md doesn't know about
    let saved_fields = extract_scheduling_fields(board_dir, task_id);

    let mut args = vec!["move".to_string(), task_id.to_string(), status.to_string()];
    if let Some(claim) = claim {
        args.push("--claim".to_string());
        args.push(claim.to_string());
    }
    run_board_owned(board_dir, &args)?;

    // Restore scheduling fields that kanban-md stripped
    restore_scheduling_fields(board_dir, task_id, &saved_fields);
    Ok(())
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
                Err(BoardError::ClaimOwnerUnknown {
                    task_id: task_id.to_string(),
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
    // Save scheduling fields for all tasks before pick (we don't know which will be picked)
    let saved = snapshot_scheduling_fields(board_dir);

    match run_board(board_dir, &["pick", "--claim", claim, "--move", move_to]) {
        Ok(output) => {
            let task_id = extract_task_id(&output.stdout, "Picked and moved task #")
                .or_else(|| extract_task_id(&output.stdout, "Picked task #"));
            // Restore scheduling fields for the picked task
            if let Some(ref id) = task_id {
                if let Some(fields) = saved.get(id.as_str()) {
                    restore_scheduling_fields(board_dir, id, fields);
                }
            }
            Ok(task_id)
        }
        Err(BoardError::Permanent { stderr, .. }) if is_empty_pick(&stderr) => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn show_task(board_dir: &Path, task_id: &str) -> Result<String, BoardError> {
    run_board(board_dir, &["show", task_id])
        .map(|output| output.stdout)
        .map_err(|error| match error {
            BoardError::Permanent { stderr, .. }
                if stderr
                    .to_ascii_lowercase()
                    .contains(&format!("task #{task_id} not found")) =>
            {
                BoardError::TaskNotFound {
                    id: task_id.to_string(),
                }
            }
            other => other,
        })
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

/// Extract scheduling fields from a task file's YAML frontmatter.
/// Returns a map of field_name→field_value for any scheduling fields found.
fn extract_scheduling_fields(board_dir: &Path, task_id: &str) -> Vec<(String, String)> {
    let task_path = match find_task_file(board_dir, task_id) {
        Some(path) => path,
        None => return Vec::new(),
    };
    let content = match std::fs::read_to_string(&task_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    parse_scheduling_fields_from_frontmatter(&content)
}

/// Parse scheduling fields out of raw file content with YAML frontmatter.
fn parse_scheduling_fields_from_frontmatter(content: &str) -> Vec<(String, String)> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return Vec::new();
    }
    let after_open = &trimmed[3..];
    let after_open = after_open.strip_prefix('\n').unwrap_or(after_open);
    let close_pos = match after_open.find("\n---") {
        Some(pos) => pos,
        None => return Vec::new(),
    };
    let frontmatter = &after_open[..close_pos];

    let mut fields = Vec::new();
    for line in frontmatter.lines() {
        for &field in SCHEDULING_FIELDS {
            if let Some(rest) = line.strip_prefix(field) {
                if let Some(value) = rest.strip_prefix(':') {
                    let value = value.trim().trim_matches('"').trim_matches('\'');
                    if !value.is_empty() {
                        fields.push((field.to_string(), value.to_string()));
                    }
                }
            }
        }
    }
    fields
}

/// Re-insert scheduling fields into a task file after kanban-md has rewritten it.
fn restore_scheduling_fields(board_dir: &Path, task_id: &str, fields: &[(String, String)]) {
    if fields.is_empty() {
        return;
    }
    let task_path = match find_task_file(board_dir, task_id) {
        Some(path) => path,
        None => return,
    };
    let content = match std::fs::read_to_string(&task_path) {
        Ok(c) => c,
        Err(_) => return,
    };

    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return;
    }
    let after_open = &trimmed[3..];
    let after_open = after_open.strip_prefix('\n').unwrap_or(after_open);
    let close_pos = match after_open.find("\n---") {
        Some(pos) => pos,
        None => return,
    };

    let frontmatter = &after_open[..close_pos];
    let body = &after_open[close_pos + 4..];

    // Build new frontmatter lines, appending scheduling fields at the end
    let mut lines: Vec<String> = frontmatter.lines().map(|l| l.to_string()).collect();
    for (key, value) in fields {
        // Remove any existing line for this field (shouldn't exist after move, but be safe)
        lines.retain(|l| !l.starts_with(key.as_str()) || !l[key.len()..].starts_with(':'));
        lines.push(format!("{key}: {value}"));
    }

    let mut updated = String::from("---\n");
    for line in &lines {
        updated.push_str(line);
        updated.push('\n');
    }
    updated.push_str("---");
    updated.push_str(body);

    let _ = std::fs::write(&task_path, updated);
}

/// Snapshot scheduling fields for all tasks in the board (used before pick).
fn snapshot_scheduling_fields(
    board_dir: &Path,
) -> std::collections::HashMap<String, Vec<(String, String)>> {
    let tasks_dir = board_dir.join("tasks");
    let mut map = std::collections::HashMap::new();
    let entries = match std::fs::read_dir(&tasks_dir) {
        Ok(e) => e,
        Err(_) => return map,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.ends_with(".md") {
            continue;
        }
        // Extract task ID from filename prefix (e.g., "001-title.md" → "1")
        if let Some(id_str) = name_str.split('-').next() {
            if let Ok(id) = id_str.parse::<u32>() {
                let content = match std::fs::read_to_string(entry.path()) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let fields = parse_scheduling_fields_from_frontmatter(&content);
                if !fields.is_empty() {
                    map.insert(id.to_string(), fields);
                }
            }
        }
    }
    map
}

/// Find a task file by numeric ID in the board's tasks directory.
fn find_task_file(board_dir: &Path, task_id: &str) -> Option<std::path::PathBuf> {
    let id: u32 = task_id.parse().ok()?;
    crate::task::find_task_path_by_id(&board_dir.join("tasks"), id).ok()
}

fn run_board_owned(board_dir: &Path, args: &[String]) -> Result<BoardOutput, BoardError> {
    let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    run_board(board_dir, &arg_refs)
}

pub(crate) fn run_board_with_program(
    program: &str,
    board_dir: &Path,
    args: &[&str],
) -> Result<BoardOutput, BoardError> {
    match super::task_cmd::repair_board_frontmatter_compat(board_dir) {
        Ok(repairs) => {
            for repair in repairs {
                let reason = repair.reason.as_deref().unwrap_or("unknown repair");
                info!(
                    task_id = ?repair.task_id,
                    status = repair.status.as_deref().unwrap_or("unknown"),
                    reason = reason,
                    path = %repair.path.display(),
                    "repaired malformed task frontmatter before board command"
                );
            }
        }
        Err(error) => warn!(
            board = %board_dir.display(),
            error = %error,
            "failed to repair malformed task frontmatter before board command"
        ),
    }
    let current_dir = board_dir
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let command = format_board_command(program, board_dir, args);
    let output = Command::new(program)
        .current_dir(current_dir)
        .args(build_board_args(board_dir, args))
        .output()
        .map_err(|source| BoardError::Exec {
            command: command.clone(),
            source,
        })?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if output.status.success() {
        return Ok(BoardOutput { stdout, stderr });
    }

    let status = output.status.code().map_or_else(
        || "terminated by signal".to_string(),
        |code| format!("exit code {code}"),
    );
    let message = format!("`{command}` failed with {status}");
    Err(classify_failure(message, stderr))
}

fn format_board_command(program: &str, board_dir: &Path, args: &[&str]) -> String {
    let mut parts = vec![program.to_string()];
    parts.extend(args.iter().map(|arg| arg.to_string()));
    parts.push("--dir".to_string());
    parts.push(board_dir.display().to_string());
    parts.join(" ")
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

    const REAL_KANBAN_MD: &str = "/opt/homebrew/bin/kanban-md";

    fn with_process_cwd<T>(cwd: &Path, action: impl FnOnce() -> T) -> T {
        let _guard = CWD_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(cwd).unwrap();
        let result = catch_unwind(AssertUnwindSafe(action));
        std::env::set_current_dir(original).unwrap();
        match result {
            Ok(value) => value,
            Err(panic) => resume_unwind(panic),
        }
    }

    fn real_kanban_available() -> bool {
        Path::new(REAL_KANBAN_MD).is_file()
    }

    fn run_real_board(board_dir: &Path, args: &[&str]) -> Result<BoardOutput, BoardError> {
        run_board_with_program(REAL_KANBAN_MD, board_dir, args)
    }

    fn create_task_real(
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

        let arg_refs = args.iter().map(String::as_str).collect::<Vec<_>>();
        let output = run_real_board(board_dir, &arg_refs)?;
        extract_task_id(&output.stdout, "Created task #").ok_or_else(|| BoardError::Permanent {
            message: format!(
                "failed to parse task ID from create output: {}",
                output.stdout.lines().next().unwrap_or_default()
            ),
            stderr: output.stderr,
        })
    }

    fn pick_task_real(
        board_dir: &Path,
        claim: &str,
        move_to: &str,
    ) -> Result<Option<String>, BoardError> {
        match run_real_board(board_dir, &["pick", "--claim", claim, "--move", move_to]) {
            Ok(output) => Ok(extract_task_id(&output.stdout, "Picked and moved task #")
                .or_else(|| extract_task_id(&output.stdout, "Picked task #"))),
            Err(BoardError::Permanent { stderr, .. }) if is_empty_pick(&stderr) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn with_live_board_cwd<T>(action: impl FnOnce() -> T) -> T {
        let live_repo = TempDir::new().unwrap();
        let live_board_dir = live_repo.path().join(".batty/team_config/board");
        fs::create_dir_all(live_board_dir.parent().unwrap()).unwrap();
        run_real_board(&live_board_dir, &["init"]).unwrap();

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
        assert!(matches!(error, BoardError::Exec { .. }));
        assert!(
            error
                .to_string()
                .contains("__batty_missing_kanban__ list --dir")
        );
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
    fn run_board_repairs_legacy_blocked_frontmatter_before_invocation() {
        let temp = TempDir::new().unwrap();
        let board_dir = temp.path().join("board");
        let tasks_dir = board_dir.join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        let task_path = tasks_dir.join("001-blocked-task.md");
        fs::write(
            &task_path,
            "---\nid: 1\ntitle: blocked task\nstatus: blocked\npriority: high\nblocked: legacy reason string\nclass: standard\n---\n\nTask body.\n",
        )
        .unwrap();

        let script_path = temp.path().join("fake-kanban.sh");
        fs::write(&script_path, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            let mut permissions = fs::metadata(&script_path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&script_path, permissions).unwrap();
        }

        run_board_with_program(script_path.to_str().unwrap(), &board_dir, &["list"]).unwrap();

        let content = fs::read_to_string(&task_path).unwrap();
        assert!(content.contains("blocked: true"));
        assert!(content.contains("block_reason: legacy reason string"));
    }

    #[test]
    fn run_board_repairs_legacy_timestamp_frontmatter_before_list_invocation() {
        let temp = TempDir::new().unwrap();
        let board_dir = temp.path().join("board");
        let tasks_dir = board_dir.join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        let task_path = tasks_dir.join("623-stale-review.md");
        fs::write(
            &task_path,
            "---\nid: 623\ntitle: stale review\nstatus: review\npriority: high\ncreated: 2026-04-10T16:31:02.743151-04:00\nupdated: 2026-04-10T19:26:40-0400\nclass: standard\n---\n\nTask body.\n",
        )
        .unwrap();

        let script_path = temp.path().join("fake-kanban.sh");
        fs::write(&script_path, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            let mut permissions = fs::metadata(&script_path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&script_path, permissions).unwrap();
        }

        run_board_with_program(script_path.to_str().unwrap(), &board_dir, &["list"]).unwrap();

        let content = fs::read_to_string(&task_path).unwrap();
        assert!(content.contains("updated: 2026-04-10T19:26:40-04:00"));
        assert!(content.ends_with("\n\nTask body.\n"));
    }

    #[test]
    fn extracts_claim_owner_from_show_output() {
        let output = "Claimed by:  eng-1-2 (since 2026-03-21 01:11)";
        assert_eq!(extract_claimed_by(output).as_deref(), Some("eng-1-2"));
        assert_eq!(extract_claimed_by("Claimed by:  --"), None);
    }

    #[test]
    fn init_create_show_round_trip_when_kanban_available() {
        if !real_kanban_available() {
            return;
        }

        with_live_board_cwd(|| {
            let temp = TempDir::new().unwrap();
            let board_dir = temp.path().join("board");

            let init_output = run_real_board(&board_dir, &["init"]).unwrap();
            assert!(board_dir.is_dir());
            assert!(init_output.stdout.contains("Initialized board"));

            let task_id = create_task_real(
                &board_dir,
                "Test task",
                "Body text",
                Some("high"),
                Some("phase-8,wave-1"),
                None,
            )
            .unwrap();
            assert_eq!(task_id, "1");

            let show = run_real_board(&board_dir, &["show", &task_id])
                .unwrap()
                .stdout;
            assert!(show.contains("Task #1: Test task"));
            assert!(show.contains("Body text"));

            let list = run_real_board(&board_dir, &["list", "--status", "backlog"])
                .unwrap()
                .stdout;
            assert!(list.contains("Test task"));

            let picked = pick_task_real(&board_dir, "eng-1-2", "in-progress").unwrap();
            assert_eq!(picked.as_deref(), Some("1"));

            run_real_board(
                &board_dir,
                &["move", &task_id, "review", "--claim", "eng-1-2"],
            )
            .unwrap();
            let review = run_real_board(&board_dir, &["show", &task_id])
                .unwrap()
                .stdout;
            assert!(review.contains("Status:      review"));

            run_real_board(
                &board_dir,
                &[
                    "edit",
                    &task_id,
                    "--block",
                    "needs manager input",
                    "--claim",
                    "eng-1-2",
                ],
            )
            .unwrap();
            let task_file = fs::read_to_string(board_dir.join("tasks/001-test-task.md")).unwrap();
            assert!(task_file.contains("block_reason: needs manager input"));
        });
    }

    #[test]
    fn pick_task_returns_none_when_board_is_empty() {
        if !real_kanban_available() {
            return;
        }

        with_live_board_cwd(|| {
            let temp = TempDir::new().unwrap();
            let board_dir = temp.path().join("board");
            run_real_board(&board_dir, &["init"]).unwrap();

            let picked = pick_task_real(&board_dir, "eng-1-2", "in-progress").unwrap();
            assert_eq!(picked, None);
        });
    }

    // --- extract_task_id edge cases ---

    #[test]
    fn extract_task_id_returns_none_when_prefix_not_found() {
        assert_eq!(extract_task_id("no match here", "Created task #"), None);
    }

    #[test]
    fn extract_task_id_returns_none_when_no_digits_after_prefix() {
        assert_eq!(extract_task_id("Created task #abc", "Created task #"), None);
    }

    #[test]
    fn extract_task_id_stops_at_non_digit() {
        assert_eq!(
            extract_task_id("Created task #42 is done", "Created task #"),
            Some("42".to_string())
        );
    }

    #[test]
    fn extract_task_id_from_empty_string() {
        assert_eq!(extract_task_id("", "Created task #"), None);
    }

    #[test]
    fn extract_task_id_with_picked_prefix() {
        assert_eq!(
            extract_task_id(
                "Picked and moved task #7 to in-progress",
                "Picked and moved task #"
            ),
            Some("7".to_string())
        );
    }

    // --- is_transient_stderr edge cases ---

    #[test]
    fn io_error_is_transient() {
        assert!(is_transient_stderr("i/o error during write"));
    }

    #[test]
    fn try_again_is_transient() {
        assert!(is_transient_stderr("operation failed, try again later"));
    }

    #[test]
    fn no_such_file_is_transient() {
        assert!(is_transient_stderr("no such file or directory: /tmp/board"));
    }

    #[test]
    fn unknown_command_is_not_transient() {
        assert!(!is_transient_stderr(
            "unknown command \"invalid\" for \"kanban-md\""
        ));
    }

    // --- is_empty_pick ---

    #[test]
    fn empty_pick_detected() {
        assert!(is_empty_pick("No unblocked, unclaimed tasks found in todo"));
    }

    #[test]
    fn non_empty_pick_error_not_detected() {
        assert!(!is_empty_pick("task #5 is already claimed"));
    }

    // --- claim_required_for_edit ---

    #[test]
    fn claim_required_detected_in_stderr() {
        assert!(claim_required_for_edit("task #5 is claimed by eng-1-2"));
    }

    #[test]
    fn claim_not_required_for_unrelated_error() {
        assert!(!claim_required_for_edit("unknown field \"foo\""));
    }

    // --- extract_claimed_by edge cases ---

    #[test]
    fn extract_claimed_by_empty_input() {
        assert_eq!(extract_claimed_by(""), None);
    }

    #[test]
    fn extract_claimed_by_no_claimed_line() {
        assert_eq!(extract_claimed_by("Status: todo\nPriority: high"), None);
    }

    #[test]
    fn extract_claimed_by_empty_claim_value() {
        assert_eq!(extract_claimed_by("Claimed by:  "), None);
    }

    #[test]
    fn extract_claimed_by_without_timestamp() {
        assert_eq!(
            extract_claimed_by("Claimed by:  manager-1"),
            Some("manager-1".to_string())
        );
    }

    #[test]
    fn extract_claimed_by_dash_dash_is_none() {
        assert_eq!(extract_claimed_by("Claimed by: --"), None);
    }

    // --- contains_word ---

    #[test]
    fn contains_word_matches_isolated_word() {
        assert!(contains_word("the lock is active", "lock"));
    }

    #[test]
    fn contains_word_rejects_substring() {
        assert!(!contains_word("unlock the door", "lock"));
    }

    #[test]
    fn contains_word_empty_text() {
        assert!(!contains_word("", "lock"));
    }

    // --- format_board_command ---

    #[test]
    fn format_board_command_includes_all_args() {
        let result =
            format_board_command("kanban-md", Path::new("/tmp/board"), &["move", "5", "done"]);
        assert_eq!(result, "kanban-md move 5 done --dir /tmp/board");
    }

    #[test]
    fn format_board_command_no_args() {
        let result = format_board_command("kanban-md", Path::new("/board"), &[]);
        assert_eq!(result, "kanban-md --dir /board");
    }

    // --- classify_failure edge cases ---

    #[test]
    fn classify_failure_lock_in_stderr() {
        let error = classify_failure("failed".to_string(), "file lock held".to_string());
        assert!(matches!(error, BoardError::Transient { .. }));
    }

    #[test]
    fn classify_failure_empty_stderr_is_permanent() {
        let error = classify_failure("failed".to_string(), "".to_string());
        assert!(matches!(error, BoardError::Permanent { .. }));
    }

    // --- scheduling field preservation ---

    #[test]
    fn parse_scheduling_fields_extracts_all_three() {
        let content = "---\nid: 42\ntitle: recurring task\nstatus: todo\nscheduled_for: 2026-04-01T09:00:00Z\ncron_schedule: 0 9 * * 1\ncron_last_run: 2026-03-21T09:00:00Z\n---\n\nBody.\n";
        let fields = parse_scheduling_fields_from_frontmatter(content);
        assert_eq!(fields.len(), 3);
        assert_eq!(
            fields[0],
            (
                "scheduled_for".to_string(),
                "2026-04-01T09:00:00Z".to_string()
            )
        );
        assert_eq!(
            fields[1],
            ("cron_schedule".to_string(), "0 9 * * 1".to_string())
        );
        assert_eq!(
            fields[2],
            (
                "cron_last_run".to_string(),
                "2026-03-21T09:00:00Z".to_string()
            )
        );
    }

    #[test]
    fn parse_scheduling_fields_extracts_quoted_values() {
        let content = "---\nid: 1\ntitle: test\nstatus: todo\nscheduled_for: \"2026-06-15T12:00:00Z\"\n---\n\nBody.\n";
        let fields = parse_scheduling_fields_from_frontmatter(content);
        assert_eq!(fields.len(), 1);
        assert_eq!(
            fields[0],
            (
                "scheduled_for".to_string(),
                "2026-06-15T12:00:00Z".to_string()
            )
        );
    }

    #[test]
    fn parse_scheduling_fields_returns_empty_when_none_present() {
        let content = "---\nid: 1\ntitle: test\nstatus: todo\n---\n\nBody.\n";
        let fields = parse_scheduling_fields_from_frontmatter(content);
        assert!(fields.is_empty());
    }

    #[test]
    fn parse_scheduling_fields_handles_missing_frontmatter() {
        let fields = parse_scheduling_fields_from_frontmatter("no frontmatter here");
        assert!(fields.is_empty());
    }

    #[test]
    fn parse_scheduling_fields_handles_unclosed_frontmatter() {
        let fields = parse_scheduling_fields_from_frontmatter("---\nid: 1\ntitle: test\n");
        assert!(fields.is_empty());
    }

    #[test]
    fn parse_scheduling_fields_ignores_empty_values() {
        let content = "---\nid: 1\ntitle: test\nstatus: todo\nscheduled_for:\n---\n\nBody.\n";
        let fields = parse_scheduling_fields_from_frontmatter(content);
        assert!(fields.is_empty());
    }

    #[test]
    fn find_task_file_locates_by_id() {
        let temp = TempDir::new().unwrap();
        let tasks_dir = temp.path().join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        fs::write(
            tasks_dir.join("042-recurring-task.md"),
            "---\nid: 42\n---\n",
        )
        .unwrap();
        fs::write(tasks_dir.join("001-other.md"), "---\nid: 1\n---\n").unwrap();

        let found = find_task_file(temp.path(), "42");
        assert!(found.is_some());
        assert!(found.unwrap().ends_with("042-recurring-task.md"));
    }

    #[test]
    fn find_task_file_returns_none_for_missing_id() {
        let temp = TempDir::new().unwrap();
        let tasks_dir = temp.path().join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        fs::write(tasks_dir.join("001-test.md"), "---\nid: 1\n---\n").unwrap();

        assert!(find_task_file(temp.path(), "99").is_none());
    }

    #[test]
    fn find_task_file_returns_none_for_missing_dir() {
        let temp = TempDir::new().unwrap();
        assert!(find_task_file(temp.path(), "1").is_none());
    }

    #[test]
    fn extract_and_restore_scheduling_fields_round_trip() {
        let temp = TempDir::new().unwrap();
        let tasks_dir = temp.path().join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();

        let original = "---\nid: 5\ntitle: recurring\nstatus: todo\nscheduled_for: 2026-04-01T09:00:00Z\ncron_schedule: 0 9 * * 1\ncron_last_run: 2026-03-21T09:00:00Z\n---\n\nTask body.\n";
        fs::write(tasks_dir.join("005-recurring.md"), original).unwrap();

        // Extract fields
        let fields = extract_scheduling_fields(temp.path(), "5");
        assert_eq!(fields.len(), 3);

        // Simulate kanban-md stripping the fields
        let stripped = "---\nid: 5\ntitle: recurring\nstatus: in-progress\nclaimed_by: eng-1-2\n---\n\nTask body.\n";
        fs::write(tasks_dir.join("005-recurring.md"), stripped).unwrap();

        // Restore fields
        restore_scheduling_fields(temp.path(), "5", &fields);

        // Verify the fields are back
        let result = fs::read_to_string(tasks_dir.join("005-recurring.md")).unwrap();
        assert!(result.contains("scheduled_for: 2026-04-01T09:00:00Z"));
        assert!(result.contains("cron_schedule: 0 9 * * 1"));
        assert!(result.contains("cron_last_run: 2026-03-21T09:00:00Z"));
        // And the new status is preserved
        assert!(result.contains("status: in-progress"));
        assert!(result.contains("claimed_by: eng-1-2"));
    }

    #[test]
    fn restore_does_nothing_when_no_fields() {
        let temp = TempDir::new().unwrap();
        let tasks_dir = temp.path().join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();

        let content = "---\nid: 1\ntitle: test\nstatus: done\n---\n\nBody.\n";
        fs::write(tasks_dir.join("001-test.md"), content).unwrap();

        restore_scheduling_fields(temp.path(), "1", &[]);

        let result = fs::read_to_string(tasks_dir.join("001-test.md")).unwrap();
        assert_eq!(result, content);
    }

    #[test]
    fn restore_handles_missing_task_file() {
        let temp = TempDir::new().unwrap();
        let tasks_dir = temp.path().join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();

        // Should not panic — gracefully no-op
        restore_scheduling_fields(
            temp.path(),
            "99",
            &[("cron_schedule".to_string(), "0 9 * * *".to_string())],
        );
    }

    #[test]
    fn snapshot_scheduling_fields_indexes_by_task_id() {
        let temp = TempDir::new().unwrap();
        let tasks_dir = temp.path().join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();

        fs::write(
            tasks_dir.join("010-scheduled.md"),
            "---\nid: 10\ntitle: scheduled\nstatus: todo\nscheduled_for: 2026-05-01T00:00:00Z\n---\n\nBody.\n",
        ).unwrap();
        fs::write(
            tasks_dir.join("011-plain.md"),
            "---\nid: 11\ntitle: plain\nstatus: todo\n---\n\nBody.\n",
        )
        .unwrap();
        fs::write(
            tasks_dir.join("012-cron.md"),
            "---\nid: 12\ntitle: cron\nstatus: todo\ncron_schedule: 30 8 * * *\n---\n\nBody.\n",
        )
        .unwrap();

        let snapshot = snapshot_scheduling_fields(temp.path());
        assert_eq!(snapshot.len(), 2);
        assert!(snapshot.contains_key("10"));
        assert!(snapshot.contains_key("12"));
        assert!(!snapshot.contains_key("11"));
    }

    #[test]
    fn move_task_preserves_scheduling_fields_end_to_end() {
        if !real_kanban_available() {
            return;
        }

        with_live_board_cwd(|| {
            let temp = TempDir::new().unwrap();
            let board_dir = temp.path().join("board");
            run_real_board(&board_dir, &["init"]).unwrap();

            // Create a task
            let task_id = create_task_real(
                &board_dir,
                "Recurring task",
                "This runs on a schedule",
                Some("medium"),
                None,
                None,
            )
            .unwrap();

            // Manually add scheduling fields to the task file
            let task_file = board_dir.join("tasks/001-recurring-task.md");
            let content = fs::read_to_string(&task_file).unwrap();
            let patched = content.replace(
                "\n---\n",
                "\nscheduled_for: 2026-06-01T00:00:00Z\ncron_schedule: 0 9 * * 1\ncron_last_run: 2026-05-25T09:00:00Z\n---\n",
            );
            fs::write(&task_file, &patched).unwrap();

            // Verify fields are present before move
            let before = fs::read_to_string(&task_file).unwrap();
            assert!(before.contains("cron_schedule: 0 9 * * 1"));

            // Move the task — this would normally strip the fields
            run_real_board(
                &board_dir,
                &["move", &task_id, "in-progress", "--claim", "eng-1-2"],
            )
            .unwrap();

            // Without our fix, these fields would be gone. But we're testing the raw
            // kanban-md here. Let's verify they ARE stripped by raw kanban-md:
            let after_raw = fs::read_to_string(&task_file).unwrap();
            // (This documents the bug — kanban-md strips them)
            let fields_stripped = !after_raw.contains("cron_schedule");

            if fields_stripped {
                // Now test our wrapper: reset, re-add fields, use our move_task
                run_real_board(
                    &board_dir,
                    &["move", &task_id, "todo", "--claim", "eng-1-2"],
                )
                .unwrap();
                let content2 = fs::read_to_string(&task_file).unwrap();
                let patched2 = content2.replace(
                    "\n---\n",
                    "\nscheduled_for: 2026-06-01T00:00:00Z\ncron_schedule: 0 9 * * 1\ncron_last_run: 2026-05-25T09:00:00Z\n---\n",
                );
                fs::write(&task_file, &patched2).unwrap();

                // Use a real wrapper that calls the real binary
                // We can't use move_task directly because it uses "kanban-md" not the real path,
                // but we can test the extract/restore logic directly
                let saved = extract_scheduling_fields(&board_dir, &task_id);
                assert_eq!(saved.len(), 3);

                run_real_board(
                    &board_dir,
                    &["move", &task_id, "in-progress", "--claim", "eng-1-2"],
                )
                .unwrap();

                // Fields are gone after raw move
                let after = fs::read_to_string(&task_file).unwrap();
                assert!(!after.contains("cron_schedule"));

                // Restore them
                restore_scheduling_fields(&board_dir, &task_id, &saved);

                // Now they're back
                let restored = fs::read_to_string(&task_file).unwrap();
                assert!(restored.contains("scheduled_for: 2026-06-01T00:00:00Z"));
                assert!(restored.contains("cron_schedule: 0 9 * * 1"));
                assert!(restored.contains("cron_last_run: 2026-05-25T09:00:00Z"));
                assert!(restored.contains("status: in-progress"));
            }
        });
    }
}
