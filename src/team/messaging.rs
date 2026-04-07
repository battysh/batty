//! Message routing, inbox operations, merge, and Telegram setup.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use super::{completion, config, hierarchy, inbox, merge, team_config_path, telegram};

const INBOX_BODY_PREVIEW_CHARS: usize = 140;

/// Resolve a member instance name (e.g. "eng-1-2") to its role definition name
/// (e.g. "engineer"). Returns the name itself if no config is available.
fn resolve_role_name(project_root: &Path, member_name: &str) -> String {
    // "human" is not a member instance — it's the CLI user
    if matches!(member_name, "human" | "daemon") {
        return member_name.to_string();
    }
    let config_path = team_config_path(project_root);
    if let Ok(team_config) = config::TeamConfig::load(&config_path) {
        if let Ok(members) = hierarchy::resolve_hierarchy(&team_config) {
            if let Some(m) = members.iter().find(|m| m.name == member_name) {
                return m.role_name.clone();
            }
        }
    }
    // Fallback: the name might already be a role name
    member_name.to_string()
}

/// Resolve a caller-facing role/member name to a concrete member instance.
///
/// Examples:
/// - exact member names pass through unchanged (`sam-designer-1-1`)
/// - unique role aliases resolve to their single member instance (`sam-designer`)
/// - ambiguous aliases error and require an explicit member name
pub(crate) fn resolve_member_name(project_root: &Path, member_name: &str) -> Result<String> {
    if matches!(member_name, "human" | "daemon") {
        return Ok(member_name.to_string());
    }

    let config_path = team_config_path(project_root);
    if let Ok(team_config) = config::TeamConfig::load(&config_path) {
        if let Ok(members) = hierarchy::resolve_hierarchy(&team_config) {
            if let Some(member) = members.iter().find(|m| m.name == member_name) {
                return Ok(member.name.clone());
            }

            let matches: Vec<String> = members
                .iter()
                .filter(|m| m.role_name == member_name)
                .map(|m| m.name.clone())
                .collect();

            return match matches.len() {
                0 => Ok(member_name.to_string()),
                1 => Ok(matches[0].clone()),
                _ => bail!(
                    "'{member_name}' matches multiple members: {}. Use the explicit member name.",
                    matches.join(", ")
                ),
            };
        }
    }

    Ok(member_name.to_string())
}

/// Send a message to a role via their Maildir inbox.
///
/// The sender is auto-detected from the `@batty_role` tmux pane option
/// (set during layout). Falls back to "human" if not in a batty pane.
/// Enforces communication routing rules from team config.
pub fn send_message(project_root: &Path, role: &str, msg: &str) -> Result<()> {
    send_message_as(project_root, None, role, msg)
}

pub fn send_message_as(
    project_root: &Path,
    from_override: Option<&str>,
    role: &str,
    msg: &str,
) -> Result<()> {
    let from = effective_sender(project_root, from_override);
    let recipient = resolve_member_name(project_root, role)?;

    // Enforce routing: check talks_to rules
    let config_path = team_config_path(project_root);
    if config_path.exists() {
        if let Ok(team_config) = config::TeamConfig::load(&config_path) {
            let from_role = resolve_role_name(project_root, &from);
            let to_role = resolve_role_name(project_root, &recipient);
            if !team_config.can_talk(&from_role, &to_role) {
                bail!(
                    "{from} ({from_role}) is not allowed to message {recipient} ({to_role}). \
                     Check talks_to in team.yaml."
                );
            }
        }
    }

    let root = inbox::inboxes_root(project_root);
    let inbox_msg = inbox::InboxMessage::new_send(&from, &recipient, msg);
    let id = inbox::deliver_to_inbox(&root, &inbox_msg)?;
    if let Err(error) = completion::ingest_completion_message(project_root, msg) {
        warn!(from, to = %recipient, error = %error, "failed to ingest completion packet");
    }
    info!(to = %recipient, id = %id, "message delivered to inbox");
    Ok(())
}

/// Detect who is calling `batty send` by reading the `@batty_role` option
/// from the current tmux pane.
pub(crate) fn detect_sender() -> Option<String> {
    // 1. Check BATTY_MEMBER env var (set by SDK mode shim subprocess)
    if let Ok(member) = std::env::var("BATTY_MEMBER") {
        if !member.is_empty() {
            return Some(member);
        }
    }

    // 2. Fall back to tmux pane role detection (PTY mode)
    let pane_id = std::env::var("TMUX_PANE").ok()?;
    let output = std::process::Command::new("tmux")
        .args(["show-options", "-p", "-t", &pane_id, "-v", "@batty_role"])
        .output()
        .ok()?;
    if output.status.success() {
        let role = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !role.is_empty() { Some(role) } else { None }
    } else {
        None
    }
}

fn effective_sender(project_root: &Path, from_override: Option<&str>) -> String {
    let candidate = from_override
        .map(str::to_string)
        .or_else(detect_sender)
        .unwrap_or_else(|| "human".to_string());

    if sender_belongs_to_project(project_root, &candidate) {
        candidate
    } else {
        "human".to_string()
    }
}

fn sender_belongs_to_project(project_root: &Path, sender: &str) -> bool {
    if matches!(sender, "human" | "daemon") {
        return true;
    }

    let config_path = team_config_path(project_root);
    let Ok(team_config) = config::TeamConfig::load(&config_path) else {
        return false;
    };

    if team_config.roles.iter().any(|role| role.name == sender) {
        return true;
    }

    hierarchy::resolve_hierarchy(&team_config)
        .map(|members| members.iter().any(|member| member.name == sender))
        .unwrap_or(false)
}

/// Assign a task to an engineer via their Maildir inbox.
pub fn assign_task(project_root: &Path, engineer: &str, task: &str) -> Result<String> {
    let from = effective_sender(project_root, None);
    let recipient = resolve_member_name(project_root, engineer)?;

    let config_path = team_config_path(project_root);
    if config_path.exists() {
        if let Ok(team_config) = config::TeamConfig::load(&config_path) {
            let from_role = resolve_role_name(project_root, &from);
            let to_role = resolve_role_name(project_root, &recipient);
            if !team_config.can_talk(&from_role, &to_role) {
                bail!(
                    "{from} ({from_role}) is not allowed to assign {recipient} ({to_role}). \
                     Check talks_to in team.yaml."
                );
            }
        }
    }

    let root = inbox::inboxes_root(project_root);
    let msg = inbox::InboxMessage::new_assign(&from, &recipient, task);
    let id = inbox::deliver_to_inbox(&root, &msg)?;
    info!(from, engineer = %recipient, task, id = %id, "assignment delivered to inbox");
    Ok(id)
}

/// List inbox messages for a member.
pub fn list_inbox(project_root: &Path, member: &str, limit: Option<usize>) -> Result<()> {
    let member = resolve_member_name(project_root, member)?;
    let root = inbox::inboxes_root(project_root);
    let messages = inbox::all_messages(&root, &member)?;
    print!("{}", format_inbox_listing(&member, &messages, limit));
    Ok(())
}

fn format_inbox_listing(
    member: &str,
    messages: &[(inbox::InboxMessage, bool)],
    limit: Option<usize>,
) -> String {
    if messages.is_empty() {
        return format!("No messages for {member}.\n");
    }

    let start = match limit {
        Some(0) => messages.len(),
        Some(n) => messages.len().saturating_sub(n),
        None => 0,
    };
    let shown = &messages[start..];
    let refs = inbox_message_refs(messages);
    let shown_refs = &refs[start..];

    let mut out = String::new();
    if shown.len() < messages.len() {
        out.push_str(&format!(
            "Showing {} of {} messages for {member}. Use `-n <N>` or `--all` to see more.\n",
            shown.len(),
            messages.len()
        ));
    }
    out.push_str(&format!(
        "{:<10} {:<12} {:<12} {:<14} BODY\n",
        "STATUS", "FROM", "TYPE", "REF"
    ));
    out.push_str(&format!("{}\n", "-".repeat(96)));
    for ((msg, delivered), msg_ref) in shown.iter().zip(shown_refs.iter()) {
        let status = if *delivered { "delivered" } else { "pending" };
        let body_short = truncate_chars(&msg.body, INBOX_BODY_PREVIEW_CHARS);
        out.push_str(&format!(
            "{:<10} {:<12} {:<12} {:<14} {}\n",
            status,
            msg.from,
            format!("{:?}", msg.msg_type).to_lowercase(),
            msg_ref,
            body_short,
        ));
    }
    out
}

fn inbox_message_refs(messages: &[(inbox::InboxMessage, bool)]) -> Vec<String> {
    let mut totals = HashMap::new();
    for (msg, _) in messages {
        *totals.entry(msg.timestamp).or_insert(0usize) += 1;
    }

    let mut seen = HashMap::new();
    messages
        .iter()
        .map(|(msg, _)| {
            let ordinal = seen.entry(msg.timestamp).or_insert(0usize);
            *ordinal += 1;
            if totals.get(&msg.timestamp).copied().unwrap_or(0) <= 1 {
                msg.timestamp.to_string()
            } else {
                format!("{}-{}", msg.timestamp, ordinal)
            }
        })
        .collect()
}

fn resolve_inbox_message_indices(
    messages: &[(inbox::InboxMessage, bool)],
    selector: &str,
) -> Vec<usize> {
    let refs = inbox_message_refs(messages);
    messages
        .iter()
        .enumerate()
        .filter_map(|(idx, (msg, _))| {
            if msg.id == selector || msg.id.starts_with(selector) || refs[idx] == selector {
                Some(idx)
            } else {
                None
            }
        })
        .collect()
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    let mut truncated: String = input.chars().take(max_chars).collect();
    truncated.push_str("...");
    truncated
}

/// Read a specific message from a member's inbox by ID, ID prefix, or REF.
pub fn read_message(project_root: &Path, member: &str, id: &str) -> Result<()> {
    let member = resolve_member_name(project_root, member)?;
    let root = inbox::inboxes_root(project_root);
    let messages = inbox::all_messages(&root, &member)?;

    let matching = resolve_inbox_message_indices(&messages, id);

    match matching.len() {
        0 => bail!("no message matching '{id}' in {member}'s inbox"),
        1 => {
            let (msg, delivered) = &messages[matching[0]];
            let status = if *delivered { "delivered" } else { "pending" };
            println!("ID:     {}", msg.id);
            println!("From:   {}", msg.from);
            println!("To:     {}", msg.to);
            println!("Type:   {:?}", msg.msg_type);
            println!("Status: {status}");
            println!("Time:   {}", msg.timestamp);
            println!();
            println!("{}", msg.body);
        }
        n => {
            bail!(
                "'{id}' matches {n} messages — use a longer prefix or the REF column from `batty inbox`"
            );
        }
    }

    Ok(())
}

/// Acknowledge (mark delivered) a message in a member's inbox by ID, prefix, or REF.
pub fn ack_message(project_root: &Path, member: &str, id: &str) -> Result<()> {
    let member = resolve_member_name(project_root, member)?;
    let root = inbox::inboxes_root(project_root);
    let messages = inbox::all_messages(&root, &member)?;
    let matching = resolve_inbox_message_indices(&messages, id);
    let resolved_id = match matching.len() {
        0 => bail!("no message matching '{id}' in {member}'s inbox"),
        1 => messages[matching[0]].0.id.clone(),
        n => bail!(
            "'{id}' matches {n} messages — use a longer prefix or the REF column from `batty inbox`"
        ),
    };
    inbox::mark_delivered(&root, &member, &resolved_id)?;
    info!(member, id = %resolved_id, "message acknowledged");
    Ok(())
}

/// Purge delivered messages from one inbox or all inboxes.
pub fn purge_inbox(
    project_root: &Path,
    member: Option<&str>,
    all_roles: bool,
    before: Option<u64>,
    purge_all: bool,
) -> Result<inbox::InboxPurgeSummary> {
    if !purge_all && before.is_none() {
        bail!("use `--all` or `--before <unix-timestamp>` with `batty inbox purge`");
    }

    let root = inbox::inboxes_root(project_root);
    if all_roles {
        return inbox::purge_delivered_messages_for_all(&root, before, purge_all);
    }

    let member = member.context("member is required unless using `--all-roles`")?;
    let member = resolve_member_name(project_root, member)?;
    let messages = inbox::purge_delivered_messages(&root, &member, before, purge_all)?;
    Ok(inbox::InboxPurgeSummary { roles: 1, messages })
}

/// Merge an engineer's worktree branch.
pub fn merge_worktree(project_root: &Path, engineer: &str) -> Result<()> {
    let engineer = resolve_member_name(project_root, engineer)?;
    match merge::merge_engineer_branch(project_root, &engineer)? {
        merge::MergeOutcome::Success => Ok(()),
        merge::MergeOutcome::RebaseConflict(stderr) => {
            bail!("merge blocked by rebase conflict: {stderr}")
        }
        merge::MergeOutcome::MergeFailure(stderr) => bail!("merge failed: {stderr}"),
    }
}

/// Run the interactive Telegram setup wizard.
pub fn setup_telegram(project_root: &Path) -> Result<()> {
    telegram::setup_telegram(project_root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::{board, inbox, team_config_dir, team_config_path};
    use serial_test::serial;

    struct EnvVarGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvVarGuard {
        fn unset(key: &'static str) -> Self {
            let original = std::env::var(key).ok();
            unsafe {
                std::env::remove_var(key);
            }
            Self { key, original }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.original.as_deref() {
                Some(value) => unsafe {
                    std::env::set_var(self.key, value);
                },
                None => unsafe {
                    std::env::remove_var(self.key);
                },
            }
        }
    }

    fn write_team_config(project_root: &Path, yaml: &str) {
        std::fs::create_dir_all(team_config_dir(project_root)).unwrap();
        std::fs::write(team_config_path(project_root), yaml).unwrap();
    }

    #[test]
    fn send_message_delivers_to_inbox() {
        let tmp = tempfile::tempdir().unwrap();
        let _tmux_pane = EnvVarGuard::unset("TMUX_PANE");
        let _batty_member = EnvVarGuard::unset("BATTY_MEMBER");
        send_message(tmp.path(), "architect", "hello").unwrap();

        let root = inbox::inboxes_root(tmp.path());
        let pending = inbox::pending_messages(&root, "architect").unwrap();
        assert_eq!(pending.len(), 1);
        let expected_from = effective_sender(tmp.path(), None);
        assert_eq!(pending[0].from, expected_from);
        assert_eq!(pending[0].to, "architect");
        assert_eq!(pending[0].body, "hello");
    }

    #[test]
    fn send_message_ingests_completion_packet_into_workflow_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = team_config_dir(tmp.path()).join("board").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let task_path = tasks_dir.join("027-completion-packets.md");
        std::fs::write(
            &task_path,
            "---\nid: 27\ntitle: Completion packets\nstatus: review\npriority: medium\nclaimed_by: human\nclass: standard\n---\n\nTask body.\n",
        )
        .unwrap();

        send_message(
            tmp.path(),
            "architect",
            r#"Done.

## Completion Packet

```json
{"task_id":27,"branch":"eng-1-4/task-27","worktree_path":".batty/worktrees/eng-1-4","commit":"abc1234","changed_paths":["src/team/completion.rs"],"tests_run":true,"tests_passed":true,"artifacts":["docs/workflow.md"],"outcome":"ready_for_review"}
```"#,
        )
        .unwrap();

        let metadata = board::read_workflow_metadata(&task_path).unwrap();
        assert_eq!(metadata.branch.as_deref(), Some("eng-1-4/task-27"));
        assert_eq!(
            metadata.worktree_path.as_deref(),
            Some(".batty/worktrees/eng-1-4")
        );
        assert_eq!(metadata.commit.as_deref(), Some("abc1234"));
        assert_eq!(metadata.tests_run, Some(true));
        assert_eq!(metadata.tests_passed, Some(true));
        assert_eq!(metadata.outcome.as_deref(), Some("ready_for_review"));
        assert!(metadata.review_blockers.is_empty());
    }

    #[test]
    fn send_message_does_not_ingest_failed_test_completion_packet() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks_dir = team_config_dir(tmp.path()).join("board").join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let task_path = tasks_dir.join("027-completion-packets.md");
        std::fs::write(
            &task_path,
            "---\nid: 27\ntitle: Completion packets\nstatus: review\npriority: medium\nclaimed_by: human\nclass: standard\n---\n\nTask body.\n",
        )
        .unwrap();

        send_message(
            tmp.path(),
            "architect",
            r#"Done.

## Completion Packet

```json
{"task_id":27,"branch":"eng-1-4/task-27","worktree_path":".batty/worktrees/eng-1-4","commit":"abc1234","changed_paths":["src/team/completion.rs"],"tests_run":true,"tests_passed":false,"artifacts":[],"outcome":"ready_for_review"}
```"#,
        )
        .unwrap();

        let metadata = board::read_workflow_metadata(&task_path).unwrap();
        assert!(metadata.branch.is_none());
        assert!(metadata.tests_run.is_none());
        assert!(metadata.review_blockers.is_empty());
    }

    #[test]
    fn assign_task_delivers_to_inbox() {
        let tmp = tempfile::tempdir().unwrap();
        let _tmux_pane = EnvVarGuard::unset("TMUX_PANE");
        let _batty_member = EnvVarGuard::unset("BATTY_MEMBER");
        let id = assign_task(tmp.path(), "eng-1-1", "fix bug").unwrap();
        assert!(!id.is_empty());

        let root = inbox::inboxes_root(tmp.path());
        let pending = inbox::pending_messages(&root, "eng-1-1").unwrap();
        assert_eq!(pending.len(), 1);
        let expected_from = effective_sender(tmp.path(), None);
        assert_eq!(pending[0].from, expected_from);
        assert_eq!(pending[0].to, "eng-1-1");
        assert_eq!(pending[0].body, "fix bug");
        assert_eq!(pending[0].msg_type, inbox::MessageType::Assign);
    }

    #[test]
    #[serial]
    fn send_message_ignores_detected_sender_outside_project() {
        let tmp = tempfile::tempdir().unwrap();
        let _member = EnvVarGuard::unset("TMUX_PANE");
        let original_member = std::env::var("BATTY_MEMBER").ok();
        unsafe {
            std::env::set_var("BATTY_MEMBER", "foreign-engineer-9-9");
        }

        send_message(tmp.path(), "architect", "hello").unwrap();

        match original_member.as_deref() {
            Some(value) => unsafe {
                std::env::set_var("BATTY_MEMBER", value);
            },
            None => unsafe {
                std::env::remove_var("BATTY_MEMBER");
            },
        }

        let root = inbox::inboxes_root(tmp.path());
        let pending = inbox::pending_messages(&root, "architect").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].from, "human");
    }

    #[test]
    fn resolve_member_name_maps_unique_role_alias_to_instance() {
        let tmp = tempfile::tempdir().unwrap();
        write_team_config(
            tmp.path(),
            r#"
name: test
roles:
  - name: human
    role_type: user
    talks_to:
      - sam-designer
  - name: jordan-pm
    role_type: manager
    agent: claude
    instances: 1
  - name: sam-designer
    role_type: engineer
    agent: codex
    instances: 1
    talks_to:
      - jordan-pm
"#,
        );

        assert_eq!(
            resolve_member_name(tmp.path(), "sam-designer").unwrap(),
            "sam-designer-1-1"
        );
        assert_eq!(
            resolve_member_name(tmp.path(), "sam-designer-1-1").unwrap(),
            "sam-designer-1-1"
        );
    }

    #[test]
    fn resolve_member_name_rejects_ambiguous_role_alias() {
        let tmp = tempfile::tempdir().unwrap();
        write_team_config(
            tmp.path(),
            r#"
name: test
roles:
  - name: jordan-pm
    role_type: manager
    agent: claude
    instances: 2
  - name: sam-designer
    role_type: engineer
    agent: codex
    instances: 1
    talks_to:
      - jordan-pm
"#,
        );

        let error = resolve_member_name(tmp.path(), "sam-designer")
            .unwrap_err()
            .to_string();
        assert!(error.contains("matches multiple members"));
        assert!(error.contains("sam-designer-1-1"));
        assert!(error.contains("sam-designer-2-1"));
    }

    #[test]
    #[serial]
    fn send_message_delivers_to_unique_instance_inbox() {
        let tmp = tempfile::tempdir().unwrap();
        let _tmux_pane = EnvVarGuard::unset("TMUX_PANE");
        let _batty_member = EnvVarGuard::unset("BATTY_MEMBER");
        write_team_config(
            tmp.path(),
            r#"
name: test
roles:
  - name: human
    role_type: user
    talks_to:
      - sam-designer
  - name: jordan-pm
    role_type: manager
    agent: claude
    instances: 1
  - name: sam-designer
    role_type: engineer
    agent: codex
    instances: 1
    talks_to:
      - jordan-pm
"#,
        );

        let original_tmux_pane = std::env::var_os("TMUX_PANE");
        unsafe {
            std::env::remove_var("TMUX_PANE");
        }
        let send_result = send_message(tmp.path(), "sam-designer", "hello");
        match original_tmux_pane {
            Some(value) => unsafe {
                std::env::set_var("TMUX_PANE", value);
            },
            None => unsafe {
                std::env::remove_var("TMUX_PANE");
            },
        }
        send_result.unwrap();

        let root = inbox::inboxes_root(tmp.path());
        assert!(
            inbox::pending_messages(&root, "sam-designer")
                .unwrap()
                .is_empty()
        );

        let pending = inbox::pending_messages(&root, "sam-designer-1-1").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].to, "sam-designer-1-1");
        assert_eq!(pending[0].body, "hello");
    }

    #[test]
    fn truncate_chars_handles_unicode_boundaries() {
        let body = "Task #109 confirmed complete on main. I'm available for next assignment.";
        let truncated = truncate_chars(body, 40);
        assert!(truncated.ends_with("..."));
        assert!(truncated.starts_with("Task #109 confirmed complete on main."));
    }

    #[test]
    fn format_inbox_listing_shows_most_recent_messages_by_default_limit() {
        let messages: Vec<_> = (0..25)
            .map(|idx| {
                (
                    inbox::InboxMessage {
                        id: format!("msg{idx:05}"),
                        from: "architect".to_string(),
                        to: "black-lead".to_string(),
                        body: format!("message {idx}"),
                        msg_type: inbox::MessageType::Send,
                        timestamp: idx,
                    },
                    true,
                )
            })
            .collect();

        let rendered = format_inbox_listing("black-lead", &messages, Some(20));
        assert!(rendered.contains("Showing 20 of 25 messages for black-lead."));
        assert!(!rendered.contains("message 0"));
        assert!(rendered.contains("message 5"));
        assert!(rendered.contains("message 24"));
        assert!(!rendered.contains("msg00005"));
        assert!(!rendered.contains("msg00024"));
    }

    #[test]
    fn format_inbox_listing_allows_showing_all_messages() {
        let messages: Vec<_> = (0..3)
            .map(|idx| {
                (
                    inbox::InboxMessage {
                        id: format!("msg{idx:05}"),
                        from: "architect".to_string(),
                        to: "black-lead".to_string(),
                        body: format!("message {idx}"),
                        msg_type: inbox::MessageType::Send,
                        timestamp: idx,
                    },
                    idx % 2 == 0,
                )
            })
            .collect();

        let rendered = format_inbox_listing("black-lead", &messages, None);
        assert!(!rendered.contains("Showing 20"));
        assert!(rendered.contains("REF"));
        assert!(rendered.contains("BODY"));
        assert!(rendered.contains("message 0"));
        assert!(rendered.contains("message 1"));
        assert!(rendered.contains("message 2"));
        assert!(!rendered.contains("msg00000"));
        assert!(!rendered.contains("msg00001"));
        assert!(!rendered.contains("msg00002"));
    }

    #[test]
    fn format_inbox_listing_hides_internal_message_ids() {
        let messages = vec![(
            inbox::InboxMessage {
                id: "1773930387654321.M123456P7890Q42.example".to_string(),
                from: "architect".to_string(),
                to: "black-lead".to_string(),
                body: "message body".to_string(),
                msg_type: inbox::MessageType::Send,
                timestamp: 1_773_930_725,
            },
            true,
        )];

        let rendered = format_inbox_listing("black-lead", &messages, None);
        assert!(rendered.contains("1773930725"));
        assert!(!rendered.contains("1773930387654321.M123456P7890Q42.example"));
        assert!(!rendered.contains("ID BODY"));
    }

    #[test]
    fn inbox_message_refs_use_timestamp_when_unique() {
        let messages = vec![(
            inbox::InboxMessage {
                id: "msg-1".to_string(),
                from: "architect".to_string(),
                to: "black-lead".to_string(),
                body: "message body".to_string(),
                msg_type: inbox::MessageType::Send,
                timestamp: 1_773_930_725,
            },
            true,
        )];

        let refs = inbox_message_refs(&messages);
        assert_eq!(refs, vec!["1773930725".to_string()]);
        assert_eq!(
            resolve_inbox_message_indices(&messages, "1773930725"),
            vec![0]
        );
    }

    #[test]
    fn inbox_message_refs_suffix_same_second_collisions() {
        let messages = vec![
            (
                inbox::InboxMessage {
                    id: "msg-1".to_string(),
                    from: "architect".to_string(),
                    to: "black-lead".to_string(),
                    body: "first".to_string(),
                    msg_type: inbox::MessageType::Send,
                    timestamp: 1_773_930_725,
                },
                true,
            ),
            (
                inbox::InboxMessage {
                    id: "msg-2".to_string(),
                    from: "architect".to_string(),
                    to: "black-lead".to_string(),
                    body: "second".to_string(),
                    msg_type: inbox::MessageType::Send,
                    timestamp: 1_773_930_725,
                },
                true,
            ),
        ];

        let refs = inbox_message_refs(&messages);
        assert_eq!(
            refs,
            vec!["1773930725-1".to_string(), "1773930725-2".to_string()]
        );
        assert!(resolve_inbox_message_indices(&messages, "1773930725").is_empty());
        assert_eq!(
            resolve_inbox_message_indices(&messages, "1773930725-1"),
            vec![0]
        );
        assert_eq!(
            resolve_inbox_message_indices(&messages, "1773930725-2"),
            vec![1]
        );
    }
}
