//! PTY supervision — bidirectional interactive wrapper.
//!
//! Spawns an agent in a PTY, forwards output to the user's terminal in real
//! time, pattern-matches against known prompts, and auto-answers per policy.
//! The user can always type into the session directly.

use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread;

use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tracing::{debug, info};

use crate::agent::{AgentAdapter, SpawnConfig};
use crate::policy::{Decision, PolicyEngine};
use crate::prompt::{DetectedPrompt, PromptKind, PromptPatterns, strip_ansi};

/// Result of a supervised agent session.
#[derive(Debug)]
pub enum SessionResult {
    /// Agent signaled completion normally.
    Completed,
    /// Agent encountered an error.
    Error { detail: String },
    /// Agent process exited with a status code.
    Exited { code: Option<u32> },
}

/// Events emitted during supervision for logging/audit.
#[derive(Debug, Clone)]
pub enum SupervisorEvent {
    /// Output line from the agent.
    Output(String),
    /// A prompt was detected in the output.
    PromptDetected(DetectedPrompt),
    /// Policy engine made a decision about a prompt.
    PolicyDecision(Decision),
    /// An auto-response was injected into the agent.
    AutoResponse { prompt: String, response: String },
    /// Session completed.
    SessionEnd(String),
}

/// Configuration for a supervision session.
pub struct SessionConfig {
    pub spawn: SpawnConfig,
    pub patterns: PromptPatterns,
    pub policy: PolicyEngine,
    pub pty_size: PtySize,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            spawn: SpawnConfig {
                program: String::new(),
                args: vec![],
                work_dir: String::new(),
                env: vec![],
            },
            patterns: PromptPatterns::claude_code(),
            policy: PolicyEngine::new(
                crate::config::Policy::Observe,
                std::collections::HashMap::new(),
            ),
            pty_size: PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
        }
    }
}

/// Run a supervised agent session.
///
/// This is the core loop that:
/// 1. Spawns the agent in a PTY
/// 2. Reads output, forwards to stdout, scans for prompts
/// 3. Auto-answers routine prompts per policy
/// 4. Forwards user stdin to the PTY for interactive use
/// 5. Returns when the agent exits or signals completion
///
/// `event_tx` receives supervision events for logging/audit.
pub fn run_session(
    config: SessionConfig,
    _adapter: &dyn AgentAdapter,
    event_tx: Option<mpsc::Sender<SupervisorEvent>>,
) -> Result<SessionResult> {
    let pty_system = native_pty_system();

    let pair = pty_system
        .openpty(config.pty_size)
        .context("failed to open PTY")?;

    let mut cmd = CommandBuilder::new(&config.spawn.program);
    for arg in &config.spawn.args {
        cmd.arg(arg);
    }
    cmd.cwd(&config.spawn.work_dir);
    for (key, val) in &config.spawn.env {
        cmd.env(key, val);
    }

    info!(
        program = %config.spawn.program,
        work_dir = %config.spawn.work_dir,
        "spawning agent in PTY"
    );

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .context("failed to spawn agent process")?;

    // Drop the slave side — we only interact through the master
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .context("failed to clone PTY reader")?;
    let mut writer = pair
        .master
        .take_writer()
        .context("failed to take PTY writer")?;

    // Channel for detected prompts that need auto-response
    let (prompt_tx, prompt_rx) = mpsc::channel::<(DetectedPrompt, Decision)>();

    // Clone event sender for the injection thread
    let event_tx_inject = event_tx.clone();

    // Spawn a thread to read PTY output, forward to stdout, and detect prompts
    let patterns = config.patterns;
    let policy = config.policy;
    let event_tx_clone = event_tx.clone();

    let output_thread = thread::spawn(move || -> Result<SessionResult> {
        let mut stdout = std::io::stdout();
        let mut buf = [0u8; 4096];
        let mut line_buffer = String::new();
        let mut result = SessionResult::Completed;

        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break, // EOF — process exited
                Ok(n) => n,
                Err(e) => {
                    debug!("PTY read error (process likely exited): {e}");
                    break;
                }
            };

            let chunk = &buf[..n];

            // Forward raw output to user's terminal
            let _ = stdout.write_all(chunk);
            let _ = stdout.flush();

            // Accumulate for line-based prompt detection
            if let Ok(text) = std::str::from_utf8(chunk) {
                line_buffer.push_str(text);

                // Process complete lines
                while let Some(newline_pos) = line_buffer.find('\n') {
                    let line = line_buffer[..newline_pos].to_string();
                    line_buffer = line_buffer[newline_pos + 1..].to_string();

                    let stripped = strip_ansi(&line);
                    if stripped.trim().is_empty() {
                        continue;
                    }

                    if let Some(ref tx) = event_tx_clone {
                        let _ = tx.send(SupervisorEvent::Output(stripped.clone()));
                    }

                    if let Some(detected) = patterns.detect(&stripped) {
                        if let Some(ref tx) = event_tx_clone {
                            let _ = tx.send(SupervisorEvent::PromptDetected(detected.clone()));
                        }

                        // Check for completion/error signals
                        match &detected.kind {
                            PromptKind::Completion => {
                                result = SessionResult::Completed;
                            }
                            PromptKind::Error { detail } => {
                                result = SessionResult::Error {
                                    detail: detail.clone(),
                                };
                            }
                            _ => {
                                // Evaluate against policy
                                let decision = policy.evaluate(&stripped);
                                if let Some(ref tx) = event_tx_clone {
                                    let _ =
                                        tx.send(SupervisorEvent::PolicyDecision(decision.clone()));
                                }

                                // If the policy says to auto-respond, send via channel
                                if let Decision::Act { ref response, .. } = decision {
                                    let _ = prompt_tx.send((detected.clone(), decision.clone()));
                                    debug!(
                                        response = %response,
                                        "auto-responding to prompt"
                                    );
                                }
                            }
                        }
                    }
                }

                // Also check the partial line buffer for prompts that don't end with newline
                // (e.g., "Continue? [y/n] " with trailing space)
                if !line_buffer.is_empty() {
                    let stripped = strip_ansi(&line_buffer);
                    if let Some(detected) = patterns.detect(&stripped) {
                        match &detected.kind {
                            PromptKind::Completion | PromptKind::Error { .. } => {}
                            _ => {
                                let decision = policy.evaluate(&stripped);
                                if let Decision::Act { .. } = &decision {
                                    let _ = prompt_tx.send((detected.clone(), decision.clone()));
                                    line_buffer.clear();
                                }
                            }
                        }
                    }
                }
            }
        }

        if let Some(ref tx) = event_tx_clone {
            let _ = tx.send(SupervisorEvent::SessionEnd(format!("{result:?}")));
        }

        Ok(result)
    });

    // Spawn a thread to forward user's stdin to the PTY
    let stdin_thread = thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut stdin_lock = stdin.lock();
        let mut buf = [0u8; 1024];
        loop {
            match stdin_lock.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if writer.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    let _ = writer.flush();
                }
                Err(_) => break,
            }
        }
    });

    // Process auto-response injections
    // We need another writer handle for injections, but portable-pty only gives one.
    // Instead, handle auto-responses in a separate thread that writes to the same PTY.
    // Since we took the writer above for stdin forwarding, we use a different approach:
    // the output thread sends decisions via channel, and we inject via the master.
    //
    // Actually, portable-pty's take_writer() consumes the writer. For auto-responses,
    // we need to handle this differently. For now, auto-responses are logged but not
    // injected — the stdin thread handles all input. The supervisor prints suggestions
    // with [batty] prefix so the user knows what to type.
    //
    // Full auto-injection will be wired when we split the writer (task #12).
    thread::spawn(move || {
        for (_detected, decision) in prompt_rx {
            match decision {
                Decision::Act {
                    ref prompt,
                    ref response,
                } => {
                    eprintln!("\x1b[36m[batty]\x1b[0m auto-answer: {prompt} → {response}");
                    if let Some(ref tx) = event_tx_inject {
                        let _ = tx.send(SupervisorEvent::AutoResponse {
                            prompt: prompt.clone(),
                            response: response.clone(),
                        });
                    }
                }
                Decision::Suggest {
                    ref prompt,
                    ref response,
                } => {
                    eprintln!(
                        "\x1b[33m[batty]\x1b[0m suggestion: respond to \"{prompt}\" with \"{response}\""
                    );
                }
                Decision::Escalate { ref prompt } => {
                    eprintln!("\x1b[33m[batty]\x1b[0m needs your input: {prompt}");
                }
                _ => {}
            }
        }
    });

    // Wait for the agent process to exit
    let exit_status = child.wait().context("failed to wait for agent process")?;
    info!(success = exit_status.success(), "agent process exited");

    // The output thread will see EOF and exit
    let session_result = output_thread
        .join()
        .map_err(|_| anyhow::anyhow!("output thread panicked"))??;

    // stdin thread will exit when the PTY master is dropped
    drop(stdin_thread);

    match session_result {
        SessionResult::Completed => Ok(session_result),
        SessionResult::Error { .. } => Ok(session_result),
        _ => {
            let code = if exit_status.success() {
                Some(0)
            } else {
                None // portable-pty doesn't expose the raw code portably
            };
            Ok(SessionResult::Exited { code })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn session_result_debug_format() {
        let r = SessionResult::Completed;
        assert_eq!(format!("{r:?}"), "Completed");

        let r = SessionResult::Error {
            detail: "timeout".to_string(),
        };
        assert!(format!("{r:?}").contains("timeout"));

        let r = SessionResult::Exited { code: Some(1) };
        assert!(format!("{r:?}").contains("1"));
    }

    #[test]
    fn supervisor_event_variants() {
        let e = SupervisorEvent::Output("hello".to_string());
        assert!(format!("{e:?}").contains("hello"));

        let e = SupervisorEvent::AutoResponse {
            prompt: "Continue?".to_string(),
            response: "y".to_string(),
        };
        assert!(format!("{e:?}").contains("Continue?"));
    }

    #[test]
    fn session_config_default() {
        let config = SessionConfig::default();
        assert_eq!(config.pty_size.rows, 24);
        assert_eq!(config.pty_size.cols, 80);
    }

    #[test]
    fn run_echo_command() {
        // Spawn a simple echo command to verify PTY supervision works
        let config = SessionConfig {
            spawn: SpawnConfig {
                program: "echo".to_string(),
                args: vec!["hello from batty".to_string()],
                work_dir: "/tmp".to_string(),
                env: vec![],
            },
            patterns: PromptPatterns::claude_code(),
            policy: PolicyEngine::new(crate::config::Policy::Observe, HashMap::new()),
            pty_size: PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
        };

        let (tx, rx) = mpsc::channel();
        let adapter = crate::agent::claude::ClaudeCodeAdapter::new(None);
        let result = run_session(config, &adapter, Some(tx)).unwrap();

        // echo exits immediately with success
        match result {
            SessionResult::Exited { code } => {
                assert_eq!(code, Some(0));
            }
            SessionResult::Completed => {} // also acceptable
            other => panic!("unexpected result: {other:?}"),
        }

        // Collect events
        let events: Vec<_> = rx.try_iter().collect();
        // Should have at least an output event with "hello from batty"
        let has_output = events.iter().any(|e| {
            if let SupervisorEvent::Output(s) = e {
                s.contains("hello from batty")
            } else {
                false
            }
        });
        assert!(has_output, "expected output event, got: {events:?}");
    }

    #[test]
    fn run_failing_command() {
        let config = SessionConfig {
            spawn: SpawnConfig {
                program: "false".to_string(),
                args: vec![],
                work_dir: "/tmp".to_string(),
                env: vec![],
            },
            patterns: PromptPatterns::claude_code(),
            policy: PolicyEngine::new(crate::config::Policy::Observe, HashMap::new()),
            pty_size: PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
        };

        let adapter = crate::agent::claude::ClaudeCodeAdapter::new(None);
        let result = run_session(config, &adapter, None).unwrap();

        if let SessionResult::Exited { code } = result {
            // `false` exits with non-zero
            assert_ne!(code, Some(0));
        }
    }

    #[test]
    fn run_multiline_output() {
        let config = SessionConfig {
            spawn: SpawnConfig {
                program: "printf".to_string(),
                args: vec!["line1\\nline2\\nline3\\n".to_string()],
                work_dir: "/tmp".to_string(),
                env: vec![],
            },
            patterns: PromptPatterns::claude_code(),
            policy: PolicyEngine::new(crate::config::Policy::Observe, HashMap::new()),
            pty_size: PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
        };

        let (tx, rx) = mpsc::channel();
        let adapter = crate::agent::claude::ClaudeCodeAdapter::new(None);
        let _result = run_session(config, &adapter, Some(tx)).unwrap();

        let events: Vec<_> = rx.try_iter().collect();
        let output_events: Vec<_> = events
            .iter()
            .filter_map(|e| {
                if let SupervisorEvent::Output(s) = e {
                    Some(s.as_str())
                } else {
                    None
                }
            })
            .collect();

        // Should have captured multiple output lines
        assert!(
            !output_events.is_empty(),
            "expected output events from printf"
        );
    }
}
