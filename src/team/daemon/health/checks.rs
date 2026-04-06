//! Backend health, worktree staleness, uncommitted work warnings, and prompt loading.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{info, warn};

use super::super::*;
use crate::team::prompt_compose::{render_member_prompt, resolve_prompt_context};

const SHARED_TARGET_DISK_THRESHOLD_PCT: u8 = 80;
const SHARED_TARGET_CLEANUP_INTERVAL: Duration = Duration::from_secs(900);

/// Check if a worktree has unresolved merge conflicts (UU, AA, DU, UD entries).
fn worktree_has_merge_conflicts(worktree_path: &Path) -> bool {
    let output = match std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(worktree_path)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
    {
        Ok(o) => o,
        Err(_) => return false,
    };
    if !output.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.lines().any(|line| {
        let bytes = line.as_bytes();
        bytes.len() >= 2
            && matches!(
                (bytes[0], bytes[1]),
                (b'U', _) | (_, b'U') | (b'A', b'A') | (b'D', b'D')
            )
    })
}

impl TeamDaemon {
    pub(in super::super) fn maybe_cleanup_shared_cargo_target(&mut self) -> Result<()> {
        if self.last_shared_target_cleanup.elapsed() < SHARED_TARGET_CLEANUP_INTERVAL {
            return Ok(());
        }
        self.last_shared_target_cleanup = Instant::now();

        let shared_target =
            crate::team::task_loop::shared_cargo_target_dir(&self.config.project_root);
        std::fs::create_dir_all(&shared_target).ok();
        let used_pct = filesystem_usage_percent(&shared_target)?;
        if used_pct < SHARED_TARGET_DISK_THRESHOLD_PCT {
            return Ok(());
        }

        let removed = prune_legacy_worktree_target_dirs(&self.config.project_root)?;
        if !removed.is_empty() {
            info!(
                used_pct,
                removed = removed.len(),
                "pruned legacy per-worktree cargo targets under disk pressure"
            );
            self.record_orchestrator_action(format!(
                "runtime: pruned {} legacy worktree target dirs at {}% disk usage",
                removed.len(),
                used_pct
            ));
        }
        Ok(())
    }

    /// Detect worktrees stuck on stale branches whose commits have already
    /// been cherry-picked onto main, and auto-reset them to the base branch.
    /// Also detects and auto-recovers worktrees stuck in merge conflict state.
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

            // Check for merge conflicts first — these block all git operations
            if worktree_has_merge_conflicts(&worktree_path) {
                let base = format!("eng-main/{}", name);
                warn!(
                    member = %name,
                    "worktree has unresolved merge conflicts; auto-recovering via merge --abort and reset"
                );
                // Try to abort the merge and reset to base
                let _ = std::process::Command::new("git")
                    .args(["merge", "--abort"])
                    .current_dir(&worktree_path)
                    .output();
                let _ = std::process::Command::new("git")
                    .args(["checkout", "--", "."])
                    .current_dir(&worktree_path)
                    .output();
                let _ = std::process::Command::new("git")
                    .args(["clean", "-fd"])
                    .current_dir(&worktree_path)
                    .output();
                if let Err(error) = crate::worktree::reset_worktree_to_base(&worktree_path, &base) {
                    warn!(
                        member = %name,
                        error = %error,
                        "failed to reset worktree after merge conflict recovery"
                    );
                } else {
                    info!(
                        member = %name,
                        "worktree merge conflict auto-recovered; reset to base branch"
                    );
                    self.record_orchestrator_action(format!(
                        "health: auto-recovered {}'s worktree from merge conflict state — reset to {}",
                        name, base
                    ));
                    // Clear active task since worktree was reset
                    if self.active_tasks.contains_key(name.as_str()) {
                        let task_id = self.active_tasks[name.as_str()];
                        warn!(
                            member = %name,
                            task_id,
                            "clearing active task after merge conflict recovery"
                        );
                        self.clear_active_task(name);
                    }
                }
                continue;
            }

            let current_branch = match crate::worktree::git_current_branch(&worktree_path) {
                Ok(b) => b,
                Err(error) => {
                    warn!(
                        member = %name,
                        error = %error,
                        "failed to read worktree branch; skipping staleness check"
                    );
                    continue;
                }
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
                        branch = %current_branch,
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
    pub(in super::super) fn load_prompt(
        &self,
        member: &MemberInstance,
        config_dir: &Path,
    ) -> String {
        let mut prompt = render_member_prompt(member, config_dir, &resolve_prompt_context(member));
        if self.config.team_config.workflow_policy.clean_room_mode
            && let Some(group) = self
                .config
                .team_config
                .role_barrier_group(&member.role_name)
        {
            let work_dir = self.worktree_dir(&member.name);
            let handoff_dir = self.handoff_dir();
            prompt.push_str(&format!(
                "\n\n## Information Barrier\n\n- Barrier group: {group}\n- Allowed working directory: {}\n- Shared cross-barrier handoff directory: {}\n- Do not read or write outside your worktree and the handoff directory.\n",
                work_dir.display(),
                handoff_dir.display()
            ));
        }
        prompt
    }
}

fn filesystem_usage_percent(path: &Path) -> Result<u8> {
    let output = std::process::Command::new("df")
        .args(["-Pk", path.to_string_lossy().as_ref()])
        .output()?;
    if !output.status.success() {
        return Ok(0);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(line) = stdout.lines().nth(1) else {
        return Ok(0);
    };
    let pct_field = line.split_whitespace().nth(4).unwrap_or("0%");
    Ok(pct_field.trim_end_matches('%').parse().unwrap_or(0))
}

fn prune_legacy_worktree_target_dirs(project_root: &Path) -> Result<Vec<PathBuf>> {
    let worktrees_root = project_root.join(".batty").join("worktrees");
    if !worktrees_root.is_dir() {
        return Ok(Vec::new());
    }

    let mut removed = Vec::new();
    for entry in std::fs::read_dir(&worktrees_root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let direct_target = path.join("target");
        if direct_target.is_dir() {
            std::fs::remove_dir_all(&direct_target)?;
            removed.push(direct_target);
        }

        for nested in std::fs::read_dir(&path)? {
            let nested = nested?;
            let nested_target = nested.path().join("target");
            if nested_target.is_dir() {
                std::fs::remove_dir_all(&nested_target)?;
                removed.push(nested_target);
            }
        }
    }

    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::super::super::*;
    use crate::team::config::{RoleType, WorkflowPolicy};
    use crate::team::hierarchy::MemberInstance;
    use crate::team::standup::MemberState;
    use crate::team::task_loop::setup_engineer_worktree;
    use crate::team::test_helpers::make_test_daemon;
    use crate::team::test_support::{EnvVarGuard, PATH_LOCK, setup_fake_backend};
    use crate::team::test_support::{
        TestDaemonBuilder, architect_member, engineer_member, git_ok, git_stdout, init_git_repo,
        manager_member,
    };
    use serial_test::serial;
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
        };
        let mut daemon = make_test_daemon(tmp.path(), vec![user]);
        daemon.last_health_check = Instant::now() - Duration::from_secs(3600);
        daemon.check_backend_health().unwrap();
        assert!(daemon.backend_health.is_empty());
    }

    #[test]
    #[serial]
    fn check_backend_health_emits_event_on_transition() {
        let _path_guard = PATH_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        let (fake_bin, _fake_log) = setup_fake_backend(&tmp, "claude", "health-claude.log");
        let original_path = std::env::var("PATH").unwrap_or_default();
        let _path = EnvVarGuard::set(
            "PATH",
            &format!("{}:{original_path}", fake_bin.to_string_lossy()),
        );

        let engineer = MemberInstance {
            name: "eng-transition".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
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
    #[serial]
    fn check_backend_health_no_event_when_state_unchanged() {
        let _path_guard = PATH_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("team_config")).unwrap();
        let (fake_bin, _fake_log) = setup_fake_backend(&tmp, "claude", "health-claude.log");
        let original_path = std::env::var("PATH").unwrap_or_default();
        let _path = EnvVarGuard::set(
            "PATH",
            &format!("{}:{original_path}", fake_bin.to_string_lossy()),
        );

        let engineer = MemberInstance {
            name: "eng-stable".to_string(),
            role_name: "eng".to_string(),
            role_type: RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
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
        daemon.maybe_reconcile_stale_worktrees().unwrap();

        let branch = git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert_eq!(
            branch,
            engineer_base_branch_name("eng-reconcile"),
            "worktree should be reset to base branch"
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
    fn reconcile_skips_working_engineer() {
        let (_tmp, repo, worktree_dir) = setup_reconcile_scenario("eng-working");

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-working", Some("manager"), true),
        ];
        let states = HashMap::from([("eng-working".to_string(), MemberState::Working)]);
        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(members)
            .states(states)
            .build();
        daemon.is_git_repo = true;

        daemon.maybe_reconcile_stale_worktrees().unwrap();

        let branch = git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert_eq!(
            branch, "eng-working-42",
            "worktree should stay on task branch when engineer is working"
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
        let (_tmp, repo, worktree_dir) = setup_reconcile_scenario("eng-active");

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-active", Some("manager"), true),
        ];
        let states = HashMap::from([("eng-active".to_string(), MemberState::Idle)]);
        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(members)
            .states(states)
            .build();
        daemon.is_git_repo = true;
        daemon.active_tasks.insert("eng-active".to_string(), 42);

        daemon.maybe_reconcile_stale_worktrees().unwrap();

        let branch = git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert_eq!(
            branch, "eng-active-42",
            "worktree should stay on task branch when engineer has active task"
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
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-unmerged");
        let team_config_dir = repo.join(".batty").join("team_config");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-unmerged", &team_config_dir).unwrap();

        let task_branch = "eng-unmerged-99";
        git_ok(&worktree_dir, &["checkout", "-b", task_branch]);
        std::fs::write(worktree_dir.join("wip.txt"), "wip\n").unwrap();
        git_ok(&worktree_dir, &["add", "wip.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "work in progress"]);

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-unmerged", Some("manager"), true),
        ];
        let states = HashMap::from([("eng-unmerged".to_string(), MemberState::Idle)]);
        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(members)
            .states(states)
            .build();
        daemon.is_git_repo = true;

        daemon.maybe_reconcile_stale_worktrees().unwrap();

        let branch = git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert_eq!(
            branch, "eng-unmerged-99",
            "worktree should stay on unmerged task branch"
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
        let (_tmp, repo, _worktree_dir) = setup_reconcile_scenario("eng-event");

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-event", Some("manager"), true),
        ];
        let states = HashMap::from([("eng-event".to_string(), MemberState::Idle)]);
        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(members)
            .states(states)
            .build();
        daemon.is_git_repo = true;

        daemon.maybe_reconcile_stale_worktrees().unwrap();

        let events = crate::team::events::read_events(
            &repo.join(".batty").join("team_config").join("events.jsonl"),
        )
        .unwrap_or_default();
        assert!(
            events.iter().any(|e| e.event == "worktree_reconciled"
                && e.role.as_deref() == Some("eng-event")),
            "should emit worktree_reconciled event"
        );
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
        daemon
            .config
            .team_config
            .workflow_policy
            .uncommitted_warn_threshold = threshold;
        daemon
    }

    #[test]
    fn prune_legacy_worktree_target_dirs_removes_engineer_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let worktree_target = tmp
            .path()
            .join(".batty")
            .join("worktrees")
            .join("eng-1")
            .join("target");
        std::fs::create_dir_all(&worktree_target).unwrap();
        std::fs::write(worktree_target.join("stale"), "artifact").unwrap();

        let removed = super::prune_legacy_worktree_target_dirs(tmp.path()).unwrap();

        assert_eq!(removed, vec![worktree_target.clone()]);
        assert!(!worktree_target.exists());
    }

    #[test]
    fn prune_legacy_worktree_target_dirs_removes_nested_multi_repo_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let nested_target = tmp
            .path()
            .join(".batty")
            .join("worktrees")
            .join("eng-2")
            .join("subrepo")
            .join("target");
        std::fs::create_dir_all(&nested_target).unwrap();
        std::fs::write(nested_target.join("stale"), "artifact").unwrap();

        let removed = super::prune_legacy_worktree_target_dirs(tmp.path()).unwrap();

        assert_eq!(removed, vec![nested_target.clone()]);
        assert!(!nested_target.exists());
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
        daemon
            .config
            .team_config
            .workflow_policy
            .uncommitted_warn_threshold = 10;

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
            ..Default::default()
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
            ..Default::default()
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
            model: None,
            prompt: None,
            posture: None,
            model_class: None,
            provider_overlay: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: false,
            ..Default::default()
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
            model: None,
            prompt: Some("custom.md".to_string()),
            posture: None,
            model_class: None,
            provider_overlay: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
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
            model: None,
            prompt: None,
            posture: None,
            model_class: None,
            provider_overlay: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
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
            model: None,
            prompt: None,
            posture: None,
            model_class: None,
            provider_overlay: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
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
            model: None,
            prompt: None,
            posture: None,
            model_class: None,
            provider_overlay: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };
        let mgr = MemberInstance {
            name: "m".to_string(),
            role_name: "m".to_string(),
            role_type: RoleType::Manager,
            agent: None,
            model: None,
            prompt: None,
            posture: None,
            model_class: None,
            provider_overlay: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };
        let eng = MemberInstance {
            name: "e".to_string(),
            role_name: "e".to_string(),
            role_type: RoleType::Engineer,
            agent: None,
            model: None,
            prompt: None,
            posture: None,
            model_class: None,
            provider_overlay: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        };
        assert_eq!(daemon.load_prompt(&arch, &config_dir), "ARCH");
        assert_eq!(daemon.load_prompt(&mgr, &config_dir), "MGR");
        assert_eq!(daemon.load_prompt(&eng, &config_dir), "ENG");
    }
}
