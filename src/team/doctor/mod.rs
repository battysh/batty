use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::config::TeamConfig;
use super::hierarchy;
use super::standup::MemberState;

mod checks;
pub(super) mod cleanup;
mod util;

use checks::{
    build_board_dependency_graph, build_board_git_checks, build_performance_checks,
    build_resume_eligibility, build_worktree_statuses,
};
use cleanup::{
    apply_cleanup_plan, detect_cleanup_plan, render_cleanup_plan, render_cleanup_summary,
};
use util::{
    file_size, launch_state_path, load_daemon_state, load_launch_state, load_team_config,
    prompt_yes_no, short_prompt_hash,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct LaunchIdentityRecord {
    agent: String,
    prompt: String,
    session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DoctorDaemonState {
    clean_shutdown: bool,
    saved_at: u64,
    states: HashMap<String, MemberState>,
    active_tasks: HashMap<String, u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResumeEligibility {
    member: String,
    eligible: bool,
    reason: String,
    stored_prompt_hash: Option<String>,
    current_prompt_hash: Option<String>,
    session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorktreeStatus {
    member: String,
    path: PathBuf,
    branch: Option<String>,
    dirty: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LogSize {
    name: &'static str,
    bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckLevel {
    Pass,
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CheckLine {
    level: CheckLevel,
    message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OrphanStatus {
    branches: Vec<String>,
    worktrees: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CleanupSummary {
    branches_removed: usize,
    worktrees_removed: usize,
    stale_state_removed: usize,
    test_sessions_removed: usize,
    actions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CleanupPlan {
    orphan_status: OrphanStatus,
    stale_state: Vec<PathBuf>,
    orphan_test_sessions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveTaskTargets {
    branches: HashSet<String>,
    worktrees: HashSet<PathBuf>,
}

impl CleanupPlan {
    fn is_empty(&self) -> bool {
        self.orphan_status.branches.is_empty()
            && self.orphan_status.worktrees.is_empty()
            && self.stale_state.is_empty()
            && self.orphan_test_sessions.is_empty()
    }
}

pub fn run(project_root: &Path, fix: bool, yes: bool) -> Result<String> {
    let mut report = build_report(project_root)?;
    if !fix {
        return Ok(report);
    }

    let cleanup_plan = detect_cleanup_plan(project_root)?;
    report.push('\n');
    report.push_str(&render_cleanup_plan(project_root, &cleanup_plan));

    if cleanup_plan.is_empty() {
        return Ok(report);
    }

    if yes {
        let summary = apply_cleanup_plan(project_root, &cleanup_plan)?;
        report.push('\n');
        report.push_str(&render_cleanup_summary(&summary));
        return Ok(report);
    }

    print!("{report}");
    io::stdout().flush()?;
    if !prompt_yes_no("Apply the safe cleanup actions listed above? [y/N] ", false)? {
        return Ok("\nCleanup aborted.\n".to_string());
    }

    let summary = apply_cleanup_plan(project_root, &cleanup_plan)?;
    Ok(format!("\n{}", render_cleanup_summary(&summary)))
}

pub fn build_report(project_root: &Path) -> Result<String> {
    let launch_state = load_launch_state(&launch_state_path(project_root))?;
    let daemon_state = load_daemon_state(&super::daemon_state_path(project_root))?;
    let team_config = load_team_config(project_root)?;
    let members = match &team_config {
        Some(config) => hierarchy::resolve_hierarchy(config)?,
        None => Vec::new(),
    };
    let resume =
        build_resume_eligibility(project_root, team_config.as_ref(), &members, &launch_state);
    let worktrees = build_worktree_statuses(project_root, &members);
    let board_git_checks = build_board_git_checks(project_root);
    let board_dependency_graph = build_board_dependency_graph(project_root);
    let performance_checks = build_performance_checks(project_root);
    let orphan_test_sessions = crate::tmux::list_sessions_with_prefix("batty-test-");
    let log_sizes = vec![
        LogSize {
            name: "daemon.log",
            bytes: file_size(&project_root.join(".batty").join("daemon.log")),
        },
        LogSize {
            name: "orchestrator.log",
            bytes: file_size(&project_root.join(".batty").join("orchestrator.log")),
        },
        LogSize {
            name: "events.jsonl",
            bytes: file_size(
                &project_root
                    .join(".batty")
                    .join("team_config")
                    .join("events.jsonl"),
            ),
        },
    ];

    Ok(render_report(DoctorReportData {
        project_root,
        launch_state: launch_state.as_ref(),
        daemon_state: daemon_state.as_ref(),
        resume: &resume,
        worktrees: &worktrees,
        board_git_checks: &board_git_checks,
        board_dependency_graph: &board_dependency_graph,
        performance_checks: &performance_checks,
        orphan_test_sessions: &orphan_test_sessions,
        log_sizes: &log_sizes,
    }))
}

struct DoctorReportData<'a> {
    project_root: &'a Path,
    launch_state: Option<&'a HashMap<String, LaunchIdentityRecord>>,
    daemon_state: Option<&'a DoctorDaemonState>,
    resume: &'a [ResumeEligibility],
    worktrees: &'a [WorktreeStatus],
    board_git_checks: &'a [CheckLine],
    board_dependency_graph: &'a [String],
    performance_checks: &'a [CheckLine],
    orphan_test_sessions: &'a [String],
    log_sizes: &'a [LogSize],
}

fn render_report(report: DoctorReportData<'_>) -> String {
    let DoctorReportData {
        project_root,
        launch_state,
        daemon_state,
        resume,
        worktrees,
        board_git_checks,
        board_dependency_graph,
        performance_checks,
        orphan_test_sessions,
        log_sizes,
    } = report;
    let mut out = String::new();
    out.push_str(&format!("Batty doctor for {}\n\n", project_root.display()));

    out.push_str("== Launch State ==\n");
    match launch_state {
        Some(state) if !state.is_empty() => {
            let mut names: Vec<_> = state.keys().cloned().collect();
            names.sort();
            for name in names {
                let identity = &state[&name];
                out.push_str(&format!(
                    "{}: agent={}, prompt_hash={}, session_id={}\n",
                    name,
                    identity.agent,
                    short_prompt_hash(&identity.prompt),
                    identity.session_id.as_deref().unwrap_or("-"),
                ));
            }
        }
        _ => out.push_str("(missing)\n"),
    }
    out.push('\n');

    out.push_str("== Daemon State ==\n");
    match daemon_state {
        Some(state) => {
            out.push_str(&format!("clean_shutdown: {}\n", state.clean_shutdown));
            if state.states.is_empty() {
                out.push_str("member_states: (none)\n");
            } else {
                let mut names: Vec<_> = state.states.keys().cloned().collect();
                names.sort();
                out.push_str("member_states:\n");
                for name in names {
                    out.push_str(&format!("  {}: {:?}\n", name, state.states[&name]));
                }
            }
            if state.active_tasks.is_empty() {
                out.push_str("active_tasks: (none)\n");
            } else {
                let mut names: Vec<_> = state.active_tasks.keys().cloned().collect();
                names.sort();
                out.push_str("active_tasks:\n");
                for name in names {
                    out.push_str(&format!("  {}: #{}\n", name, state.active_tasks[&name]));
                }
            }
        }
        None => out.push_str("(missing)\n"),
    }
    out.push('\n');

    out.push_str("== Resume Eligibility ==\n");
    if resume.is_empty() {
        out.push_str("(no team config or members)\n");
    } else {
        for item in resume {
            out.push_str(&format!(
                "{}: eligible={} reason={} stored_hash={} current_hash={} session_id={}\n",
                item.member,
                item.eligible,
                item.reason,
                item.stored_prompt_hash.as_deref().unwrap_or("-"),
                item.current_prompt_hash.as_deref().unwrap_or("-"),
                item.session_id.as_deref().unwrap_or("-"),
            ));
        }
    }
    out.push('\n');

    out.push_str("== Worktree Status ==\n");
    if worktrees.is_empty() {
        out.push_str("(no engineers)\n");
    } else {
        for status in worktrees {
            let dirty = match status.dirty {
                Some(true) => "dirty",
                Some(false) => "clean",
                None => "missing",
            };
            out.push_str(&format!(
                "{}: path={} branch={} status={}\n",
                status.member,
                status.path.display(),
                status.branch.as_deref().unwrap_or("-"),
                dirty,
            ));
        }
    }
    out.push('\n');

    out.push_str("== Board-Git Consistency ==\n");
    if board_git_checks.is_empty() {
        out.push_str("PASS: no active board tasks or git consistency issues detected\n");
    } else {
        for line in board_git_checks {
            out.push_str(&format!(
                "{}: {}\n",
                match line.level {
                    CheckLevel::Pass => "PASS",
                    CheckLevel::Warn => "WARN",
                    CheckLevel::Fail => "FAIL",
                },
                line.message
            ));
        }
    }
    out.push('\n');

    out.push_str("== Board Dependency Graph ==\n");
    for line in board_dependency_graph {
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');

    out.push_str("== Performance Regression ==\n");
    for line in performance_checks {
        out.push_str(&format!(
            "{}: {}\n",
            match line.level {
                CheckLevel::Pass => "PASS",
                CheckLevel::Warn => "WARN",
                CheckLevel::Fail => "FAIL",
            },
            line.message
        ));
    }
    out.push('\n');

    out.push_str("== Orphaned Test Sessions ==\n");
    if orphan_test_sessions.is_empty() {
        out.push_str("PASS: no orphaned test sessions found\n");
    } else {
        out.push_str(&format!(
            "WARN: found {} orphaned test sessions (run with --fix to clean)\n",
            orphan_test_sessions.len()
        ));
        for session in orphan_test_sessions {
            out.push_str(&format!("  {session}\n"));
        }
    }
    out.push('\n');

    out.push_str("== Log Sizes ==\n");
    for log in log_sizes {
        match log.bytes {
            Some(bytes) => out.push_str(&format!("{}: {} bytes\n", log.name, bytes)),
            None => out.push_str(&format!("{}: missing\n", log.name)),
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use super::util::{launch_state_path, strip_nudge_section};
    use super::*;

    fn write_team_config(root: &Path) {
        write_named_team_config(root, "test");
    }

    fn write_named_team_config(root: &Path, name: &str) {
        let team_dir = root.join(".batty").join("team_config");
        fs::create_dir_all(&team_dir).unwrap();
        fs::write(
            team_dir.join("team.yaml"),
            format!(
                r#"
name: {name}
roles:
  - name: architect
    role_type: architect
    agent: claude
  - name: manager
    role_type: manager
    agent: codex
  - name: engineer
    role_type: engineer
    agent: codex
    use_worktrees: true
"#
            ),
        )
        .unwrap();
        fs::write(
            team_dir.join("architect.md"),
            "Architect prompt\n## Nudge\nnudge text\n## Next\nkeep this",
        )
        .unwrap();
        fs::write(team_dir.join("manager.md"), "Manager prompt").unwrap();
        fs::write(team_dir.join("engineer.md"), "Engineer prompt").unwrap();
    }

    fn init_git_repo(root: &Path) {
        git_ok(root, &["init", "-b", "main"]);
        git_ok(root, &["config", "user.email", "batty-test@example.com"]);
        git_ok(root, &["config", "user.name", "Batty Test"]);
        fs::write(root.join("README.md"), "initial\n").unwrap();
        git_ok(root, &["add", "README.md"]);
        git_ok(root, &["commit", "-m", "initial"]);
    }

    fn git_ok(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap_or_else(|error| panic!("git {:?} failed to run: {error}", args));
        assert!(
            output.status.success(),
            "git {:?} failed: stdout={} stderr={}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn write_board_task(
        root: &Path,
        id: u32,
        status: &str,
        branch: Option<&str>,
        worktree_path: Option<&str>,
    ) {
        let tasks_dir = root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        let mut content = format!(
            "---\nid: {id}\ntitle: Task {id}\nstatus: {status}\npriority: medium\nclass: standard\n"
        );
        if let Some(branch) = branch {
            content.push_str(&format!("branch: {branch}\n"));
        }
        if let Some(worktree_path) = worktree_path {
            content.push_str(&format!("worktree_path: {worktree_path}\n"));
        }
        content.push_str("---\n\nTask body.\n");
        fs::write(tasks_dir.join(format!("{id:03}-task-{id}.md")), content).unwrap();
    }

    fn write_dependency_task(root: &Path, id: u32, title: &str, status: &str, depends_on: &[u32]) {
        let tasks_dir = root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        let mut content = format!(
            "---\nid: {id}\ntitle: {title}\nstatus: {status}\npriority: medium\nclass: standard\n"
        );
        if !depends_on.is_empty() {
            content.push_str("depends_on:\n");
            for dep_id in depends_on {
                content.push_str(&format!("  - {dep_id}\n"));
            }
        }
        content.push_str("---\n\nTask body.\n");
        fs::write(tasks_dir.join(format!("{id:03}-task-{id}.md")), content).unwrap();
    }

    #[test]
    fn test_doctor_parses_launch_state() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("launch-state.json");
        fs::write(
            &path,
            r#"{"manager":{"agent":"codex-cli","prompt":"Manager prompt","session_id":null}}"#,
        )
        .unwrap();

        let parsed = util::load_launch_state(&path).unwrap().unwrap();
        assert_eq!(parsed["manager"].agent, "codex-cli");
        assert_eq!(parsed["manager"].prompt, "Manager prompt");
        assert_eq!(parsed["manager"].session_id, None);
    }

    #[test]
    fn test_doctor_parses_daemon_state() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon-state.json");
        fs::write(
            &path,
            r#"{"clean_shutdown":true,"saved_at":10,"states":{"manager":"idle"},"active_tasks":{"eng-1":42}}"#,
        )
        .unwrap();

        let parsed = util::load_daemon_state(&path).unwrap().unwrap();
        assert!(parsed.clean_shutdown);
        assert_eq!(parsed.states["manager"], MemberState::Idle);
        assert_eq!(parsed.active_tasks["eng-1"], 42);
    }

    #[test]
    fn test_doctor_formats_output() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        write_team_config(tmp.path());
        fs::create_dir_all(tmp.path().join(".batty").join("worktrees").join("engineer")).unwrap();
        let launch_state = HashMap::from([
            (
                "architect".to_string(),
                LaunchIdentityRecord {
                    agent: "claude-code".to_string(),
                    prompt: strip_nudge_section(
                        "Architect prompt\n## Nudge\nnudge text\n## Next\nkeep this",
                    ),
                    session_id: Some("missing".to_string()),
                },
            ),
            (
                "manager".to_string(),
                LaunchIdentityRecord {
                    agent: "codex-cli".to_string(),
                    prompt: "Manager prompt".to_string(),
                    session_id: None,
                },
            ),
            (
                "engineer".to_string(),
                LaunchIdentityRecord {
                    agent: "codex-cli".to_string(),
                    prompt: "Engineer prompt".to_string(),
                    session_id: None,
                },
            ),
        ]);
        fs::write(
            launch_state_path(tmp.path()),
            serde_json::to_string(&launch_state).unwrap(),
        )
        .unwrap();
        fs::write(
            crate::team::daemon_state_path(tmp.path()),
            r#"{"clean_shutdown":false,"saved_at":10,"states":{"architect":"working","manager":"idle"},"active_tasks":{"engineer":58}}"#,
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join(".batty")).unwrap();
        fs::write(tmp.path().join(".batty").join("daemon.log"), "daemon").unwrap();
        fs::write(
            tmp.path().join(".batty").join("orchestrator.log"),
            "orchestrator",
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        fs::write(
            tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
            "events",
        )
        .unwrap();

        let report = build_report(tmp.path()).unwrap();

        assert!(report.contains("== Launch State =="));
        assert!(report.contains("== Daemon State =="));
        assert!(report.contains("== Resume Eligibility =="));
        assert!(report.contains("== Worktree Status =="));
        assert!(report.contains("== Board-Git Consistency =="));
        assert!(report.contains("== Board Dependency Graph =="));
        assert!(report.contains("== Performance Regression =="));
        assert!(report.contains("== Log Sizes =="));
        assert!(report.contains("manager: agent=codex-cli"));
        assert!(report.contains("clean_shutdown: false"));
        assert!(report.contains("path="));
        assert!(report.contains("status=missing"));
        assert!(report.contains("daemon.log: 6 bytes"));
        assert!(report.contains("events.jsonl: 6 bytes"));
    }

    #[test]
    fn test_doctor_handles_missing_files() {
        let tmp = tempfile::tempdir().unwrap();

        let report = build_report(tmp.path()).unwrap();

        assert!(report.contains("== Launch State =="));
        assert!(report.contains("(missing)"));
        assert!(report.contains("== Daemon State =="));
        assert!(report.contains("== Resume Eligibility =="));
        assert!(report.contains("== Board-Git Consistency =="));
        assert!(report.contains("== Board Dependency Graph =="));
        assert!(report.contains("PASS: board tasks directory missing; nothing to visualize"));
        assert!(report.contains("== Performance Regression =="));
        assert!(report.contains("(no team config or members)"));
        assert!(report.contains("daemon.log: missing"));
        assert!(report.contains("orchestrator.log: missing"));
        assert!(report.contains("events.jsonl: missing"));
    }

    #[test]
    fn doctor_board_clean_state_reports_passes() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        write_board_task(
            tmp.path(),
            69,
            "in-progress",
            Some("eng-1-3/69"),
            Some(".batty/worktrees/eng-1-3"),
        );

        let worktree_path = tmp.path().join(".batty").join("worktrees").join("eng-1-3");
        git_ok(
            tmp.path(),
            &[
                "worktree",
                "add",
                "-b",
                "eng-1-3/69",
                worktree_path.to_string_lossy().as_ref(),
                "main",
            ],
        );
        fs::write(worktree_path.join("feature.txt"), "feature\n").unwrap();
        git_ok(&worktree_path, &["add", "feature.txt"]);
        git_ok(&worktree_path, &["commit", "-m", "task work"]);

        let report = build_report(tmp.path()).unwrap();

        assert!(report.contains("== Board-Git Consistency =="));
        assert!(report.contains("== Board Dependency Graph =="));
        assert!(report.contains("PASS: no task dependencies declared"));
        assert!(report.contains("PASS: all 1 active task branches exist and are ahead of main"));
        assert!(
            report.contains("PASS: all 1 active task worktrees exist and match board metadata")
        );
        assert!(report.contains("PASS: no orphan task branches found"));
        assert!(report.contains("PASS: no orphan task worktrees found"));
    }

    #[test]
    fn doctor_performance_regression_warns_on_latest_slow_merge() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        fs::create_dir_all(tmp.path().join(".batty")).unwrap();
        fs::write(
            tmp.path().join(".batty").join("test_timing.jsonl"),
            [
                r#"{"task_id":1,"engineer":"eng-1","branch":"eng-1/task-1","measured_at":1,"duration_ms":1000,"rolling_average_ms":null,"regression_pct":null,"regression_detected":false}"#,
                r#"{"task_id":2,"engineer":"eng-1","branch":"eng-1/task-2","measured_at":2,"duration_ms":1000,"rolling_average_ms":null,"regression_pct":null,"regression_detected":false}"#,
                r#"{"task_id":3,"engineer":"eng-1","branch":"eng-1/task-3","measured_at":3,"duration_ms":1000,"rolling_average_ms":null,"regression_pct":null,"regression_detected":false}"#,
                r#"{"task_id":4,"engineer":"eng-1","branch":"eng-1/task-4","measured_at":4,"duration_ms":1000,"rolling_average_ms":null,"regression_pct":null,"regression_detected":false}"#,
                r#"{"task_id":5,"engineer":"eng-1","branch":"eng-1/task-5","measured_at":5,"duration_ms":1000,"rolling_average_ms":null,"regression_pct":null,"regression_detected":false}"#,
                r#"{"task_id":6,"engineer":"eng-1","branch":"eng-1/task-6","measured_at":6,"duration_ms":1300,"rolling_average_ms":1000,"regression_pct":30,"regression_detected":true}"#,
            ]
            .join("\n"),
        )
        .unwrap();

        let report = build_report(tmp.path()).unwrap();

        assert!(report.contains("== Performance Regression =="));
        assert!(report.contains("WARN: latest merge test runtime regressed on task #6"));
        assert!(report.contains("1300 ms"));
        assert!(report.contains("1000 ms"));
        assert!(report.contains("30% slower"));
    }

    #[test]
    fn doctor_performance_regression_reports_clean_baseline() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        fs::create_dir_all(tmp.path().join(".batty")).unwrap();
        fs::write(
            tmp.path().join(".batty").join("test_timing.jsonl"),
            [
                r#"{"task_id":1,"engineer":"eng-1","branch":"eng-1/task-1","measured_at":1,"duration_ms":1000,"rolling_average_ms":null,"regression_pct":null,"regression_detected":false}"#,
                r#"{"task_id":2,"engineer":"eng-1","branch":"eng-1/task-2","measured_at":2,"duration_ms":1000,"rolling_average_ms":null,"regression_pct":null,"regression_detected":false}"#,
                r#"{"task_id":3,"engineer":"eng-1","branch":"eng-1/task-3","measured_at":3,"duration_ms":1000,"rolling_average_ms":null,"regression_pct":null,"regression_detected":false}"#,
                r#"{"task_id":4,"engineer":"eng-1","branch":"eng-1/task-4","measured_at":4,"duration_ms":1000,"rolling_average_ms":null,"regression_pct":null,"regression_detected":false}"#,
                r#"{"task_id":5,"engineer":"eng-1","branch":"eng-1/task-5","measured_at":5,"duration_ms":1000,"rolling_average_ms":null,"regression_pct":null,"regression_detected":false}"#,
                r#"{"task_id":6,"engineer":"eng-1","branch":"eng-1/task-6","measured_at":6,"duration_ms":1100,"rolling_average_ms":1000,"regression_pct":10,"regression_detected":false}"#,
            ]
            .join("\n"),
        )
        .unwrap();

        let report = build_report(tmp.path()).unwrap();

        assert!(report.contains(
            "PASS: latest merge test runtime is 1100 ms vs rolling 5-merge average 1000 ms"
        ));
    }

    #[test]
    fn doctor_board_warns_on_missing_branch_and_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        write_board_task(
            tmp.path(),
            69,
            "review",
            Some("eng-1-3/task-69"),
            Some(".batty/worktrees/eng-1-3"),
        );

        let report = build_report(tmp.path()).unwrap();

        assert!(report.contains("WARN: task #69 declares missing branch 'eng-1-3/task-69'"));
        assert!(report.contains("WARN: task #69 declares missing worktree"));
    }

    #[test]
    fn doctor_board_warns_on_dirty_and_orphaned_git_state() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        write_board_task(
            tmp.path(),
            69,
            "in-progress",
            Some("eng-1-3/task-69"),
            Some(".batty/worktrees/eng-1-3"),
        );

        let active_worktree = tmp.path().join(".batty").join("worktrees").join("eng-1-3");
        git_ok(
            tmp.path(),
            &[
                "worktree",
                "add",
                "-b",
                "eng-1-3/task-69",
                active_worktree.to_string_lossy().as_ref(),
                "main",
            ],
        );
        fs::write(active_worktree.join("feature.txt"), "feature\n").unwrap();
        git_ok(&active_worktree, &["add", "feature.txt"]);
        git_ok(&active_worktree, &["commit", "-m", "task work"]);
        fs::write(active_worktree.join("dirty.txt"), "dirty\n").unwrap();

        git_ok(tmp.path(), &["checkout", "-b", "eng-9/task-99"]);
        fs::write(tmp.path().join("orphan.txt"), "orphan\n").unwrap();
        git_ok(tmp.path(), &["add", "orphan.txt"]);
        git_ok(tmp.path(), &["commit", "-m", "orphan branch"]);
        git_ok(tmp.path(), &["checkout", "main"]);

        let orphan_worktree = tmp.path().join(".batty").join("worktrees").join("orphan");
        git_ok(
            tmp.path(),
            &[
                "worktree",
                "add",
                "-b",
                "eng-9/task-100",
                orphan_worktree.to_string_lossy().as_ref(),
                "main",
            ],
        );

        let report = build_report(tmp.path()).unwrap();

        assert!(report.contains("WARN: task #69 worktree"));
        assert!(report.contains("has uncommitted changes"));
        assert!(
            report.contains("WARN: orphan task branch 'eng-9/task-99' has no active board task")
        );
        assert!(report.contains("WARN: orphan worktree"));
        assert!(report.contains("eng-9/task-100"));
    }

    #[test]
    fn doctor_dependency_graph_marks_satisfied_and_blocking_deps() {
        let tmp = tempfile::tempdir().unwrap();
        write_dependency_task(tmp.path(), 10, "Completed dependency", "done", &[]);
        write_dependency_task(tmp.path(), 11, "Active dependency", "in-progress", &[]);
        write_dependency_task(tmp.path(), 12, "Consumer task", "todo", &[10, 11, 99]);

        let report = build_report(tmp.path()).unwrap();

        assert!(report.contains("== Board Dependency Graph =="));
        assert!(report.contains("#12 [todo] Consumer task"));
        assert!(report.contains("  -> #10 [done] Completed dependency (satisfied)"));
        assert!(report.contains("  -> #11 [in-progress] Active dependency (blocking)"));
        assert!(report.contains("  -> #99 [missing] (blocking)"));
    }

    #[test]
    fn doctor_dependency_graph_detects_cycles() {
        let tmp = tempfile::tempdir().unwrap();
        write_dependency_task(tmp.path(), 20, "Task 20", "todo", &[21]);
        write_dependency_task(tmp.path(), 21, "Task 21", "todo", &[20]);

        let report = build_report(tmp.path()).unwrap();

        assert!(report.contains("Circular dependencies:"));
        assert!(report.contains("  WARN: #20 -> #21 -> #20"));
    }
}
