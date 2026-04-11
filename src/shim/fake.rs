//! In-process fake shim for the scenario framework.
//!
//! A [`FakeShim`] wraps the **child** side of a real
//! [`protocol::socketpair`](crate::shim::protocol::socketpair) and responds
//! synchronously on the same thread as the test. Zero subprocess spawns,
//! fully deterministic.
//!
//! Typical usage from a scenario:
//!
//! ```ignore
//! let (mut fake, parent) = FakeShim::new_pair("eng-1");
//! daemon.scenario_hooks().insert_fake_shim("eng-1", parent, 0, "claude", "claude", worktree);
//! fake.queue(ShimBehavior::complete_with("done", vec![]));
//! daemon.tick();
//! let events = fake.process_inbound(&worktree).unwrap();
//! ```
//!
//! Commands flow daemon → parent → child → fake. Events flow fake → child →
//! parent → daemon (picked up on the next `daemon.poll_shim_handles()`
//! inside the next tick).
//!
//! Ticket #638 of the scenario framework execution plan.

#![cfg(any(test, feature = "scenario-test"))]

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};

use crate::shim::protocol::{self, Channel, Command, Event, ShimState};

/// Scripted response a [`FakeShim`] produces when it receives a command.
#[derive(Debug, Clone)]
pub enum ShimBehavior {
    /// Happy-path completion. Emits
    /// `StateChanged(Idle→Working) → MessageDelivered → StateChanged(Working→Idle) → Completion`
    /// and (if `files_touched` is non-empty) commits the listed files on
    /// the current HEAD of `worktree_dir`.
    CompleteWith {
        response: String,
        files_touched: Vec<(PathBuf, String)>,
    },
    /// Respond with an [`Event::Error`] for the incoming command.
    ErrorOut { command: String, reason: String },
    /// Respond with [`Event::ContextExhausted`].
    ContextExhausted { message: String },
    /// Swallow the command silently. Used for silent-death scenarios.
    Silent,
    /// Respond with [`Event::Completion`] but do not write any files.
    NarrationOnly { response: String },
    /// First call responds with narration-only, subsequent calls complete
    /// cleanly with the given files.
    NarrationFirstThenClean {
        clean_response: String,
        files: Vec<(PathBuf, String)>,
    },
    /// Emit a verbatim event sequence without any protocol scaffolding.
    Script(Vec<Event>),
    /// Apply the inner behavior exactly once, then fall back to the default.
    Once(Box<ShimBehavior>),
}

impl ShimBehavior {
    /// Convenience constructor for a plain clean completion.
    pub fn complete_with(
        response: impl Into<String>,
        files_touched: Vec<(PathBuf, String)>,
    ) -> Self {
        Self::CompleteWith {
            response: response.into(),
            files_touched,
        }
    }

    /// Convenience constructor for an error response.
    pub fn error(command: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::ErrorOut {
            command: command.into(),
            reason: reason.into(),
        }
    }

    /// Convenience constructor for a narration-only completion.
    pub fn narration_only(response: impl Into<String>) -> Self {
        Self::NarrationOnly {
            response: response.into(),
        }
    }
}

/// In-process fake shim. Owns the child side of a real socketpair and
/// synchronously responds to commands queued by the orchestrator daemon.
pub struct FakeShim {
    pub name: String,
    child: Channel,
    behaviors: VecDeque<ShimBehavior>,
    default_behavior: Option<ShimBehavior>,
    handled_commands: Vec<Command>,
    state: ShimState,
    /// Incrementing message counter used for synthetic message ids.
    message_seq: u64,
}

impl FakeShim {
    /// Construct a fake shim and return it along with the parent side of
    /// the socketpair (to be handed to [`ScenarioHooks::insert_fake_shim`]).
    ///
    /// Both sides of the socket are configured with a short read timeout
    /// so the daemon's `try_recv_event` (parent side) and the fake's
    /// `process_inbound` (child side) return `WouldBlock`/`TimedOut`
    /// instead of blocking forever on empty sockets.
    pub fn new_pair(name: &str) -> Result<(Self, Channel)> {
        let (parent, child) = protocol::socketpair()?;
        let child_channel = Channel::new(child);
        let mut parent_channel = Channel::new(parent);
        parent_channel.set_read_timeout(Some(Duration::from_millis(10)))?;

        let mut fake = Self {
            name: name.to_string(),
            child: child_channel,
            behaviors: VecDeque::new(),
            default_behavior: None,
            handled_commands: Vec::new(),
            state: ShimState::Idle,
            message_seq: 0,
        };
        // Non-blocking drains require a read timeout on the child socket.
        fake.child
            .set_read_timeout(Some(Duration::from_millis(10)))?;
        Ok((fake, parent_channel))
    }

    /// Queue a one-shot behavior. Consumed in FIFO order when commands
    /// arrive.
    pub fn queue(&mut self, behavior: ShimBehavior) {
        self.behaviors.push_back(behavior);
    }

    /// Set the default behavior used when the queued list is empty.
    pub fn set_default(&mut self, behavior: ShimBehavior) {
        self.default_behavior = Some(behavior);
    }

    /// All commands the fake has observed so far. Order-preserving.
    pub fn handled_commands(&self) -> &[Command] {
        &self.handled_commands
    }

    /// Current fake shim state.
    pub fn state(&self) -> ShimState {
        self.state
    }

    /// Drain all pending commands from the channel and respond to each one
    /// according to its matching behavior. Returns the full event sequence
    /// that was sent back to the daemon (useful for assertions).
    ///
    /// `worktree_dir` is the directory that `CompleteWith { files_touched }`
    /// variants commit into. Other variants ignore it.
    pub fn process_inbound(&mut self, worktree_dir: &Path) -> Result<Vec<Event>> {
        let mut emitted = Vec::new();
        loop {
            let received: Option<Command> = match self.child.recv::<Command>() {
                Ok(msg) => msg,
                Err(e) => {
                    if let Some(io_err) = e.downcast_ref::<io::Error>() {
                        if matches!(
                            io_err.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                        ) {
                            break;
                        }
                    }
                    return Err(e);
                }
            };
            let Some(command) = received else {
                // Clean EOF.
                break;
            };
            self.handled_commands.push(clone_command(&command));

            // Commands like Ping/GetState/Resize are infrastructure-level
            // and do not consume a behavior slot: respond in-band.
            if let Some(infra_events) = self.handle_infrastructure_command(&command)? {
                for ev in &infra_events {
                    self.child.send(ev)?;
                }
                emitted.extend(infra_events);
                continue;
            }

            // Pick the next scripted behavior.
            let behavior = self.next_behavior();
            let Some(behavior) = behavior else {
                // No behavior queued and no default: drop silently (tests
                // should set at least a default if they want parity).
                continue;
            };

            let events = self.apply_behavior(behavior, &command, worktree_dir)?;
            for ev in &events {
                self.child.send(ev)?;
            }
            emitted.extend(events);
        }
        Ok(emitted)
    }

    // -----------------------------------------------------------------
    // Internals
    // -----------------------------------------------------------------

    fn next_behavior(&mut self) -> Option<ShimBehavior> {
        if let Some(front) = self.behaviors.pop_front() {
            return Some(front);
        }
        self.default_behavior.clone()
    }

    fn handle_infrastructure_command(&mut self, cmd: &Command) -> Result<Option<Vec<Event>>> {
        match cmd {
            Command::Ping => Ok(Some(vec![Event::Pong])),
            Command::GetState => Ok(Some(vec![Event::State {
                state: self.state,
                since_secs: 0,
            }])),
            Command::Resize { .. } => Ok(Some(Vec::new())),
            Command::Shutdown { .. } | Command::Kill => {
                self.state = ShimState::Dead;
                Ok(Some(vec![Event::Died {
                    exit_code: Some(0),
                    last_lines: String::new(),
                }]))
            }
            _ => Ok(None),
        }
    }

    fn apply_behavior(
        &mut self,
        behavior: ShimBehavior,
        _command: &Command,
        worktree_dir: &Path,
    ) -> Result<Vec<Event>> {
        match behavior {
            ShimBehavior::CompleteWith {
                response,
                files_touched,
            } => {
                if !files_touched.is_empty() {
                    commit_files(worktree_dir, &files_touched)?;
                }
                let message_id = self.next_message_id();
                let events = vec![
                    Event::StateChanged {
                        from: self.state,
                        to: ShimState::Working,
                        summary: "fake_shim: started working".to_string(),
                    },
                    Event::MessageDelivered {
                        id: message_id.clone(),
                    },
                    Event::StateChanged {
                        from: ShimState::Working,
                        to: ShimState::Idle,
                        summary: "fake_shim: finished".to_string(),
                    },
                    Event::Completion {
                        message_id: Some(message_id),
                        response,
                        last_lines: String::new(),
                    },
                ];
                self.state = ShimState::Idle;
                Ok(events)
            }
            ShimBehavior::NarrationOnly { response } => {
                let message_id = self.next_message_id();
                let events = vec![
                    Event::StateChanged {
                        from: self.state,
                        to: ShimState::Working,
                        summary: "fake_shim: narration-only pass".to_string(),
                    },
                    Event::MessageDelivered {
                        id: message_id.clone(),
                    },
                    Event::StateChanged {
                        from: ShimState::Working,
                        to: ShimState::Idle,
                        summary: "fake_shim: narration-only done".to_string(),
                    },
                    Event::Completion {
                        message_id: Some(message_id),
                        response,
                        last_lines: String::new(),
                    },
                ];
                self.state = ShimState::Idle;
                Ok(events)
            }
            ShimBehavior::NarrationFirstThenClean {
                clean_response,
                files,
            } => {
                // First invocation: narration-only. Queue a
                // CompleteWith at the front so the next command uses it.
                self.behaviors.push_front(ShimBehavior::CompleteWith {
                    response: clean_response,
                    files_touched: files,
                });
                self.apply_behavior(
                    ShimBehavior::NarrationOnly {
                        response: "narration-first".to_string(),
                    },
                    _command,
                    worktree_dir,
                )
            }
            ShimBehavior::ErrorOut {
                command: cmd_name,
                reason,
            } => Ok(vec![
                Event::StateChanged {
                    from: self.state,
                    to: ShimState::Idle,
                    summary: "fake_shim: error".to_string(),
                },
                Event::Error {
                    command: cmd_name,
                    reason,
                },
            ]),
            ShimBehavior::ContextExhausted { message } => {
                self.state = ShimState::ContextExhausted;
                Ok(vec![Event::ContextExhausted {
                    message,
                    last_lines: String::new(),
                }])
            }
            ShimBehavior::Silent => Ok(Vec::new()),
            ShimBehavior::Script(events) => Ok(events),
            ShimBehavior::Once(inner) => self.apply_behavior(*inner, _command, worktree_dir),
        }
    }

    fn next_message_id(&mut self) -> String {
        self.message_seq += 1;
        format!("{}-msg-{}", self.name, self.message_seq)
    }
}

/// `Command` is not `Clone` (it holds `String`s from serde). We need a
/// manual shallow clone for test assertions.
fn clone_command(cmd: &Command) -> Command {
    match cmd {
        Command::SendMessage {
            from,
            body,
            message_id,
        } => Command::SendMessage {
            from: from.clone(),
            body: body.clone(),
            message_id: message_id.clone(),
        },
        Command::CaptureScreen { last_n_lines } => Command::CaptureScreen {
            last_n_lines: *last_n_lines,
        },
        Command::GetState => Command::GetState,
        Command::Resize { rows, cols } => Command::Resize {
            rows: *rows,
            cols: *cols,
        },
        Command::Shutdown {
            timeout_secs,
            reason,
        } => Command::Shutdown {
            timeout_secs: *timeout_secs,
            reason: *reason,
        },
        Command::Kill => Command::Kill,
        Command::Ping => Command::Ping,
    }
}

fn commit_files(worktree_dir: &Path, files: &[(PathBuf, String)]) -> Result<()> {
    if !worktree_dir.exists() {
        return Err(anyhow!(
            "fake_shim: worktree_dir does not exist: {}",
            worktree_dir.display()
        ));
    }
    for (rel, contents) in files {
        let target = worktree_dir.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("fake_shim: create parent dir {}", parent.display()))?;
        }
        std::fs::write(&target, contents)
            .with_context(|| format!("fake_shim: write {}", target.display()))?;
    }

    let git_env = [
        ("GIT_AUTHOR_NAME", "Batty FakeShim"),
        ("GIT_AUTHOR_EMAIL", "fake-shim@batty.test"),
        ("GIT_COMMITTER_NAME", "Batty FakeShim"),
        ("GIT_COMMITTER_EMAIL", "fake-shim@batty.test"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ];

    run_git(worktree_dir, &git_env, &["add", "-A"])?;
    run_git(
        worktree_dir,
        &git_env,
        &["commit", "-m", "fake_shim: scripted completion"],
    )?;
    Ok(())
}

fn run_git(dir: &Path, env: &[(&str, &str)], args: &[&str]) -> Result<()> {
    let mut command = StdCommand::new("git");
    command.current_dir(dir).args(args);
    for (k, v) in env {
        command.env(k, v);
    }
    let output = command
        .output()
        .with_context(|| format!("fake_shim: spawn git {:?}", args))?;
    if !output.status.success() {
        return Err(anyhow!(
            "fake_shim: git {:?} failed: stdout={} stderr={}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    fn init_git_repo(dir: &Path) {
        let env = [
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_SYSTEM", "/dev/null"),
        ];
        let mut init = StdCommand::new("git");
        init.current_dir(dir).args(["init", "-b", "main"]);
        for (k, v) in &env {
            init.env(k, v);
        }
        assert!(init.status().unwrap().success());
        // Seed an initial commit so `git add -A && git commit` has a
        // parent to attach to.
        std::fs::write(dir.join("README.md"), "seed\n").unwrap();
        run_git(
            dir,
            &[
                ("GIT_AUTHOR_NAME", "seed"),
                ("GIT_AUTHOR_EMAIL", "seed@batty.test"),
                ("GIT_COMMITTER_NAME", "seed"),
                ("GIT_COMMITTER_EMAIL", "seed@batty.test"),
                ("GIT_CONFIG_GLOBAL", "/dev/null"),
                ("GIT_CONFIG_SYSTEM", "/dev/null"),
            ],
            &["add", "README.md"],
        )
        .unwrap();
        run_git(
            dir,
            &[
                ("GIT_AUTHOR_NAME", "seed"),
                ("GIT_AUTHOR_EMAIL", "seed@batty.test"),
                ("GIT_COMMITTER_NAME", "seed"),
                ("GIT_COMMITTER_EMAIL", "seed@batty.test"),
                ("GIT_CONFIG_GLOBAL", "/dev/null"),
                ("GIT_CONFIG_SYSTEM", "/dev/null"),
            ],
            &["commit", "-m", "seed"],
        )
        .unwrap();
    }

    fn send_message(parent: &mut Channel, body: &str) {
        parent
            .send(&Command::SendMessage {
                from: "manager".to_string(),
                body: body.to_string(),
                message_id: Some("m-1".to_string()),
            })
            .unwrap();
    }

    #[test]
    fn fake_shim_responds_to_send_message_with_completion() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        let (mut fake, mut parent) = FakeShim::new_pair("eng-1").unwrap();

        fake.queue(ShimBehavior::complete_with("done", vec![]));
        send_message(&mut parent, "do the thing");

        let events = fake.process_inbound(tmp.path()).unwrap();

        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Completion { response, .. } if response == "done")),
            "expected Completion event, got {:?}",
            events
        );
        assert_eq!(fake.state(), ShimState::Idle);
    }

    #[test]
    fn fake_shim_writes_committed_files_to_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        let (mut fake, mut parent) = FakeShim::new_pair("eng-1").unwrap();

        fake.queue(ShimBehavior::complete_with(
            "wrote code",
            vec![(PathBuf::from("src/new.rs"), "pub fn ok() {}\n".into())],
        ));
        send_message(&mut parent, "implement src/new.rs");

        let _events = fake.process_inbound(tmp.path()).unwrap();

        // The file should exist
        let written = tmp.path().join("src/new.rs");
        assert!(written.exists(), "fake shim should create the file");
        // And there should be a new commit on main
        let log = StdCommand::new("git")
            .current_dir(tmp.path())
            .args(["log", "--oneline"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(log.status.success());
        let log_text = String::from_utf8_lossy(&log.stdout);
        assert!(
            log_text.contains("fake_shim: scripted completion"),
            "expected fake_shim commit, got git log:\n{log_text}"
        );
    }

    #[test]
    fn fake_shim_silent_behavior_drops_command() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        let (mut fake, mut parent) = FakeShim::new_pair("eng-1").unwrap();

        fake.queue(ShimBehavior::Silent);
        send_message(&mut parent, "silent");

        let events = fake.process_inbound(tmp.path()).unwrap();
        assert!(
            events.is_empty(),
            "silent behavior should emit nothing, got {:?}",
            events
        );
        assert_eq!(fake.handled_commands().len(), 1);
    }

    #[test]
    fn fake_shim_error_out_returns_error_event() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        let (mut fake, mut parent) = FakeShim::new_pair("eng-1").unwrap();

        fake.queue(ShimBehavior::error("send_message", "boom"));
        send_message(&mut parent, "break");

        let events = fake.process_inbound(tmp.path()).unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Error { reason, .. } if reason == "boom")),
            "expected Error event, got {:?}",
            events
        );
    }

    #[test]
    fn fake_shim_script_plays_events_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        let (mut fake, mut parent) = FakeShim::new_pair("eng-1").unwrap();

        fake.queue(ShimBehavior::Script(vec![
            Event::Warning {
                message: "warn-1".into(),
                idle_secs: None,
            },
            Event::Warning {
                message: "warn-2".into(),
                idle_secs: None,
            },
        ]));
        send_message(&mut parent, "script");

        let events = fake.process_inbound(tmp.path()).unwrap();
        let messages: Vec<String> = events
            .into_iter()
            .filter_map(|e| match e {
                Event::Warning { message, .. } => Some(message),
                _ => None,
            })
            .collect();
        assert_eq!(messages, vec!["warn-1".to_string(), "warn-2".to_string()]);
    }

    #[test]
    fn fake_shim_responds_to_ping_with_pong() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        let (mut fake, mut parent) = FakeShim::new_pair("eng-1").unwrap();

        parent.send(&Command::Ping).unwrap();
        let events = fake.process_inbound(tmp.path()).unwrap();
        assert!(
            events.iter().any(|e| matches!(e, Event::Pong)),
            "expected Pong, got {:?}",
            events
        );
    }

    #[test]
    fn fake_shim_narration_first_then_clean_uses_both() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        let (mut fake, mut parent) = FakeShim::new_pair("eng-1").unwrap();

        fake.queue(ShimBehavior::NarrationFirstThenClean {
            clean_response: "clean".into(),
            files: vec![(PathBuf::from("src/second.rs"), "// clean\n".into())],
        });

        // First call — narration
        send_message(&mut parent, "first");
        let first = fake.process_inbound(tmp.path()).unwrap();
        assert!(
            first.iter().any(|e| matches!(
                e,
                Event::Completion { response, .. } if response == "narration-first"
            )),
            "expected narration-first completion, got {:?}",
            first
        );
        assert!(
            !tmp.path().join("src/second.rs").exists(),
            "narration pass should not write files"
        );

        // Second call — clean completion
        send_message(&mut parent, "second");
        let second = fake.process_inbound(tmp.path()).unwrap();
        assert!(
            second.iter().any(|e| matches!(
                e,
                Event::Completion { response, .. } if response == "clean"
            )),
            "expected clean completion on second call, got {:?}",
            second
        );
        assert!(
            tmp.path().join("src/second.rs").exists(),
            "clean pass should write files"
        );
    }
}
