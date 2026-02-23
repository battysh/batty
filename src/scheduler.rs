//! DAG-driven board scheduler for parallel agent dispatch.
//!
//! The scheduler is responsible for:
//! - polling board state,
//! - computing dependency-ready frontier from the DAG,
//! - dispatching ready tasks to idle agents via `kanban-md pick --claim`,
//! - verifying claim ownership from task frontmatter,
//! - detecting completions, deadlocks, and stuck agents.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::Deserialize;

use crate::dag::TaskDag;
use crate::task::{self, Task, TaskBattyConfig};

#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    #[allow(dead_code)]
    pub poll_interval: Duration,
    pub stuck_timeout: Duration,
    pub kanban_program: String,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(5),
            stuck_timeout: Duration::from_secs(300),
            kanban_program: "kanban-md".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentState {
    Idle,
    Busy {
        task_id: u32,
        last_progress_epoch: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dispatch {
    pub agent: String,
    pub task_id: u32,
    pub task_title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StuckAgent {
    pub agent: String,
    pub task_id: u32,
    pub stalled_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerTick {
    pub ready: Vec<u32>,
    pub completed: Vec<u32>,
    pub dispatched: Vec<Dispatch>,
    pub all_done: bool,
    pub total_tasks: usize,
    pub done_tasks: usize,
    pub deadlocked: bool,
    pub stuck: Vec<StuckAgent>,
}

#[derive(Debug, Clone)]
pub struct SchedulerTask {
    pub id: u32,
    pub title: String,
    pub status: String,
    pub depends_on: Vec<u32>,
    pub source_path: PathBuf,
}

impl SchedulerTask {
    fn as_task(&self) -> Task {
        Task {
            id: self.id,
            title: self.title.clone(),
            status: self.status.clone(),
            priority: String::new(),
            tags: vec![],
            depends_on: self.depends_on.clone(),
            description: String::new(),
            batty_config: None::<TaskBattyConfig>,
            source_path: self.source_path.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct BoardSnapshot {
    pub tasks: BTreeMap<u32, SchedulerTask>,
}

impl BoardSnapshot {
    fn completed_ids(&self) -> HashSet<u32> {
        self.tasks
            .iter()
            .filter_map(|(id, task)| (task.status == "done").then_some(*id))
            .collect()
    }

    fn remaining_count(&self) -> usize {
        self.tasks
            .values()
            .filter(|task| task.status != "done" && task.status != "archived")
            .count()
    }

    fn task_path(&self, task_id: u32) -> Option<&Path> {
        self.tasks.get(&task_id).map(|task| task.source_path.as_path())
    }
}

#[derive(Debug, Clone)]
pub struct CommandResult {
    pub status_success: bool,
    pub stdout: String,
    pub stderr: String,
}

pub trait CommandRunner: Send + Sync + 'static {
    fn run(&self, program: &str, args: &[String], cwd: &Path) -> Result<CommandResult>;
}

#[derive(Debug, Default, Clone)]
pub struct ShellCommandRunner;

impl CommandRunner for ShellCommandRunner {
    fn run(&self, program: &str, args: &[String], cwd: &Path) -> Result<CommandResult> {
        let output = Command::new(program)
            .current_dir(cwd)
            .args(args)
            .output()
            .with_context(|| format!("failed to run command '{}' in {}", program, cwd.display()))?;

        Ok(CommandResult {
            status_success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }
}

pub struct Scheduler<R: CommandRunner = ShellCommandRunner> {
    board_dir: PathBuf,
    config: SchedulerConfig,
    runner: R,
    agent_states: HashMap<String, AgentState>,
    known_done: HashSet<u32>,
}

impl<R: CommandRunner> Scheduler<R> {
    pub fn new(
        board_dir: PathBuf,
        agent_names: Vec<String>,
        config: SchedulerConfig,
        runner: R,
    ) -> Self {
        let agent_states = agent_names
            .into_iter()
            .map(|name| (name, AgentState::Idle))
            .collect();
        Self {
            board_dir,
            config,
            runner,
            agent_states,
            known_done: HashSet::new(),
        }
    }

    pub fn poll_board(&self) -> Result<BoardSnapshot> {
        let tasks_dir = self.board_dir.join("tasks");
        let tasks = task::load_tasks_from_dir(&tasks_dir)
            .with_context(|| format!("failed to load tasks from {}", tasks_dir.display()))?;

        let mut map = BTreeMap::new();
        for task in tasks {
            map.insert(
                task.id,
                SchedulerTask {
                    id: task.id,
                    title: task.title,
                    status: task.status,
                    depends_on: task.depends_on,
                    source_path: task.source_path,
                },
            );
        }

        Ok(BoardSnapshot { tasks: map })
    }

    pub fn ready_frontier(&self, snapshot: &BoardSnapshot) -> Result<Vec<u32>> {
        let dag_input = snapshot
            .tasks
            .values()
            .map(SchedulerTask::as_task)
            .collect::<Vec<_>>();
        let dag = TaskDag::from_tasks(&dag_input)?;
        Ok(dag.ready_set(&snapshot.completed_ids()))
    }

    pub fn tick(&mut self, now_epoch: u64) -> Result<SchedulerTick> {
        let snapshot = self.poll_board()?;
        let completed = self.detect_completions(&snapshot);
        self.mark_completed_agents_idle(&completed);

        let ready = self.ready_frontier(&snapshot)?;
        let dispatched = self.dispatch_ready(&snapshot, &ready, now_epoch)?;
        let deadlocked = self.detect_deadlock(&snapshot, &ready);
        let stuck = self.detect_stuck(now_epoch);

        Ok(SchedulerTick {
            ready,
            completed,
            dispatched,
            all_done: snapshot.remaining_count() == 0,
            total_tasks: snapshot.remaining_count() + snapshot.completed_ids().len(),
            done_tasks: snapshot.completed_ids().len(),
            deadlocked,
            stuck,
        })
    }

    pub fn agent_states(&self) -> &HashMap<String, AgentState> {
        &self.agent_states
    }

    #[allow(dead_code)]
    pub fn mark_agent_progress(&mut self, agent: &str, now_epoch: u64) {
        if let Some(AgentState::Busy {
            task_id: _,
            last_progress_epoch,
        }) = self.agent_states.get_mut(agent)
        {
            *last_progress_epoch = now_epoch;
        }
    }

    pub fn handle_agent_crash(&mut self, agent: &str) -> Result<()> {
        let busy_task = match self.agent_states.get(agent) {
            Some(AgentState::Busy { task_id, .. }) => Some(*task_id),
            _ => None,
        };
        if let Some(task_id) = busy_task {
            self.release_claim(task_id)?;
        }
        self.agent_states.insert(agent.to_string(), AgentState::Idle);
        Ok(())
    }

    fn detect_completions(&mut self, snapshot: &BoardSnapshot) -> Vec<u32> {
        let done_now = snapshot.completed_ids();
        let mut newly_done = done_now
            .difference(&self.known_done)
            .copied()
            .collect::<Vec<_>>();
        newly_done.sort_unstable();
        self.known_done = done_now;
        newly_done
    }

    fn mark_completed_agents_idle(&mut self, completed: &[u32]) {
        if completed.is_empty() {
            return;
        }
        let completed_set: HashSet<u32> = completed.iter().copied().collect();
        for state in self.agent_states.values_mut() {
            if let AgentState::Busy { task_id, .. } = state
                && completed_set.contains(task_id)
            {
                *state = AgentState::Idle;
            }
        }
    }

    fn dispatch_ready(
        &mut self,
        snapshot: &BoardSnapshot,
        ready: &[u32],
        now_epoch: u64,
    ) -> Result<Vec<Dispatch>> {
        if ready.is_empty() {
            return Ok(Vec::new());
        }

        let ready_set: HashSet<u32> = ready.iter().copied().collect();
        let mut dispatched = Vec::new();
        for agent in self.idle_agents() {
            let Some(task_id) = self.try_pick_for_agent(&agent, &ready_set)? else {
                continue;
            };

            self.verify_claim(snapshot, task_id, &agent)?;
            self.agent_states.insert(
                agent.clone(),
                AgentState::Busy {
                    task_id,
                    last_progress_epoch: now_epoch,
                },
            );
            let task_title = snapshot
                .tasks
                .get(&task_id)
                .map(|task| task.title.clone())
                .unwrap_or_else(|| "task".to_string());
            dispatched.push(Dispatch {
                agent,
                task_id,
                task_title,
            });
        }

        Ok(dispatched)
    }

    fn idle_agents(&self) -> Vec<String> {
        let mut names = self
            .agent_states
            .iter()
            .filter_map(|(name, state)| matches!(state, AgentState::Idle).then_some(name.clone()))
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    fn try_pick_for_agent(&self, agent: &str, ready_set: &HashSet<u32>) -> Result<Option<u32>> {
        let args = vec![
            "pick".to_string(),
            "--claim".to_string(),
            agent.to_string(),
            "--status".to_string(),
            "backlog".to_string(),
            "--move".to_string(),
            "in-progress".to_string(),
            "--dir".to_string(),
            self.board_dir.display().to_string(),
        ];

        let result = self
            .runner
            .run(&self.config.kanban_program, &args, &self.board_dir)?;
        if !result.status_success {
            return Ok(None);
        }

        let Some(task_id) = parse_picked_task_id(&result.stdout) else {
            bail!("scheduler dispatch could not parse picked task id from output");
        };

        if !ready_set.contains(&task_id) {
            self.release_claim(task_id)?;
            bail!(
                "scheduler dispatched non-ready task #{} for agent {}",
                task_id,
                agent
            );
        }

        Ok(Some(task_id))
    }

    fn verify_claim(&self, snapshot: &BoardSnapshot, task_id: u32, agent: &str) -> Result<()> {
        let Some(task_path) = snapshot.task_path(task_id) else {
            self.release_claim(task_id)?;
            bail!("picked task #{} not found in current board snapshot", task_id);
        };

        let claimed_by = parse_claimed_by(task_path)?;
        if claimed_by.as_deref() != Some(agent) {
            self.release_claim(task_id)?;
            bail!(
                "claim verification failed for task #{}: expected '{}', found {:?}",
                task_id,
                agent,
                claimed_by
            );
        }

        Ok(())
    }

    fn release_claim(&self, task_id: u32) -> Result<()> {
        let args = vec![
            "edit".to_string(),
            task_id.to_string(),
            "--release".to_string(),
            "--dir".to_string(),
            self.board_dir.display().to_string(),
        ];
        let result = self
            .runner
            .run(&self.config.kanban_program, &args, &self.board_dir)?;
        if !result.status_success {
            bail!(
                "failed to release claim for task #{}: {}",
                task_id,
                result.stderr.trim()
            );
        }
        Ok(())
    }

    fn detect_deadlock(&self, snapshot: &BoardSnapshot, ready: &[u32]) -> bool {
        let all_idle = self
            .agent_states
            .values()
            .all(|state| matches!(state, AgentState::Idle));
        ready.is_empty() && all_idle && snapshot.remaining_count() > 0
    }

    fn detect_stuck(&self, now_epoch: u64) -> Vec<StuckAgent> {
        let mut stuck = Vec::new();
        for (agent, state) in &self.agent_states {
            if let AgentState::Busy {
                task_id,
                last_progress_epoch,
            } = state
            {
                let stalled_secs = now_epoch.saturating_sub(*last_progress_epoch);
                if stalled_secs >= self.config.stuck_timeout.as_secs() {
                    stuck.push(StuckAgent {
                        agent: agent.clone(),
                        task_id: *task_id,
                        stalled_secs,
                    });
                }
            }
        }
        stuck.sort_by(|a, b| a.agent.cmp(&b.agent));
        stuck
    }
}

#[derive(Debug, Deserialize)]
struct ClaimFrontmatter {
    #[serde(default)]
    claimed_by: Option<String>,
}

fn parse_claimed_by(path: &Path) -> Result<Option<String>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read task file {}", path.display()))?;
    let (frontmatter, _) = split_frontmatter(&content)?;
    let parsed: ClaimFrontmatter = serde_yaml::from_str(frontmatter)
        .with_context(|| format!("failed to parse frontmatter for {}", path.display()))?;
    Ok(parsed.claimed_by)
}

fn split_frontmatter(content: &str) -> Result<(&str, &str)> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        bail!("task file missing opening frontmatter delimiter");
    }
    let after_open = trimmed[3..].strip_prefix('\n').unwrap_or(&trimmed[3..]);
    let close = after_open
        .find("\n---")
        .context("task file missing closing frontmatter delimiter")?;
    let fm = &after_open[..close];
    let body = &after_open[close + 4..];
    Ok((fm, body))
}

fn parse_picked_task_id(stdout: &str) -> Option<u32> {
    let re = Regex::new(r"task #(\d+)").ok()?;
    let captures = re.captures(stdout)?;
    captures.get(1)?.as_str().parse::<u32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    fn write_task(
        dir: &Path,
        id: u32,
        status: &str,
        depends_on: &[u32],
        claimed_by: Option<&str>,
    ) -> PathBuf {
        let file = dir.join(format!("{id:03}-task-{id}.md"));
        let deps = if depends_on.is_empty() {
            "depends_on: []".to_string()
        } else {
            let rendered = depends_on
                .iter()
                .map(|dep| format!("  - {dep}"))
                .collect::<Vec<_>>()
                .join("\n");
            format!("depends_on:\n{rendered}")
        };
        let claim_line = claimed_by
            .map(|agent| format!("claimed_by: {agent}\n"))
            .unwrap_or_default();
        let body = format!(
            "---\nid: {id}\ntitle: task-{id}\nstatus: {status}\npriority: high\ntags: []\n{deps}\n{claim_line}class: standard\n---\n\nTask {id}\n"
        );
        std::fs::write(&file, body).unwrap();
        file
    }

    #[derive(Default)]
    struct MockRunner {
        calls: Mutex<Vec<Vec<String>>>,
        outputs: Mutex<VecDeque<CommandResult>>,
    }

    impl MockRunner {
        fn with_outputs(outputs: Vec<CommandResult>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                outputs: Mutex::new(outputs.into()),
            }
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl CommandRunner for MockRunner {
        fn run(&self, program: &str, args: &[String], _cwd: &Path) -> Result<CommandResult> {
            let mut full = vec![program.to_string()];
            full.extend(args.iter().cloned());
            self.calls.lock().unwrap().push(full);

            let mut outputs = self.outputs.lock().unwrap();
            let next = outputs.pop_front().unwrap_or(CommandResult {
                status_success: false,
                stdout: String::new(),
                stderr: "mock exhausted".to_string(),
            });
            Ok(next)
        }
    }

    fn scheduler_with_runner(
        board_dir: PathBuf,
        agents: Vec<String>,
        runner: MockRunner,
    ) -> Scheduler<MockRunner> {
        Scheduler::new(board_dir, agents, SchedulerConfig::default(), runner)
    }

    #[test]
    fn ready_frontier_uses_dag_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(&tasks_dir, 1, "done", &[], None);
        write_task(&tasks_dir, 2, "backlog", &[1], None);
        write_task(&tasks_dir, 3, "backlog", &[2], None);

        let scheduler = scheduler_with_runner(
            tmp.path().to_path_buf(),
            vec!["agent-a".to_string()],
            MockRunner::default(),
        );
        let snapshot = scheduler.poll_board().unwrap();
        let ready = scheduler.ready_frontier(&snapshot).unwrap();
        assert_eq!(ready, vec![2]);
    }

    #[test]
    fn tick_dispatches_ready_task_to_idle_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(&tasks_dir, 1, "done", &[], None);
        write_task(&tasks_dir, 2, "backlog", &[1], Some("agent-a"));

        let runner = MockRunner::with_outputs(vec![CommandResult {
            status_success: true,
            stdout: "Picked and moved task #2: example".to_string(),
            stderr: String::new(),
        }]);
        let mut scheduler = scheduler_with_runner(
            tmp.path().to_path_buf(),
            vec!["agent-a".to_string()],
            runner,
        );
        let tick = scheduler.tick(100).unwrap();

        assert_eq!(
            tick.dispatched,
            vec![Dispatch {
                agent: "agent-a".to_string(),
                task_id: 2,
                task_title: "task-2".to_string()
            }]
        );
        assert!(!tick.all_done);
        assert_eq!(tick.total_tasks, 2);
        assert_eq!(tick.done_tasks, 1);
        assert_eq!(
            scheduler.agent_states().get("agent-a"),
            Some(&AgentState::Busy {
                task_id: 2,
                last_progress_epoch: 100
            })
        );
    }

    #[test]
    fn claim_verification_failure_releases_task() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(&tasks_dir, 1, "done", &[], None);
        write_task(&tasks_dir, 2, "backlog", &[1], Some("someone-else"));

        let runner = MockRunner::with_outputs(vec![
            CommandResult {
                status_success: true,
                stdout: "Picked and moved task #2: example".to_string(),
                stderr: String::new(),
            },
            CommandResult {
                status_success: true,
                stdout: "Updated task #2".to_string(),
                stderr: String::new(),
            },
        ]);
        let mut scheduler = scheduler_with_runner(
            tmp.path().to_path_buf(),
            vec!["agent-a".to_string()],
            runner,
        );
        let err = scheduler.tick(10).unwrap_err().to_string();
        assert!(err.contains("claim verification failed"));

        let calls = scheduler.runner.calls();
        assert!(
            calls.iter().any(|call| call.iter().any(|arg| arg == "--release")),
            "expected release call after failed claim verification, calls={calls:?}"
        );
    }

    #[test]
    fn handle_agent_crash_releases_claim_and_marks_idle() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(&tasks_dir, 1, "done", &[], None);
        write_task(&tasks_dir, 2, "backlog", &[1], Some("agent-a"));

        let runner = MockRunner::with_outputs(vec![
            CommandResult {
                status_success: true,
                stdout: "Picked and moved task #2: example".to_string(),
                stderr: String::new(),
            },
            CommandResult {
                status_success: true,
                stdout: "Updated task #2".to_string(),
                stderr: String::new(),
            },
        ]);

        let mut scheduler = scheduler_with_runner(
            tmp.path().to_path_buf(),
            vec!["agent-a".to_string()],
            runner,
        );
        let _ = scheduler.tick(42).unwrap();
        scheduler.handle_agent_crash("agent-a").unwrap();

        assert_eq!(
            scheduler.agent_states().get("agent-a"),
            Some(&AgentState::Idle)
        );
    }

    #[test]
    fn deadlock_and_stuck_are_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(&tasks_dir, 1, "review", &[], None);

        let mut scheduler = Scheduler::new(
            tmp.path().to_path_buf(),
            vec!["agent-a".to_string()],
            SchedulerConfig {
                stuck_timeout: Duration::from_secs(30),
                ..SchedulerConfig::default()
            },
            MockRunner::default(),
        );
        scheduler.agent_states.insert(
            "agent-a".to_string(),
            AgentState::Busy {
                task_id: 99,
                last_progress_epoch: 10,
            },
        );

        let tick = scheduler.tick(50).unwrap();
        assert!(!tick.all_done);
        assert_eq!(tick.total_tasks, 1);
        assert_eq!(tick.done_tasks, 0);
        assert!(!tick.deadlocked);
        assert_eq!(tick.stuck.len(), 1);
        assert_eq!(tick.stuck[0].agent, "agent-a");
        assert_eq!(tick.stuck[0].task_id, 99);
    }

    #[test]
    fn parse_picked_task_id_extracts_identifier() {
        assert_eq!(
            parse_picked_task_id("Picked and moved task #12: test"),
            Some(12)
        );
        assert_eq!(parse_picked_task_id("nothing here"), None);
    }

    #[test]
    fn scheduler_dispatches_distinct_tasks_across_agents() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        write_task(&tasks_dir, 1, "backlog", &[], Some("agent-a"));
        write_task(&tasks_dir, 2, "backlog", &[], Some("agent-b"));

        let runner = MockRunner::with_outputs(vec![
            CommandResult {
                status_success: true,
                stdout: "Picked and moved task #1: example".to_string(),
                stderr: String::new(),
            },
            CommandResult {
                status_success: true,
                stdout: "Picked and moved task #2: example".to_string(),
                stderr: String::new(),
            },
        ]);
        let mut scheduler = scheduler_with_runner(
            tmp.path().to_path_buf(),
            vec!["agent-a".to_string(), "agent-b".to_string()],
            runner,
        );

        let tick = scheduler.tick(100).unwrap();
        assert_eq!(tick.dispatched.len(), 2);
        let task_ids = tick
            .dispatched
            .iter()
            .map(|dispatch| dispatch.task_id)
            .collect::<HashSet<_>>();
        assert_eq!(task_ids.len(), 2, "expected unique dispatched task IDs");
    }

    #[test]
    fn empty_board_is_immediately_complete() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("tasks")).unwrap();

        let mut scheduler = scheduler_with_runner(
            tmp.path().to_path_buf(),
            vec!["agent-a".to_string()],
            MockRunner::default(),
        );
        let tick = scheduler.tick(100).unwrap();
        assert!(tick.all_done);
        assert!(!tick.deadlocked);
        assert!(tick.ready.is_empty());
        assert!(tick.dispatched.is_empty());
    }
}
