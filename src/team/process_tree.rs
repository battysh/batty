//! Process ancestry helpers for startup preflight checks.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessInfo {
    pub(crate) pid: u32,
    pub(crate) ppid: u32,
    pub(crate) command: String,
}

impl ProcessInfo {
    fn new(pid: u32, ppid: u32, command: impl Into<String>) -> Self {
        Self {
            pid,
            ppid,
            command: command.into(),
        }
    }
}

pub(crate) fn concurrent_batty_process(current_pid: u32) -> Result<Option<ProcessInfo>> {
    let processes = read_process_table().context("failed to inspect process table")?;
    Ok(find_concurrent_batty_process(current_pid, &processes).cloned())
}

fn find_concurrent_batty_process(
    current_pid: u32,
    processes: &[ProcessInfo],
) -> Option<&ProcessInfo> {
    let ancestors = ancestor_pids(current_pid, processes);
    processes
        .iter()
        .filter(|process| process.pid != current_pid)
        .filter(|process| !ancestors.contains(&process.pid))
        .filter(|process| is_batty_command(&process.command))
        .min_by_key(|process| process.pid)
}

fn ancestor_pids(current_pid: u32, processes: &[ProcessInfo]) -> HashSet<u32> {
    let by_pid: HashMap<u32, u32> = processes
        .iter()
        .map(|process| (process.pid, process.ppid))
        .collect();
    let mut ancestors = HashSet::new();
    let mut next = by_pid.get(&current_pid).copied().unwrap_or_default();

    while next != 0 && ancestors.insert(next) {
        next = by_pid.get(&next).copied().unwrap_or_default();
    }

    ancestors
}

fn is_batty_command(command: &str) -> bool {
    let command = command.trim();
    if command.is_empty() {
        return false;
    }

    let first_arg = command.split_whitespace().next().unwrap_or(command);
    Path::new(first_arg)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "batty" || name.starts_with("batty-"))
}

fn read_process_table() -> Result<Vec<ProcessInfo>> {
    #[cfg(target_os = "linux")]
    {
        match read_process_table_from_proc() {
            Ok(processes) if !processes.is_empty() => return Ok(processes),
            Ok(_) => {}
            Err(_) => {}
        }
    }

    read_process_table_from_ps()
}

#[cfg(target_os = "linux")]
fn read_process_table_from_proc() -> std::io::Result<Vec<ProcessInfo>> {
    let mut processes = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(pid_text) = file_name.to_str() else {
            continue;
        };
        if !pid_text.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }

        let stat_path = entry.path().join("stat");
        let Ok(stat) = std::fs::read_to_string(stat_path) else {
            continue;
        };
        let Some((pid, ppid, stat_command)) = parse_proc_stat(&stat) else {
            continue;
        };
        let command = read_proc_cmdline(pid).unwrap_or(stat_command);
        processes.push(ProcessInfo::new(pid, ppid, command));
    }
    Ok(processes)
}

#[cfg(target_os = "linux")]
fn read_proc_cmdline(pid: u32) -> Option<String> {
    let path = format!("/proc/{pid}/cmdline");
    let bytes = std::fs::read(path).ok()?;
    let rendered = bytes
        .split(|byte| *byte == 0)
        .filter_map(|part| std::str::from_utf8(part).ok())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if rendered.trim().is_empty() {
        None
    } else {
        Some(rendered)
    }
}

#[cfg(target_os = "linux")]
fn parse_proc_stat(stat: &str) -> Option<(u32, u32, String)> {
    let open = stat.find(" (")?;
    let close = stat.rfind(") ")?;
    if close <= open + 2 {
        return None;
    }

    let pid = stat[..open].trim().parse().ok()?;
    let command = stat[open + 2..close].to_string();
    let mut fields = stat[close + 2..].split_whitespace();
    let _state = fields.next()?;
    let ppid = fields.next()?.parse().ok()?;

    Some((pid, ppid, command))
}

fn read_process_table_from_ps() -> Result<Vec<ProcessInfo>> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,ppid=,comm="])
        .output()
        .context("failed to run `ps -axo pid=,ppid=,comm=`")?;
    if !output.status.success() {
        anyhow::bail!("`ps -axo pid=,ppid=,comm=` exited with {}", output.status);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let processes = stdout.lines().filter_map(parse_ps_line).collect::<Vec<_>>();
    Ok(processes)
}

fn parse_ps_line(line: &str) -> Option<ProcessInfo> {
    let mut parts = line.split_whitespace();
    let pid = parts.next()?.parse().ok()?;
    let ppid = parts.next()?.parse().ok()?;
    let command = parts.collect::<Vec<_>>().join(" ");
    if command.is_empty() {
        None
    } else {
        Some(ProcessInfo::new(pid, ppid, command))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, ppid: u32, command: &str) -> ProcessInfo {
        ProcessInfo::new(pid, ppid, command)
    }

    #[test]
    fn direct_parent_batty_wrapper_is_not_concurrent() {
        let processes = vec![
            proc(10, 0, "launchd"),
            proc(20, 10, "batty"),
            proc(30, 20, "batty"),
        ];

        assert_eq!(find_concurrent_batty_process(30, &processes), None);
    }

    #[test]
    fn ancestor_batty_wrapper_is_not_concurrent() {
        let processes = vec![
            proc(10, 0, "launchd"),
            proc(20, 10, "/usr/local/bin/batty"),
            proc(30, 20, "zsh"),
            proc(40, 30, "/tmp/wrapper.sh"),
            proc(50, 40, "batty"),
        ];

        assert_eq!(find_concurrent_batty_process(50, &processes), None);
    }

    #[test]
    fn unrelated_batty_process_is_concurrent() {
        let processes = vec![
            proc(10, 0, "launchd"),
            proc(20, 10, "batty"),
            proc(30, 10, "zsh"),
            proc(40, 30, "batty"),
        ];

        assert_eq!(
            find_concurrent_batty_process(40, &processes),
            Some(&proc(20, 10, "batty"))
        );
    }

    #[test]
    fn unrelated_non_batty_process_is_ignored() {
        let processes = vec![
            proc(10, 0, "launchd"),
            proc(20, 10, "zsh"),
            proc(30, 10, "bash"),
            proc(40, 30, "batty"),
        ];

        assert_eq!(find_concurrent_batty_process(40, &processes), None);
    }

    #[test]
    fn ps_line_parser_accepts_paths_with_spaces_after_ppid() {
        assert_eq!(
            parse_ps_line(" 123  45 /usr/local/bin/batty start"),
            Some(proc(123, 45, "/usr/local/bin/batty start"))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_stat_parser_handles_commands_with_spaces() {
        assert_eq!(
            parse_proc_stat("123 (batty wrapper) S 45 1 2 3"),
            Some((123, 45, "batty wrapper".to_string()))
        );
    }
}
