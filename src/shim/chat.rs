//! Chat frontend: spawns a shim subprocess, sends user messages, displays responses.
//!
//! This is a simple TTY application that demonstrates the shim protocol.
//! Under the hood it forks a shim subprocess, communicates via socketpair,
//! and presents a readline-style prompt.

use std::io::{self, BufRead, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use super::classifier::AgentType;
use super::protocol::{self, Channel, Event};

// ---------------------------------------------------------------------------
// Default command per agent type
// ---------------------------------------------------------------------------

/// Returns the default shell command used to launch each agent type.
pub fn default_cmd(agent_type: AgentType) -> &'static str {
    match agent_type {
        AgentType::Claude => "claude --dangerously-skip-permissions",
        AgentType::Codex => "codex",
        AgentType::Kiro => "kiro",
        AgentType::Generic => "bash",
    }
}

// ---------------------------------------------------------------------------
// Special command parsing
// ---------------------------------------------------------------------------

/// Recognized special commands typed at the `you> ` prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecialCommand {
    Quit,
    Screen,
    State,
    Ping,
}

/// Try to parse a line of user input as a special command.
/// Returns `None` if the input is a regular message.
pub fn parse_special(input: &str) -> Option<SpecialCommand> {
    match input {
        ":quit" | ":q" => Some(SpecialCommand::Quit),
        ":screen" => Some(SpecialCommand::Screen),
        ":state" => Some(SpecialCommand::State),
        ":ping" => Some(SpecialCommand::Ping),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Chat entry point
// ---------------------------------------------------------------------------

pub fn run(agent_type: AgentType, cmd: &str, cwd: &Path) -> Result<()> {
    // -- Create socketpair --
    let (parent_sock, child_sock) = protocol::socketpair().context("socketpair failed")?;

    // -- Find our own binary path (for spawning shim subprocess) --
    let self_exe = std::env::current_exe().context("cannot determine own executable path")?;

    // -- Spawn shim as child process, passing child_sock as fd 3 --
    let child_fd = child_sock.as_raw_fd();
    let child_fd_val = child_fd; // copy the raw fd value
    let agent_type_str = agent_type.to_string();
    let cmd_owned = cmd.to_string();
    let cwd_str = cwd.display().to_string();

    let mut child = unsafe {
        Command::new(&self_exe)
            .args([
                "shim",
                "--id",
                "chat-agent",
                "--agent-type",
                &agent_type_str,
                "--cmd",
                &cmd_owned,
                "--cwd",
                &cwd_str,
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::inherit())
            .pre_exec(move || {
                // Dup the socketpair fd to fd 3
                if child_fd_val != 3 {
                    let ret = libc::dup2(child_fd_val, 3);
                    if ret < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            })
            .spawn()
            .context("failed to spawn shim process")?
    };

    // Close child's end in parent
    drop(child_sock);

    // -- Set up channel --
    let mut send_ch = Channel::new(parent_sock);
    let mut recv_ch = send_ch.try_clone().context("failed to clone channel")?;

    eprintln!(
        "[chat] shim spawned (pid {}), waiting for agent to become ready...",
        child.id()
    );

    // -- Wait for Ready event --
    loop {
        match recv_ch.recv::<Event>()? {
            Some(Event::Ready) => {
                eprintln!("[chat] agent is ready. Type a message and press Enter.");
                eprintln!(
                    "[chat] Type :quit to exit, :screen to capture screen, :state to query state.\n"
                );
                break;
            }
            Some(Event::StateChanged { from, to, .. }) => {
                eprintln!("[chat] state: {} \u{2192} {}", from, to);
            }
            Some(Event::Error { reason, .. }) => {
                eprintln!("[chat] error during startup: {reason}");
                child.kill().ok();
                return Ok(());
            }
            Some(Event::Died {
                exit_code,
                last_lines,
            }) => {
                eprintln!(
                    "[chat] agent died before becoming ready (exit={:?})\n{}",
                    exit_code, last_lines
                );
                return Ok(());
            }
            Some(other) => {
                eprintln!("[chat] unexpected event during startup: {:?}", other);
            }
            None => {
                eprintln!("[chat] shim disconnected before ready");
                return Ok(());
            }
        }
    }

    // -- Main chat loop --
    let (event_tx, event_rx) = std::sync::mpsc::channel::<Event>();

    // Background thread: read events from shim
    let recv_handle = std::thread::spawn(move || {
        loop {
            match recv_ch.recv::<Event>() {
                Ok(Some(evt)) => {
                    if event_tx.send(evt).is_err() {
                        break; // main thread dropped receiver
                    }
                }
                Ok(None) => break, // shim closed
                Err(_) => break,
            }
        }
    });

    let stdin = io::stdin();
    let mut stdout = io::stdout();

    loop {
        print!("you> ");
        stdout.flush()?;

        let mut line = String::new();
        let n = stdin.lock().read_line(&mut line)?;
        if n == 0 {
            eprintln!("\n[chat] EOF, shutting down...");
            send_ch.send(&protocol::Command::Shutdown { timeout_secs: 5 })?;
            break;
        }

        let input = line.trim();
        if input.is_empty() {
            continue;
        }

        // -- Special commands --
        match parse_special(input) {
            Some(SpecialCommand::Quit) => {
                send_ch.send(&protocol::Command::Shutdown { timeout_secs: 5 })?;
                break;
            }
            Some(SpecialCommand::Screen) => {
                send_ch.send(&protocol::Command::CaptureScreen {
                    last_n_lines: Some(30),
                })?;
                if let Ok(Event::ScreenCapture {
                    content,
                    cursor_row,
                    cursor_col,
                }) = event_rx.recv()
                {
                    println!(
                        "--- screen capture (cursor at {},{}) ---",
                        cursor_row, cursor_col
                    );
                    println!("{content}");
                    println!("--- end screen capture ---");
                }
                continue;
            }
            Some(SpecialCommand::State) => {
                send_ch.send(&protocol::Command::GetState)?;
                if let Ok(Event::State { state, since_secs }) = event_rx.recv() {
                    println!("[state: {state}, since: {since_secs}s ago]");
                }
                continue;
            }
            Some(SpecialCommand::Ping) => {
                send_ch.send(&protocol::Command::Ping)?;
                if let Ok(Event::Pong) = event_rx.recv() {
                    println!("[pong]");
                }
                continue;
            }
            None => {}
        }

        // -- Send message to agent --
        send_ch.send(&protocol::Command::SendMessage {
            from: "user".into(),
            body: input.to_string(),
            message_id: None,
        })?;

        // Wait for completion (or other terminal events)
        let mut got_completion = false;
        while !got_completion {
            match event_rx.recv() {
                Ok(Event::Completion {
                    response,
                    last_lines,
                    ..
                }) => {
                    if !response.is_empty() {
                        println!("\n{response}");
                    } else if !last_lines.is_empty() {
                        println!("\n{last_lines}");
                    } else {
                        println!("\n[agent completed with no visible output]");
                    }
                    got_completion = true;
                }
                Ok(Event::StateChanged { from, to, .. }) => {
                    eprint!("[{from} \u{2192} {to}] ");
                    io::stderr().flush().ok();
                }
                Ok(Event::Died {
                    exit_code,
                    last_lines,
                }) => {
                    eprintln!("\n[chat] agent died (exit={exit_code:?})");
                    if !last_lines.is_empty() {
                        println!("{last_lines}");
                    }
                    return Ok(());
                }
                Ok(Event::ContextExhausted { message, .. }) => {
                    eprintln!("\n[chat] context exhausted: {message}");
                    return Ok(());
                }
                Ok(Event::Error { command, reason }) => {
                    eprintln!("\n[chat] error ({command}): {reason}");
                    got_completion = true; // don't hang
                }
                Ok(other) => {
                    eprintln!("[chat] event: {other:?}");
                }
                Err(_) => {
                    eprintln!("\n[chat] channel closed");
                    return Ok(());
                }
            }
        }
    }

    // Cleanup
    child.wait().ok();
    recv_handle.join().ok();
    eprintln!("[chat] done.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_cmd_claude() {
        assert_eq!(
            default_cmd(AgentType::Claude),
            "claude --dangerously-skip-permissions"
        );
    }

    #[test]
    fn default_cmd_codex() {
        assert_eq!(default_cmd(AgentType::Codex), "codex");
    }

    #[test]
    fn default_cmd_kiro() {
        assert_eq!(default_cmd(AgentType::Kiro), "kiro");
    }

    #[test]
    fn default_cmd_generic() {
        assert_eq!(default_cmd(AgentType::Generic), "bash");
    }

    #[test]
    fn parse_special_quit() {
        assert_eq!(parse_special(":quit"), Some(SpecialCommand::Quit));
        assert_eq!(parse_special(":q"), Some(SpecialCommand::Quit));
    }

    #[test]
    fn parse_special_screen() {
        assert_eq!(parse_special(":screen"), Some(SpecialCommand::Screen));
    }

    #[test]
    fn parse_special_state() {
        assert_eq!(parse_special(":state"), Some(SpecialCommand::State));
    }

    #[test]
    fn parse_special_ping() {
        assert_eq!(parse_special(":ping"), Some(SpecialCommand::Ping));
    }

    #[test]
    fn parse_special_none() {
        assert_eq!(parse_special("hello world"), None);
        assert_eq!(parse_special(""), None);
        assert_eq!(parse_special(":unknown"), None);
    }
}
