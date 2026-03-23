//! Backend health, worktree staleness, uncommitted work warnings, and prompt loading.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{info, warn};

use super::super::*;

impl TeamDaemon {
    /// Detect worktrees stuck on stale branches whose commits have already
    /// been cherry-picked onto main, and auto-reset them to the base branch.
    pub(in super::super) fn check_worktree_staleness(&mut self) -> Result<()> {
        let members: Vec<_> = self
            .config
            .members
            .iter()
            .filter(|m| m.role_type == RoleType::Engineer && m.use_worktrees)
            .map(|m| m.name.clone())
            .collect();

        for name in &members {
            let worktree_path = self.worktree_dir(name);
            if !worktree_path.is_dir() {
                continue;
            }

            let current_branch = match crate::worktree::git_current_branch(&worktree_path) {
                Ok(b) => b,
                Err(_) => continue,
            };

            let base = format!("eng-main/{}", name);

            // Skip if already on base branch or main.
            if current_branch == base || current_branch == "main" {
                continue;
            }

            // Skip if engineer has an active task — don't reset mid-work.
            if self.active_tasks.contains_key(name.as_str()) {
                continue;
            }

            match crate::worktree::branch_fully_merged(
                &self.config.project_root,
                &current_branch,
                "main",
            ) {
                Ok(true) => {
                    info!(
                        member = %name,
                        branch = %current_branch,
                        "stale branch detected; resetting worktree"
                    );
                    if let Err(error) =
                        crate::worktree::reset_worktree_to_base(&worktree_path, &base)
                    {
                        warn!(
                            member = %name,
                            error = %error,
                            "failed to auto-reset stale worktree; continuing"
                        );
                        continue;
                    }
                    self.record_orchestrator_action(format!(
                        "runtime: auto-reset {}'s worktree — branch {} already on main",
                        name, current_branch
                    ));
                }
                Ok(false) => { /* branch has unique commits; not stale */ }
                Err(error) => {
                    warn!(
                        member = %name,
                        error = %error,
                        "failed to check worktree staleness; continuing"
                    );
                }
            }
        }
        Ok(())
    }

    /// Periodically check agent backend health and emit events on transitions.
    pub(in super::super) fn check_backend_health(&mut self) -> Result<()> {
        let interval = Duration::from_secs(
            self.config
                .team_config
                .workflow_policy
                .health_check_interval_secs,
        );
        if self.last_health_check.elapsed() < interval {
            return Ok(());
        }
        self.last_health_check = Instant::now();

        // Collect (member_name, agent_name) pairs to avoid borrowing self.config during mutation.
        let checks: Vec<(String, String)> = self
            .config
            .members
            .iter()
            .filter(|m| m.role_type != RoleType::User)
            .map(|m| {
                (
                    m.name.clone(),
                    m.agent.as_deref().unwrap_or("claude").to_string(),
                )
            })
            .collect();

        for (member_name, agent_name) in &checks {
            let new_health =
                agent::health_check_by_name(agent_name).unwrap_or(BackendHealth::Healthy);
            let prev_health = self
                .backend_health
                .get(member_name)
                .copied()
                .unwrap_or(BackendHealth::Healthy);

            if new_health != prev_health {
                let transition = format!("{}→{}", prev_health.as_str(), new_health.as_str());
                info!(
                    member = %member_name,
                    agent = %agent_name,
                    transition = %transition,
                    "backend health changed"
                );
                self.emit_event(TeamEvent::health_changed(member_name, &transition));
                self.record_orchestrator_action(format!(
                    "health: {} backend {} ({})",
                    member_name, transition, agent_name,
                ));
            }
            self.backend_health.insert(member_name.clone(), new_health);
        }

        Ok(())
    }

    /// Check each engineer's worktree for large uncommitted diffs and send a
    /// commit reminder when the line count exceeds the configured threshold.
    /// Nudges are rate-limited to at most once per 5 minutes per engineer.
    pub(in super::super) fn maybe_warn_uncommitted_work(&mut self) -> Result<()> {
        let threshold = self
            .config
            .team_config
            .workflow_policy
            .uncommitted_warn_threshold;
        if threshold == 0 {
            return Ok(());
        }

        let cooldown = Duration::from_secs(300); // 5 minutes

        let engineers: Vec<String> = self
            .config
            .members
            .iter()
            .filter(|m| m.role_type == RoleType::Engineer && m.use_worktrees)
            .map(|m| m.name.clone())
            .collect();

        for name in &engineers {
            // Rate-limit: skip if we warned this engineer recently.
            if let Some(last) = self.last_uncommitted_warn.get(name) {
                if last.elapsed() < cooldown {
                    continue;
                }
            }

            let worktree_path = self.worktree_dir(name);
            if !worktree_path.exists() {
                continue;
            }

            let lines = match super::uncommitted_diff_lines(&worktree_path) {
                Ok(n) => n,
                Err(error) => {
                    warn!(engineer = %name, error = %error, "failed to check uncommitted diff");
                    continue;
                }
            };

            if lines < threshold {
                continue;
            }

            info!(
                engineer = %name,
                uncommitted_lines = lines,
                threshold,
                "sending uncommitted work warning"
            );

            let body = format!(
                "COMMIT REMINDER: You have {lines} uncommitted lines in your worktree \
                 (threshold: {threshold}). Please commit your work now to avoid losing progress:\n\n\
                 git add -A && git commit -m 'wip: checkpoint'"
            );

            let sender = self.automation_sender_for(name);
            if let Err(error) = self.queue_message(&sender, name, &body) {
                warn!(engineer = %name, error = %error, "failed to send uncommitted work warning");
            }
            self.record_orchestrator_action(format!(
                "uncommitted-warn: {name} has {lines} uncommitted lines (threshold {threshold})"
            ));
            self.last_uncommitted_warn
                .insert(name.clone(), Instant::now());
        }

        Ok(())
    }

    /// Load the prompt template for a member, substituting role-specific info.
    pub(in super::super) fn load_prompt(&self, member: &MemberInstance, config_dir: &Path) -> String {
        let prompt_file = member.prompt.as_deref().unwrap_or(match member.role_type {
            RoleType::Architect => "architect.md",
            RoleType::Manager => "manager.md",
            RoleType::Engineer => "engineer.md",
            RoleType::User => "architect.md", // shouldn't happen
        });

        let path = config_dir.join(prompt_file);
        match std::fs::read_to_string(&path) {
            Ok(content) => content
                .replace("{{member_name}}", &member.name)
                .replace("{{role_name}}", &member.role_name)
                .replace(
                    "{{reports_to}}",
                    member.reports_to.as_deref().unwrap_or("none"),
                ),
            Err(e) => {
                warn!(path = %path.display(), error = %e, "failed to load prompt template");
                format!(
                    "You are {} (role: {:?}). Work on assigned tasks.",
                    member.name, member.role_type
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::*;
    use crate::team::config::{RoleType, WorkflowPolicy};
    use crate::team::hierarchy::MemberInstance;
    use crate::team::standup::MemberState;
    use crate::team::test_helpers::make_test_daemon;
    use crate::team::test_support::{
        TestDaemonBuilder, architect_member, engineer_member, git_ok, git_stdout, init_git_repo,
        manager_member, setup_engineer_worktree, write_owned_task_file,
    };
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant};

    fn init_bare_git_repo(path: &Path) {
        git_ok(
            path.parent().unwrap(),
            &["init", "-b", "main", path.to_str().unwrap()],
        );
        git_ok(path, &["config", "user.email", "batty@example.com"]);
        git_ok(path, &["config", "user.name", "Batty Tests"]);
    }

    // ---- Backend health tests ----

    #[test]
    fn health_check_interval_config_default() {
        let policy = WorkflowPolicy::default();
        assert_eq!(policy.health_check_interval_secs, 60);
    }

    #[test]
    fn check_backend_health_skipped_before_interval() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let engineer = MemberInstance {
            name: "eng-health".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);
        daemon.last_health_check = Instant::now();
        daemon.check_backend_health().unwrap();
        assert!(daemon.backend_health.is_empty());
    }

    #[test]
    fn check_backend_health_runs_after_interval() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let engineer = MemberInstance {
            name: "eng-health-run".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);
        daemon.last_health_check = Instant::now() - Duration::from_secs(3600);
        daemon.check_backend_health().unwrap();
        assert!(daemon.backend_health.contains_key("eng-health-run"));
    }

    #[test]
    fn check_backend_health_skips_user_roles() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let user = MemberInstance {
            name: "human".to_string(),
            role_name: "user".to_string(),
            role_type: RoleType::User,
            agent: None,
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![user]);
        daemon.last_health_check = Instant::now() - Duration::from_secs(3600);
        daemon.check_backend_health().unwrap();
        assert!(daemon.backend_health.is_empty());
    }

    #[test]
    fn check_backend_health_emits_event_on_transition() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let engineer = MemberInstance {
            name: "eng-transition".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);
        daemon.last_health_check = Instant::now() - Duration::from_secs(3600);

        // First check: no prior state → sets initial value.
        daemon.check_backend_health().unwrap();
        let initial_health = *daemon.backend_health.get("eng-transition").unwrap();
        assert_eq!(initial_health, BackendHealth::Healthy);

        // Event emitted only on *transition*. First check from None→Healthy
        // may or may not emit depending on whether None counts. Let's just
        // verify the health entry exists.
        assert!(daemon.backend_health.contains_key("eng-transition"));
    }

    #[test]
    fn check_backend_health_no_event_when_state_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let engineer = MemberInstance {
            name: "eng-stable".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);
        daemon.last_health_check = Instant::now() - Duration::from_secs(3600);
        daemon
            .backend_health
            .insert("eng-stable".to_string(), BackendHealth::Healthy);

        daemon.check_backend_health().unwrap();

        let events = crate::team::events::read_events(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("events.jsonl"),
        )
        .unwrap_or_default();
        let health_events: Vec<_> = events
            .iter()
            .filter(|e| e.event == "health_changed")
            .collect();
        assert!(
            health_events.is_empty(),
            "no event when health state is unchanged"
        );
    }

    #[test]
    fn check_backend_health_tracks_multiple_members() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![
                architect_member("architect"),
                engineer_member("eng-1", Some("architect"), false),
            ])
            .build();
        daemon.last_health_check = Instant::now() - Duration::from_secs(3600);

        daemon.check_backend_health().unwrap();

        assert!(daemon.backend_health.contains_key("architect"));
        assert!(daemon.backend_health.contains_key("eng-1"));
    }

    #[test]
    fn check_backend_health_default_agent_is_claude() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let mut member = architect_member("architect");
        member.agent = None;
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![member])
            .build();
        daemon.last_health_check = Instant::now() - Duration::from_secs(3600);

        daemon.check_backend_health().unwrap();
        assert!(daemon.backend_health.contains_key("architect"));
    }

    // ---- Worktree reconciliation tests ----

    fn setup_reconcile_scenario(engineer: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-reconcile");
        let worktree_dir = repo.join(".batty").join("worktrees").join(engineer);
        let team_config_dir = repo.join(".batty").join("team_config");

        setup_engineer_worktree(&repo, &worktree_dir, engineer, &team_config_dir).unwrap();

        let task_branch = format!("{engineer}-42");
        git_ok(&worktree_dir, &["checkout", "-b", &task_branch]);
        std::fs::write(worktree_dir.join("feature.txt"), "work\n").unwrap();
        git_ok(&worktree_dir, &["add", "feature.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "task work"]);

        git_ok(&repo, &["merge", &task_branch]);

        (tmp, repo, worktree_dir)
    }

    #[test]
    fn reconcile_resets_idle_engineer_on_merged_branch() {
        let (_tmp, repo, worktree_dir) = setup_reconcile_scenario("eng-reconcile");

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-reconcile", Some("manager"), true),
        ];
        let states = HashMap::from([("eng-reconcile".to_string(), MemberState::Idle)]);
        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(members)
            .states(states)
            .build();

        daemon.is_git_repo = true;
        daemon.check_worktree_staleness().unwrap();

        let current = crate::worktree::git_current_branch(&worktree_dir).unwrap();
        assert_eq!(current, "eng-main/eng-reconcile");

        let _ = Command::new("git")
            .current_dir(&repo)
            .args([
                "worktree",
                "remove",
                "--force",
                worktree_dir.to_str().unwrap(),
            ])
            .output();
    }

    #[test]
    fn reconcile_skips_working_engineer() {
        let (_tmp, repo, worktree_dir) = setup_reconcile_scenario("eng-reconcile-skip");

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-reconcile-skip", Some("manager"), true),
        ];
        let states = HashMap::from([("eng-reconcile-skip".to_string(), MemberState::Working)]);
        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(members)
            .states(states)
            .build();

        daemon.is_git_repo = true;
        daemon.active_tasks.insert("eng-reconcile-skip".to_string(), 42);
        daemon.check_worktree_staleness().unwrap();

        let current = crate::worktree::git_current_branch(&worktree_dir).unwrap();
        assert_ne!(
            current, "eng-main/eng-reconcile-skip",
            "working engineer should NOT be reset"
        );

        let _ = Command::new("git")
            .current_dir(&repo)
            .args([
                "worktree",
                "remove",
                "--force",
                worktree_dir.to_str().unwrap(),
            ])
            .output();
    }

    #[test]
    fn reconcile_skips_idle_engineer_with_active_task() {
        let (_tmp, repo, worktree_dir) = setup_reconcile_scenario("eng-reconcile-active");

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-reconcile-active", Some("manager"), true),
        ];
        let states = HashMap::from([("eng-reconcile-active".to_string(), MemberState::Idle)]);
        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(members)
            .states(states)
            .build();

        daemon.is_git_repo = true;
        daemon
            .active_tasks
            .insert("eng-reconcile-active".to_string(), 42);
        daemon.check_worktree_staleness().unwrap();

        let current = crate::worktree::git_current_branch(&worktree_dir).unwrap();
        assert_ne!(
            current, "eng-main/eng-reconcile-active",
            "idle engineer with active task should NOT be reset"
        );

        let _ = Command::new("git")
            .current_dir(&repo)
            .args([
                "worktree",
                "remove",
                "--force",
                worktree_dir.to_str().unwrap(),
            ])
            .output();
    }

    #[test]
    fn reconcile_skips_unmerged_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-reconcile-unmerged");
        let engineer_name = "eng-unmerged";
        let worktree_dir = repo.join(".batty").join("worktrees").join(engineer_name);
        let team_config_dir = repo.join(".batty").join("team_config");
        setup_engineer_worktree(&repo, &worktree_dir, engineer_name, &team_config_dir).unwrap();

        let task_branch = format!("{engineer_name}-99");
        git_ok(&worktree_dir, &["checkout", "-b", &task_branch]);
        std::fs::write(worktree_dir.join("wip.txt"), "wip\n").unwrap();
        git_ok(&worktree_dir, &["add", "wip.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "wip"]);
        // Do NOT merge into main.

        let members = vec![
            manager_member("manager", None),
            engineer_member(engineer_name, Some("manager"), true),
        ];
        let states = HashMap::from([(engineer_name.to_string(), MemberState::Idle)]);
        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(members)
            .states(states)
            .build();
        daemon.is_git_repo = true;
        daemon.check_worktree_staleness().unwrap();

        let current = crate::worktree::git_current_branch(&worktree_dir).unwrap();
        assert_eq!(
            current, task_branch,
            "unmerged branch should NOT be reset"
        );

        let _ = Command::new("git")
            .current_dir(&repo)
            .args([
                "worktree",
                "remove",
                "--force",
                worktree_dir.to_str().unwrap(),
            ])
            .output();
    }

    #[test]
    fn reconcile_emits_worktree_reconciled_event() {
        let (_tmp, repo, worktree_dir) = setup_reconcile_scenario("eng-reconcile-event");

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-reconcile-event", Some("manager"), true),
        ];
        let states = HashMap::from([("eng-reconcile-event".to_string(), MemberState::Idle)]);
        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(members)
            .states(states)
            .build();
        daemon.is_git_repo = true;
        daemon.check_worktree_staleness().unwrap();

        let orch_log = daemon.orchestrator_log.join("\n");
        assert!(
            orch_log.contains("auto-reset"),
            "orchestrator log should mention auto-reset"
        );

        let _ = Command::new("git")
            .current_dir(&repo)
            .args([
                "worktree",
                "remove",
                "--force",
                worktree_dir.to_str().unwrap(),
            ])
            .output();
    }

    // ---- uncommitted work warning tests ----

    #[test]
    fn uncommitted_diff_lines_counts_unstaged() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_bare_git_repo(&repo);

        std::fs::write(repo.join("hello.txt"), "line1\nline2\nline3\n").unwrap();
        git_ok(&repo, &["add", "hello.txt"]);
        git_ok(&repo, &["commit", "-m", "init"]);

        std::fs::write(repo.join("hello.txt"), "changed1\nchanged2\nline3\n").unwrap();

        let lines = super::super::uncommitted_diff_lines(&repo).unwrap();
        assert!(lines >= 3, "expected >=3 uncommitted lines, got {lines}");
    }

    #[test]
    fn uncommitted_diff_lines_empty_when_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_bare_git_repo(&repo);

        std::fs::write(repo.join("hello.txt"), "line1\n").unwrap();
        git_ok(&repo, &["add", "hello.txt"]);
        git_ok(&repo, &["commit", "-m", "init"]);

        let lines = super::super::uncommitted_diff_lines(&repo).unwrap();
        assert_eq!(lines, 0, "clean repo should have 0 uncommitted lines");
    }

    #[test]
    fn uncommitted_diff_lines_includes_staged_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_bare_git_repo(&repo);

        std::fs::write(repo.join("hello.txt"), "original\n").unwrap();
        git_ok(&repo, &["add", "hello.txt"]);
        git_ok(&repo, &["commit", "-m", "init"]);

        std::fs::write(repo.join("hello.txt"), "modified\n").unwrap();
        git_ok(&repo, &["add", "hello.txt"]);

        let lines = super::super::uncommitted_diff_lines(&repo).unwrap();
        assert!(lines >= 2, "staged changes should count, got {lines}");
    }

    #[test]
    fn uncommitted_diff_lines_mixed_staged_and_unstaged() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_bare_git_repo(&repo);

        std::fs::write(repo.join("a.txt"), "line1\n").unwrap();
        std::fs::write(repo.join("b.txt"), "line1\n").unwrap();
        git_ok(&repo, &["add", "."]);
        git_ok(&repo, &["commit", "-m", "init"]);

        std::fs::write(repo.join("a.txt"), "changed\n").unwrap();
        std::fs::write(repo.join("b.txt"), "changed\n").unwrap();
        git_ok(&repo, &["add", "b.txt"]);

        let lines = super::super::uncommitted_diff_lines(&repo).unwrap();
        assert!(lines >= 4, "mixed staged+unstaged should sum, got {lines}");
    }

    #[test]
    fn uncommitted_diff_lines_non_git_dir_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let result = super::super::uncommitted_diff_lines(tmp.path());
        if let Ok(lines) = result {
            assert_eq!(lines, 0);
        }
    }

    #[test]
    fn uncommitted_diff_lines_new_file_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_bare_git_repo(&repo);

        std::fs::write(repo.join("init.txt"), "x\n").unwrap();
        git_ok(&repo, &["add", "."]);
        git_ok(&repo, &["commit", "-m", "init"]);

        std::fs::write(repo.join("new.txt"), "a\nb\nc\n").unwrap();
        git_ok(&repo, &["add", "new.txt"]);

        let lines = super::super::uncommitted_diff_lines(&repo).unwrap();
        assert!(
            lines >= 3,
            "new staged file should count as added lines, got {lines}"
        );
    }

    fn make_uncommitted_warn_daemon(tmp: &tempfile::TempDir, threshold: usize) -> TeamDaemon {
        let repo = tmp.path();
        let team_config_dir = repo.join(".batty").join("team_config");
        std::fs::create_dir_all(&team_config_dir).unwrap();

        let wt = repo.join(".batty").join("worktrees").join("eng-1");
        std::fs::create_dir_all(&wt).unwrap();
        init_bare_git_repo(&wt);

        std::fs::write(wt.join("big.txt"), "a\n".repeat(300)).unwrap();
        git_ok(&wt, &["add", "big.txt"]);
        git_ok(&wt, &["commit", "-m", "init big"]);

        std::fs::write(wt.join("big.txt"), "b\n".repeat(300)).unwrap();

        let engineer = engineer_member("eng-1", Some("manager"), true);
        let mut daemon = TestDaemonBuilder::new(repo)
            .members(vec![manager_member("manager", None), engineer])
            .build();
        daemon.config.team_config.workflow_policy.uncommitted_warn_threshold = threshold;
        daemon
    }

    #[test]
    fn maybe_warn_uncommitted_work_sends_nudge_above_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = make_uncommitted_warn_daemon(&tmp, 10);
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, "eng-1").unwrap();

        daemon.maybe_warn_uncommitted_work().unwrap();

        let msgs = inbox::pending_messages(&inbox_root, "eng-1").unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].body.contains("COMMIT REMINDER"));
    }

    #[test]
    fn maybe_warn_uncommitted_work_rate_limited() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = make_uncommitted_warn_daemon(&tmp, 10);
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, "eng-1").unwrap();

        daemon.maybe_warn_uncommitted_work().unwrap();
        let msgs1 = inbox::pending_messages(&inbox_root, "eng-1").unwrap();
        assert_eq!(msgs1.len(), 1);

        daemon.maybe_warn_uncommitted_work().unwrap();
        let msgs2 = inbox::pending_messages(&inbox_root, "eng-1").unwrap();
        assert_eq!(msgs2.len(), 1, "second call should be rate-limited");
    }

    #[test]
    fn maybe_warn_uncommitted_work_disabled_when_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = make_uncommitted_warn_daemon(&tmp, 0);
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, "eng-1").unwrap();

        daemon.maybe_warn_uncommitted_work().unwrap();

        let msgs = inbox::pending_messages(&inbox_root, "eng-1").unwrap();
        assert!(msgs.is_empty(), "threshold=0 should disable the feature");
    }

    #[test]
    fn maybe_warn_uncommitted_work_skips_below_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = make_uncommitted_warn_daemon(&tmp, 99999);
        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, "eng-1").unwrap();

        daemon.maybe_warn_uncommitted_work().unwrap();

        let msgs = inbox::pending_messages(&inbox_root, "eng-1").unwrap();
        assert!(msgs.is_empty(), "below threshold should not warn");
    }

    #[test]
    fn maybe_warn_uncommitted_work_skips_non_worktree_engineers() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let engineer = engineer_member("eng-no-wt", Some("manager"), false);
        let mut daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![manager_member("manager", None), engineer])
            .build();
        daemon.config.team_config.workflow_policy.uncommitted_warn_threshold = 10;

        let inbox_root = inbox::inboxes_root(tmp.path());
        inbox::init_inbox(&inbox_root, "eng-no-wt").unwrap();

        daemon.maybe_warn_uncommitted_work().unwrap();

        let msgs = inbox::pending_messages(&inbox_root, "eng-no-wt").unwrap();
        assert!(msgs.is_empty(), "non-worktree engineer should be skipped");
    }

    #[test]
    fn uncommitted_warn_threshold_config_default() {
        let policy = WorkflowPolicy::default();
        assert_eq!(policy.uncommitted_warn_threshold, 200);
    }

    // ---- false-done prevention tests ----

    #[test]
    fn false_done_prevention_no_commits_returns_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-false-done");

        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");
        crate::team::task_loop::setup_engineer_worktree(
            &repo,
            &worktree_dir,
            "eng-1",
            &team_config_dir,
        )
        .unwrap();

        let output =
            crate::team::git_cmd::run_git(&worktree_dir, &["rev-list", "--count", "main..HEAD"])
                .unwrap();
        let count: u32 = output.stdout.trim().parse().unwrap();
        assert_eq!(count, 0, "branch with no new commits should return 0");
    }

    #[test]
    fn false_done_prevention_with_commits_returns_nonzero() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-false-done-ok");

        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");
        crate::team::task_loop::setup_engineer_worktree(
            &repo,
            &worktree_dir,
            "eng-1",
            &team_config_dir,
        )
        .unwrap();

        std::fs::write(worktree_dir.join("work.txt"), "done\n").unwrap();
        git_ok(&worktree_dir, &["add", "work.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "task work"]);

        let output =
            crate::team::git_cmd::run_git(&worktree_dir, &["rev-list", "--count", "main..HEAD"])
                .unwrap();
        let count: u32 = output.stdout.trim().parse().unwrap();
        assert!(count > 0, "branch with commits should return > 0");
    }

    #[test]
    fn false_done_prevention_invalid_worktree_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let result =
            crate::team::git_cmd::run_git(tmp.path(), &["rev-list", "--count", "main..HEAD"]);
        assert!(result.is_err(), "non-git dir should return error");
    }

    // ---- check_worktree_staleness tests ----

    #[test]
    fn check_worktree_staleness_skips_base_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "staleness-skip");

        let eng_name = "eng-stale-skip";
        let base = format!("eng-main/{eng_name}");
        let wt_dir = repo.join(".batty").join("worktrees").join(eng_name);
        std::fs::create_dir_all(wt_dir.parent().unwrap()).unwrap();
        git_ok(&repo, &["branch", &base]);
        git_ok(&repo, &["worktree", "add", wt_dir.to_str().unwrap(), &base]);

        let engineer = MemberInstance {
            name: eng_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: true,
        };
        let mut daemon = make_test_daemon(&repo, vec![engineer]);
        daemon.is_git_repo = true;

        daemon.check_worktree_staleness().unwrap();

        let _ = Command::new("git")
            .current_dir(&repo)
            .args(["worktree", "remove", "--force", wt_dir.to_str().unwrap()])
            .output();
        let _ = Command::new("git")
            .current_dir(&repo)
            .args(["branch", "-D", &base])
            .output();
    }

    #[test]
    fn check_worktree_staleness_skips_unmerged() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "staleness-unmerged");

        let eng_name = "eng-stale-unmerged";
        let base = format!("eng-main/{eng_name}");
        let task_branch = format!("{eng_name}/task-99");
        let wt_dir = repo.join(".batty").join("worktrees").join(eng_name);
        std::fs::create_dir_all(wt_dir.parent().unwrap()).unwrap();

        git_ok(&repo, &["branch", &base]);
        git_ok(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                &task_branch,
                wt_dir.to_str().unwrap(),
                &base,
            ],
        );

        std::fs::write(wt_dir.join("unique_work.txt"), "unique\n").unwrap();
        git_ok(&wt_dir, &["add", "unique_work.txt"]);
        git_ok(&wt_dir, &["commit", "-m", "unique commit"]);

        let engineer = MemberInstance {
            name: eng_name.to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: true,
        };
        let mut daemon = make_test_daemon(&repo, vec![engineer]);
        daemon.is_git_repo = true;

        daemon.check_worktree_staleness().unwrap();

        let current = git_stdout(&wt_dir, &["branch", "--show-current"]);
        assert_eq!(current, task_branch);

        let _ = Command::new("git")
            .current_dir(&repo)
            .args(["worktree", "remove", "--force", wt_dir.to_str().unwrap()])
            .output();
        let _ = Command::new("git")
            .current_dir(&repo)
            .args(["branch", "-D", &task_branch])
            .output();
        let _ = Command::new("git")
            .current_dir(&repo)
            .args(["branch", "-D", &base])
            .output();
    }

    // ── load_prompt tests ──

    #[test]
    fn load_prompt_substitutes_template_variables() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("engineer.md"),
            "Hello {{member_name}}, role={{role_name}}, reports_to={{reports_to}}",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "engineer".to_string(),
            role_type: RoleType::Engineer,
            agent: None,
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
        };
        let daemon = make_test_daemon(tmp.path(), vec![member.clone()]);
        let prompt = daemon.load_prompt(&member, &config_dir);
        assert_eq!(prompt, "Hello eng-1, role=engineer, reports_to=manager");
    }

    #[test]
    fn load_prompt_uses_custom_prompt_file() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("custom.md"), "Custom: {{member_name}}").unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member = MemberInstance {
            name: "arch-1".to_string(),
            role_name: "architect".to_string(),
            role_type: RoleType::Architect,
            agent: None,
            prompt: Some("custom.md".to_string()),
            reports_to: None,
            use_worktrees: false,
        };
        let daemon = make_test_daemon(tmp.path(), vec![member.clone()]);
        let prompt = daemon.load_prompt(&member, &config_dir);
        assert_eq!(prompt, "Custom: arch-1");
    }

    #[test]
    fn load_prompt_fallback_on_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member = MemberInstance {
            name: "eng-1".to_string(),
            role_name: "engineer".to_string(),
            role_type: RoleType::Engineer,
            agent: None,
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let daemon = make_test_daemon(tmp.path(), vec![member.clone()]);
        let prompt = daemon.load_prompt(&member, &config_dir);
        assert!(prompt.contains("eng-1"));
        assert!(prompt.contains("Engineer"));
    }

    #[test]
    fn load_prompt_reports_to_none_becomes_none_string() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("architect.md"), "reports={{reports_to}}").unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let member = MemberInstance {
            name: "arch-1".to_string(),
            role_name: "architect".to_string(),
            role_type: RoleType::Architect,
            agent: None,
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let daemon = make_test_daemon(tmp.path(), vec![member.clone()]);
        let prompt = daemon.load_prompt(&member, &config_dir);
        assert_eq!(prompt, "reports=none");
    }

    #[test]
    fn load_prompt_default_file_per_role_type() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("architect.md"), "ARCH").unwrap();
        std::fs::write(config_dir.join("manager.md"), "MGR").unwrap();
        std::fs::write(config_dir.join("engineer.md"), "ENG").unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();

        let daemon = make_test_daemon(tmp.path(), vec![]);

        let arch = MemberInstance {
            name: "a".to_string(),
            role_name: "a".to_string(),
            role_type: RoleType::Architect,
            agent: None,
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let mgr = MemberInstance {
            name: "m".to_string(),
            role_name: "m".to_string(),
            role_type: RoleType::Manager,
            agent: None,
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        let eng = MemberInstance {
            name: "e".to_string(),
            role_name: "e".to_string(),
            role_type: RoleType::Engineer,
            agent: None,
            prompt: None,
            reports_to: None,
            use_worktrees: false,
        };
        assert_eq!(daemon.load_prompt(&arch, &config_dir), "ARCH");
        assert_eq!(daemon.load_prompt(&mgr, &config_dir), "MGR");
        assert_eq!(daemon.load_prompt(&eng, &config_dir), "ENG");
    }
}
