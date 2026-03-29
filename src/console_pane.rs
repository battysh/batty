use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{Local, TimeZone};

const TICK: Duration = Duration::from_millis(120);
const MAX_LOG_LINES: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneMode {
    Log,
    Screen,
    Compose,
}

pub fn run(
    project_root: &Path,
    member: &str,
    events_log_path: &Path,
    pty_log_path: &Path,
) -> Result<()> {
    let _raw = RawTerminal::new()?;
    let mut stdout = io::stdout().lock();
    let stdout_fd = stdout.as_raw_fd();
    set_nonblocking(stdout_fd, false)?;
    let mut stdin = io::stdin().lock();
    set_nonblocking(stdin.as_raw_fd(), true)?;

    let mut state = PaneState::new(
        project_root.to_path_buf(),
        member.to_string(),
        events_log_path.to_path_buf(),
        pty_log_path.to_path_buf(),
    );
    state.redraw(&mut stdout, stdout_fd)?;

    loop {
        let mut buf = [0u8; 64];
        match stdin.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => {
                for byte in &buf[..n] {
                    if state.handle_key(*byte, &mut stdout, stdout_fd)? {
                        return Ok(());
                    }
                }
            }
            Err(error) if is_transient_io_error(&error) => {}
            Err(error) => return Err(error).context("failed to read pane input"),
        }

        if state.tick_due() {
            state.refresh(&mut stdout, stdout_fd)?;
        }

        std::thread::sleep(TICK);
    }
}

fn is_transient_io_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted | io::ErrorKind::TimedOut
    ) || error.raw_os_error() == Some(libc::EAGAIN)
}

struct PaneState {
    project_root: PathBuf,
    member: String,
    events_log_path: PathBuf,
    pty_log_path: PathBuf,
    mode: PaneMode,
    compose: String,
    last_screen_mtime: Option<std::time::SystemTime>,
    last_log_mtime: Option<std::time::SystemTime>,
    last_tick: Instant,
}

impl PaneState {
    fn new(
        project_root: PathBuf,
        member: String,
        events_log_path: PathBuf,
        pty_log_path: PathBuf,
    ) -> Self {
        Self {
            project_root,
            member,
            events_log_path,
            pty_log_path,
            mode: PaneMode::Log,
            compose: String::new(),
            last_screen_mtime: None,
            last_log_mtime: None,
            last_tick: Instant::now(),
        }
    }

    fn tick_due(&mut self) -> bool {
        if self.last_tick.elapsed() >= TICK {
            self.last_tick = Instant::now();
            true
        } else {
            false
        }
    }

    fn handle_key(&mut self, byte: u8, stdout: &mut impl Write, stdout_fd: i32) -> Result<bool> {
        match self.mode {
            PaneMode::Compose => self.handle_compose_key(byte, stdout, stdout_fd),
            PaneMode::Log | PaneMode::Screen => match byte {
                b'l' | b'L' => {
                    self.mode = PaneMode::Log;
                    self.redraw(stdout, stdout_fd)?;
                    Ok(false)
                }
                b's' | b'S' => {
                    self.mode = PaneMode::Screen;
                    self.redraw(stdout, stdout_fd)?;
                    Ok(false)
                }
                b'm' | b'M' => {
                    self.mode = PaneMode::Compose;
                    self.compose.clear();
                    self.redraw(stdout, stdout_fd)?;
                    Ok(false)
                }
                3 | b'q' | b'Q' => Ok(true),
                _ => Ok(false),
            },
        }
    }

    fn handle_compose_key(
        &mut self,
        byte: u8,
        stdout: &mut impl Write,
        stdout_fd: i32,
    ) -> Result<bool> {
        match byte {
            27 => {
                self.mode = PaneMode::Log;
                self.compose.clear();
                self.redraw(stdout, stdout_fd)?;
            }
            b'\r' | b'\n' => {
                let message = self.compose.trim().to_string();
                self.mode = PaneMode::Log;
                self.compose.clear();
                self.redraw(stdout, stdout_fd)?;
                if !message.is_empty() {
                    self.send_message(&message, stdout)?;
                }
            }
            127 | 8 => {
                self.compose.pop();
                self.redraw(stdout, stdout_fd)?;
            }
            byte if (32..=126).contains(&byte) => {
                self.compose.push(byte as char);
                self.redraw(stdout, stdout_fd)?;
            }
            _ => {}
        }
        Ok(false)
    }

    fn refresh(&mut self, stdout: &mut impl Write, stdout_fd: i32) -> Result<()> {
        match self.mode {
            PaneMode::Log => self.refresh_log(stdout),
            PaneMode::Screen => self.refresh_screen(stdout, stdout_fd),
            PaneMode::Compose => Ok(()),
        }
    }

    fn redraw(&mut self, stdout: &mut impl Write, stdout_fd: i32) -> Result<()> {
        match self.mode {
            PaneMode::Log => self.render_log(stdout),
            PaneMode::Screen => self.render_screen(stdout, stdout_fd),
            PaneMode::Compose => self.render_compose(stdout),
        }
    }

    fn refresh_log(&mut self, stdout: &mut impl Write) -> Result<()> {
        let modified = file_modified(&self.events_log_path);
        if modified == self.last_log_mtime {
            return Ok(());
        }
        self.render_log(stdout)
    }

    fn refresh_screen(&mut self, stdout: &mut impl Write, stdout_fd: i32) -> Result<()> {
        let modified = file_modified(&self.pty_log_path);
        if modified == self.last_screen_mtime {
            return Ok(());
        }
        self.render_screen(stdout, stdout_fd)
    }

    fn render_log(&mut self, stdout: &mut impl Write) -> Result<()> {
        let content =
            format_event_log_for_display(&read_tail_lines(&self.events_log_path, MAX_LOG_LINES)?);
        self.last_log_mtime = file_modified(&self.events_log_path);
        clear_screen(stdout)?;
        write_banner(stdout, self.mode, &self.member)?;
        write_crlf(stdout, &content)?;
        flush_retry(stdout)?;
        Ok(())
    }

    fn render_screen(&mut self, stdout: &mut impl Write, stdout_fd: i32) -> Result<()> {
        let (rows, cols) = terminal_size(stdout_fd).unwrap_or((24, 80));
        let content = render_pty_log_screen(&self.pty_log_path, rows.max(1), cols.max(1))?;
        self.last_screen_mtime = file_modified(&self.pty_log_path);
        clear_screen(stdout)?;
        write_crlf(stdout, &content)?;
        flush_retry(stdout)?;
        Ok(())
    }

    fn render_compose(&mut self, stdout: &mut impl Write) -> Result<()> {
        clear_screen(stdout)?;
        write_retry(
            stdout,
            format!(
                "\rmessage to {}  [Enter send, Esc cancel]\r\n\r\n\rmessage> {}",
                self.member, self.compose
            )
            .as_bytes(),
        )?;
        flush_retry(stdout)?;
        Ok(())
    }

    fn send_message(&self, message: &str, stdout: &mut impl Write) -> Result<()> {
        let exe = std::env::current_exe().context("failed to resolve batty binary")?;
        let sender = supervising_sender(&self.project_root, &self.member);
        let output = Command::new(exe)
            .current_dir(&self.project_root)
            .arg("send")
            .arg("--from")
            .arg(&sender)
            .arg(&self.member)
            .arg(message)
            .output()
            .with_context(|| format!("failed to send message to {}", self.member))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            write_retry(
                stdout,
                format!("\r\nsend failed: {}\r\n", stderr.trim()).as_bytes(),
            )?;
            flush_retry(stdout)?;
        }
        Ok(())
    }
}

fn supervising_sender(project_root: &Path, member: &str) -> String {
    let config_path = project_root
        .join(".batty")
        .join("team_config")
        .join("team.yaml");
    let Ok(team_config) = crate::team::config::TeamConfig::load(&config_path) else {
        return "human".to_string();
    };
    let Ok(members) = crate::team::hierarchy::resolve_hierarchy(&team_config) else {
        return "human".to_string();
    };
    members
        .iter()
        .find(|candidate| candidate.name == member)
        .and_then(|candidate| candidate.reports_to.clone())
        .unwrap_or_else(|| "human".to_string())
}

fn render_pty_log_screen(path: &Path, rows: u16, cols: u16) -> Result<String> {
    let bytes = fs::read(path).unwrap_or_default();
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(&bytes);
    Ok(trim_screen(parser.screen().contents()))
}

fn trim_screen(content: String) -> String {
    content
        .lines()
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

fn read_tail_lines(path: &Path, max_lines: usize) -> Result<String> {
    let content = fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    Ok(lines[start..].join("\n"))
}

fn format_event_log_for_display(content: &str) -> String {
    content
        .lines()
        .map(format_event_log_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_event_log_line(line: &str) -> String {
    let Some(rest) = line.strip_prefix('[') else {
        return line.to_string();
    };
    let Some((ts, remainder)) = rest.split_once(']') else {
        return line.to_string();
    };
    let Ok(epoch) = ts.parse::<i64>() else {
        return line.to_string();
    };
    let Some(dt) = Local.timestamp_opt(epoch, 0).single() else {
        return line.to_string();
    };
    format!("[{}]{}", dt.format("%Y-%m-%d %H:%M:%S"), remainder)
}

fn file_modified(path: &Path) -> Option<std::time::SystemTime> {
    fs::metadata(path).and_then(|meta| meta.modified()).ok()
}

fn write_banner(stdout: &mut impl Write, mode: PaneMode, member: &str) -> Result<()> {
    let mode_name = match mode {
        PaneMode::Log => "logs",
        PaneMode::Screen => "screen",
        PaneMode::Compose => "message",
    };
    write_retry(
        stdout,
        format!("\r[{member}] {mode_name}  [l logs] [s screen] [m message]\r\n\r\n").as_bytes(),
    )?;
    Ok(())
}

fn write_crlf(stdout: &mut impl Write, text: &str) -> Result<()> {
    for line in text.lines() {
        write_retry(stdout, format!("\r{line}\r\n").as_bytes())?;
    }
    Ok(())
}

fn clear_screen(stdout: &mut impl Write) -> Result<()> {
    write_retry(stdout, b"\x1b[2J\x1b[H")?;
    Ok(())
}

fn write_retry(stdout: &mut impl Write, bytes: &[u8]) -> Result<()> {
    let mut written = 0;
    while written < bytes.len() {
        match stdout.write(&bytes[written..]) {
            Ok(0) => break,
            Ok(n) => written += n,
            Err(error) if is_transient_io_error(&error) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error).context("failed to write pane output"),
        }
    }
    Ok(())
}

fn flush_retry(stdout: &mut impl Write) -> Result<()> {
    loop {
        match stdout.flush() {
            Ok(()) => return Ok(()),
            Err(error) if is_transient_io_error(&error) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error).context("failed to flush pane output"),
        }
    }
}

fn terminal_size(fd: i32) -> Option<(u16, u16)> {
    let mut winsize = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // Safety: ioctl with TIOCGWINSZ reads into a valid winsize pointer.
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut winsize) };
    if rc == 0 && winsize.ws_row > 0 && winsize.ws_col > 0 {
        Some((winsize.ws_row, winsize.ws_col))
    } else {
        None
    }
}

fn set_nonblocking(fd: i32, enabled: bool) -> Result<()> {
    // Safety: fcntl on a valid file descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error()).context("fcntl(F_GETFL) failed");
    }
    let new_flags = if enabled {
        flags | libc::O_NONBLOCK
    } else {
        flags & !libc::O_NONBLOCK
    };
    // Safety: fcntl on a valid file descriptor.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, new_flags) } < 0 {
        return Err(io::Error::last_os_error()).context("fcntl(F_SETFL) failed");
    }
    Ok(())
}

struct RawTerminal {
    fd: i32,
    original: libc::termios,
}

impl RawTerminal {
    fn new() -> Result<Self> {
        let fd = io::stdin().as_raw_fd();
        let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
        // Safety: tcgetattr/tcsetattr operate on stdin fd and valid termios pointers.
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return Err(io::Error::last_os_error()).context("tcgetattr failed");
        }
        let mut raw = original;
        raw.c_iflag &= !(libc::BRKINT | libc::ICRNL | libc::INPCK | libc::ISTRIP | libc::IXON);
        raw.c_oflag &= !libc::OPOST;
        raw.c_cflag |= libc::CS8;
        raw.c_lflag &= !(libc::ECHO | libc::ICANON | libc::IEXTEN | libc::ISIG);
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 1;
        if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &raw) } != 0 {
            return Err(io::Error::last_os_error()).context("tcsetattr failed");
        }
        Ok(Self { fd, original })
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        // Safety: restoring previously captured termios to same fd.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSAFLUSH, &self.original);
        }
        let _ = set_nonblocking(self.fd, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_screen_removes_trailing_spaces() {
        let text = "abc   \nxyz  ".to_string();
        assert_eq!(trim_screen(text), "abc\nxyz");
    }

    #[test]
    fn read_tail_lines_limits_to_requested_count() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.log");
        fs::write(&path, "1\n2\n3\n4\n").unwrap();
        assert_eq!(read_tail_lines(&path, 2).unwrap(), "3\n4");
    }

    #[test]
    fn format_event_log_line_rewrites_unix_timestamp() {
        let formatted = format_event_log_line("[0] <- ready");
        assert!(formatted.starts_with("["));
        assert!(formatted.ends_with(" <- ready"));
        assert_ne!(formatted, "[0] <- ready");
        assert_eq!(formatted.len(), "[1970-01-01 00:00:00] <- ready".len());
    }

    #[test]
    fn format_event_log_line_leaves_non_timestamp_lines_alone() {
        assert_eq!(format_event_log_line("plain line"), "plain line");
        assert_eq!(format_event_log_line("[abc] nope"), "[abc] nope");
    }

    #[test]
    fn render_pty_log_screen_uses_requested_size() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pty.log");
        fs::write(&path, "0123456789abcdef").unwrap();
        let rendered = render_pty_log_screen(&path, 2, 8).unwrap();
        assert!(rendered.contains("01234567"));
    }

    #[test]
    fn transient_io_error_recognizes_eagain() {
        let error = io::Error::from_raw_os_error(libc::EAGAIN);
        assert!(is_transient_io_error(&error));
    }

    #[test]
    fn supervising_sender_prefers_parent_role() {
        let tmp = tempfile::tempdir().unwrap();
        let batty = tmp.path().join(".batty").join("team_config");
        fs::create_dir_all(&batty).unwrap();
        fs::write(
            batty.join("team.yaml"),
            r#"
name: test
roles:
  - name: architect
    role_type: architect
    agent: claude
    instances: 1
    prompt: architect.md
  - name: manager
    role_type: manager
    agent: claude
    instances: 1
    prompt: manager.md
    talks_to: [architect, engineer]
  - name: engineer
    role_type: engineer
    agent: codex
    instances: 1
    prompt: engineer.md
    talks_to: [manager]
"#,
        )
        .unwrap();

        assert_eq!(supervising_sender(tmp.path(), "eng-1-1"), "manager");
        assert_eq!(supervising_sender(tmp.path(), "manager"), "architect");
        assert_eq!(supervising_sender(tmp.path(), "architect"), "human");
    }
}
