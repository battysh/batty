//! JSONL session file tracking for shim classifiers.
//!
//! Tails agent session files for higher-fidelity state detection.
//! Claude tracker: `~/.claude/projects/<cwd_encoded>/<session_id>.jsonl`
//! Codex tracker: `~/.codex/sessions/<year>/<month>/<day>/<session_id>.jsonl`

use anyhow::Result;
use serde_json::Value;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use super::classifier::AgentType;

// ---------------------------------------------------------------------------
// Tracker verdict — what the JSONL log says the agent is doing
// ---------------------------------------------------------------------------

/// Verdict from the JSONL session tracker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackerVerdict {
    /// Agent is actively processing (tool use, progress events).
    Working,
    /// Agent finished its turn (end_turn, task_complete).
    Idle,
    /// No new information from the session file.
    Unknown,
}

// ---------------------------------------------------------------------------
// Session tracker — unified state for tailing a JSONL file
// ---------------------------------------------------------------------------

/// Tracks an agent's JSONL session file, tailing from EOF.
pub struct SessionTracker {
    agent_type: AgentType,
    /// Root directory for session discovery.
    root: PathBuf,
    /// Working directory to match sessions against.
    cwd: PathBuf,
    /// Optional pre-known session ID.
    session_id: Option<String>,
    /// Currently tracked session file.
    session_file: Option<PathBuf>,
    /// Read offset in the session file.
    offset: u64,
    /// Last known verdict (sticky until overridden).
    last_verdict: TrackerVerdict,
}

impl SessionTracker {
    /// Create a new tracker. Does not discover the session file until `poll()`.
    ///
    /// - For Claude: `root` = `~/.claude/projects`
    /// - For Codex: `root` = `~/.codex/sessions`
    pub fn new(
        agent_type: AgentType,
        root: PathBuf,
        cwd: PathBuf,
        session_id: Option<String>,
    ) -> Self {
        Self {
            agent_type,
            root,
            cwd,
            session_id,
            session_file: None,
            offset: 0,
            last_verdict: TrackerVerdict::Unknown,
        }
    }

    /// Poll the session file for new events.
    ///
    /// On first call, discovers the session file and binds at EOF so that
    /// historical entries don't produce false state transitions.
    /// Returns the current sticky verdict.
    pub fn poll(&mut self) -> Result<TrackerVerdict> {
        // First-time discovery
        if self.session_file.is_none() {
            self.session_file = discover_session_file(
                self.agent_type,
                &self.root,
                &self.cwd,
                self.session_id.as_deref(),
            )?;
            if let Some(ref path) = self.session_file {
                if self.agent_type == AgentType::Codex {
                    self.session_id = codex_session_resume_id(path)?;
                }
                // Bind at EOF — ignore history
                self.offset = file_len(path)?;
                self.last_verdict = TrackerVerdict::Unknown;
            }
            return Ok(self.last_verdict);
        }

        // Check for newer session file (rebind)
        self.maybe_rebind()?;

        let Some(ref path) = self.session_file else {
            return Ok(TrackerVerdict::Unknown);
        };

        if !path.exists() {
            self.session_file = None;
            self.offset = 0;
            self.last_verdict = TrackerVerdict::Unknown;
            return Ok(TrackerVerdict::Unknown);
        }

        let path = path.clone();
        let (verdict, new_offset) = parse_session_tail(self.agent_type, &path, self.offset)?;
        self.offset = new_offset;
        if verdict != TrackerVerdict::Unknown {
            self.last_verdict = verdict;
        }
        Ok(self.last_verdict)
    }

    /// The path of the currently tracked session file, if any.
    pub fn session_file(&self) -> Option<&Path> {
        self.session_file.as_deref()
    }

    fn maybe_rebind(&mut self) -> Result<()> {
        let Some(ref current) = self.session_file else {
            return Ok(());
        };

        let Some(newest) = discover_session_file(self.agent_type, &self.root, &self.cwd, None)?
        else {
            return Ok(());
        };

        if newest == *current {
            return Ok(());
        }

        let current_modified = file_modified(current);
        let newest_modified = file_modified(&newest);

        if newest_modified <= current_modified {
            return Ok(());
        }

        self.session_file = Some(newest.clone());
        self.session_id = match self.agent_type {
            AgentType::Codex => codex_session_resume_id(&newest)?,
            AgentType::Claude => file_stem_id(&newest),
            _ => file_stem_id(&newest),
        };
        self.offset = file_len(&newest)?;
        self.last_verdict = TrackerVerdict::Unknown;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Session file discovery
// ---------------------------------------------------------------------------

fn discover_session_file(
    agent_type: AgentType,
    root: &Path,
    cwd: &Path,
    session_id: Option<&str>,
) -> Result<Option<PathBuf>> {
    match agent_type {
        AgentType::Claude => discover_claude_session(root, cwd, session_id),
        AgentType::Codex => discover_codex_session(root, cwd, session_id),
        _ => Ok(None), // Kiro / Generic don't have JSONL sessions
    }
}

/// Claude: `~/.claude/projects/<cwd_encoded>/<session_id>.jsonl`
fn discover_claude_session(
    projects_root: &Path,
    cwd: &Path,
    session_id: Option<&str>,
) -> Result<Option<PathBuf>> {
    if !projects_root.exists() {
        return Ok(None);
    }

    let preferred_dir = projects_root.join(cwd.to_string_lossy().replace('/', "-"));

    // Exact session ID lookup
    if let Some(sid) = session_id {
        let exact = preferred_dir.join(format!("{sid}.jsonl"));
        if exact.is_file() {
            return Ok(Some(exact));
        }
        return Ok(None);
    }

    // Newest JSONL in preferred directory
    if preferred_dir.is_dir() {
        if let Some(path) = newest_jsonl_in(&preferred_dir)? {
            return Ok(Some(path));
        }
    }

    // Fall back: scan all project dirs for matching cwd
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for project_dir in read_dir_sorted(projects_root)? {
        if !project_dir.is_dir() {
            continue;
        }
        for entry in read_dir_sorted(&project_dir)? {
            if entry.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if claude_session_cwd(&entry)?.as_deref() != Some(cwd.as_os_str()) {
                continue;
            }
            let modified = file_modified(&entry);
            match &newest {
                Some((t, _)) if modified <= *t => {}
                _ => newest = Some((modified, entry)),
            }
        }
    }

    Ok(newest.map(|(_, p)| p))
}

/// Codex: `~/.codex/sessions/<year>/<month>/<day>/<session_id>.jsonl`
fn discover_codex_session(
    sessions_root: &Path,
    cwd: &Path,
    session_id: Option<&str>,
) -> Result<Option<PathBuf>> {
    if !sessions_root.exists() {
        return Ok(None);
    }

    // Walk year/month/day hierarchy
    if let Some(sid) = session_id {
        for year in read_dir_sorted(sessions_root)? {
            for month in read_dir_sorted(&year)? {
                for day in read_dir_sorted(&month)? {
                    for entry in read_dir_sorted(&day)? {
                        if entry.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                            continue;
                        }
                        let Some(meta) = read_codex_session_meta(&entry)? else {
                            continue;
                        };
                        if meta.cwd.as_deref() != Some(cwd.as_os_str()) {
                            continue;
                        }
                        if file_stem_id(&entry).as_deref() == Some(sid)
                            || meta.id.as_deref() == Some(sid)
                        {
                            return Ok(Some(entry));
                        }
                    }
                }
            }
        }
        return Ok(None);
    }

    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for year in read_dir_sorted(sessions_root)? {
        for month in read_dir_sorted(&year)? {
            for day in read_dir_sorted(&month)? {
                for entry in read_dir_sorted(&day)? {
                    if entry.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                        continue;
                    }
                    if read_codex_session_meta(&entry)?.and_then(|meta| meta.cwd)
                        != Some(cwd.as_os_str().to_os_string())
                    {
                        continue;
                    }
                    let modified = file_modified(&entry);
                    match &newest {
                        Some((t, _)) if modified <= *t => {}
                        _ => newest = Some((modified, entry)),
                    }
                }
            }
        }
    }

    Ok(newest.map(|(_, p)| p))
}

// ---------------------------------------------------------------------------
// JSONL log entry classification
// ---------------------------------------------------------------------------

fn parse_session_tail(
    agent_type: AgentType,
    path: &Path,
    start_offset: u64,
) -> Result<(TrackerVerdict, u64)> {
    let file_len = fs::metadata(path)?.len();
    let mut offset = if file_len < start_offset {
        0
    } else {
        start_offset
    };

    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(offset))?;

    let mut verdict = TrackerVerdict::Unknown;
    loop {
        let line_start = reader.stream_position()?;
        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            break;
        }
        // Incomplete line — rewind and wait for next poll
        if !line.ends_with('\n') {
            reader.seek(SeekFrom::Start(line_start))?;
            break;
        }

        if let Ok(entry) = serde_json::from_str::<Value>(&line) {
            let v = match agent_type {
                AgentType::Claude => classify_claude_entry(&entry),
                AgentType::Codex => classify_codex_entry(&entry),
                _ => TrackerVerdict::Unknown,
            };
            if v != TrackerVerdict::Unknown {
                verdict = v;
            }
        }

        offset = reader.stream_position()?;
    }

    Ok((verdict, offset))
}

/// Classify a Claude JSONL log entry.
///
/// - `type: "assistant"` with `stop_reason: "tool_use"` → Working
/// - `type: "assistant"` with `stop_reason: "end_turn"` → Idle
/// - `type: "progress"` → Working
/// - `type: "user"` with `toolUseResult` → Working
/// - `type: "user"` with interrupt text → Idle
fn classify_claude_entry(entry: &Value) -> TrackerVerdict {
    match entry.get("type").and_then(Value::as_str) {
        Some("assistant") => {
            let stop_reason = entry
                .get("message")
                .and_then(|m| m.get("stop_reason"))
                .and_then(Value::as_str);
            match stop_reason {
                Some("tool_use") => TrackerVerdict::Working,
                Some("end_turn") => TrackerVerdict::Idle,
                _ => TrackerVerdict::Unknown,
            }
        }
        Some("progress") => TrackerVerdict::Working,
        Some("user") => {
            if entry
                .get("toolUseResult")
                .and_then(Value::as_object)
                .is_some()
            {
                return TrackerVerdict::Working;
            }
            if let Some(content) = entry.get("message").and_then(|m| m.get("content")) {
                if let Some(text) = content.as_str() {
                    if text == "[Request interrupted by user]" {
                        return TrackerVerdict::Idle;
                    }
                    return TrackerVerdict::Working;
                }
                if let Some(items) = content.as_array() {
                    for item in items {
                        if item.get("tool_use_id").is_some() {
                            return TrackerVerdict::Working;
                        }
                        if item.get("type").and_then(Value::as_str) == Some("text")
                            && item.get("text").and_then(Value::as_str)
                                == Some("[Request interrupted by user]")
                        {
                            return TrackerVerdict::Idle;
                        }
                    }
                    return TrackerVerdict::Working;
                }
            }
            TrackerVerdict::Unknown
        }
        _ => TrackerVerdict::Unknown,
    }
}

/// Classify a Codex JSONL log entry.
///
/// - `type: "event_msg"` with `payload.type: "task_complete"` → Idle
/// - Any other new event → Working (handled by caller via "had new events")
fn classify_codex_entry(entry: &Value) -> TrackerVerdict {
    if entry.get("type").and_then(Value::as_str) == Some("event_msg")
        && entry
            .get("payload")
            .and_then(|p| p.get("type"))
            .and_then(Value::as_str)
            == Some("task_complete")
    {
        return TrackerVerdict::Idle;
    }
    // Any parseable JSONL line means the agent is active
    TrackerVerdict::Working
}

// ---------------------------------------------------------------------------
// Merge logic — combine screen + tracker verdicts
// ---------------------------------------------------------------------------

use super::classifier::ScreenVerdict;

/// Final merged state after combining screen and tracker verdicts.
///
/// Merge priority per spec:
/// - Claude: Screen > Tracker (screen is more reliable for live state)
/// - Codex: Tracker > Screen (task_complete is ground truth for completion)
/// - Other types: Screen only (no JSONL tracker)
pub fn merge_verdicts(
    agent_type: AgentType,
    screen: ScreenVerdict,
    tracker: TrackerVerdict,
) -> ScreenVerdict {
    match agent_type {
        AgentType::Claude => {
            // Screen takes priority — fall back to tracker when screen is Unknown
            match screen {
                ScreenVerdict::AgentIdle
                | ScreenVerdict::AgentWorking
                | ScreenVerdict::ContextExhausted => screen,
                ScreenVerdict::Unknown => match tracker {
                    TrackerVerdict::Working => ScreenVerdict::AgentWorking,
                    TrackerVerdict::Idle => ScreenVerdict::AgentIdle,
                    TrackerVerdict::Unknown => ScreenVerdict::Unknown,
                },
            }
        }
        AgentType::Codex => {
            // Tracker takes priority for completion detection
            match tracker {
                TrackerVerdict::Idle => ScreenVerdict::AgentIdle, // task_complete
                TrackerVerdict::Working => {
                    // Tracker says active — screen can override to idle only
                    // if the prompt is visible (unlikely during work)
                    match screen {
                        ScreenVerdict::ContextExhausted => ScreenVerdict::ContextExhausted,
                        _ => ScreenVerdict::AgentWorking,
                    }
                }
                TrackerVerdict::Unknown => screen, // No tracker info, use screen
            }
        }
        // Kiro / Generic: no tracker, screen only
        _ => screen,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn claude_session_cwd(path: &Path) -> Result<Option<std::ffi::OsString>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(cwd) = entry.get("cwd").and_then(Value::as_str) {
            return Ok(Some(std::ffi::OsString::from(cwd)));
        }
    }
    Ok(None)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CodexSessionMeta {
    id: Option<String>,
    cwd: Option<std::ffi::OsString>,
}

fn read_codex_session_meta(path: &Path) -> Result<Option<CodexSessionMeta>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if entry.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let payload = entry.get("payload");
        return Ok(Some(CodexSessionMeta {
            id: payload
                .and_then(|payload| payload.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string),
            cwd: payload
                .and_then(|payload| payload.get("cwd"))
                .and_then(Value::as_str)
                .map(std::ffi::OsString::from),
        }));
    }
    Ok(None)
}

fn codex_session_resume_id(path: &Path) -> Result<Option<String>> {
    Ok(read_codex_session_meta(path)?
        .and_then(|meta| meta.id)
        .or_else(|| file_stem_id(path)))
}

fn newest_jsonl_in(dir: &Path) -> Result<Option<PathBuf>> {
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in read_dir_sorted(dir)? {
        if entry.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let modified = file_modified(&entry);
        match &newest {
            Some((t, _)) if modified <= *t => {}
            _ => newest = Some((modified, entry)),
        }
    }
    Ok(newest.map(|(_, p)| p))
}

fn read_dir_sorted(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    entries.sort();
    Ok(entries)
}

fn file_len(path: &Path) -> Result<u64> {
    Ok(fs::metadata(path)?.len())
}

fn file_modified(path: &Path) -> std::time::SystemTime {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
}

fn file_stem_id(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // -- Claude entry classification --

    #[test]
    fn claude_tool_use_is_working() {
        let entry: Value =
            serde_json::from_str(r#"{"type":"assistant","message":{"stop_reason":"tool_use"}}"#)
                .unwrap();
        assert_eq!(classify_claude_entry(&entry), TrackerVerdict::Working);
    }

    #[test]
    fn claude_end_turn_is_idle() {
        let entry: Value =
            serde_json::from_str(r#"{"type":"assistant","message":{"stop_reason":"end_turn"}}"#)
                .unwrap();
        assert_eq!(classify_claude_entry(&entry), TrackerVerdict::Idle);
    }

    #[test]
    fn claude_progress_is_working() {
        let entry: Value = serde_json::from_str(r#"{"type":"progress","data":"chunk"}"#).unwrap();
        assert_eq!(classify_claude_entry(&entry), TrackerVerdict::Working);
    }

    #[test]
    fn claude_tool_result_is_working() {
        let entry: Value =
            serde_json::from_str(r#"{"type":"user","toolUseResult":{"stdout":"ok"}}"#).unwrap();
        assert_eq!(classify_claude_entry(&entry), TrackerVerdict::Working);
    }

    #[test]
    fn claude_user_text_is_working() {
        let entry: Value =
            serde_json::from_str(r#"{"type":"user","message":{"content":"do something"}}"#)
                .unwrap();
        assert_eq!(classify_claude_entry(&entry), TrackerVerdict::Working);
    }

    #[test]
    fn claude_interrupt_is_idle() {
        let entry: Value = serde_json::from_str(
            r#"{"type":"user","message":{"content":"[Request interrupted by user]"}}"#,
        )
        .unwrap();
        assert_eq!(classify_claude_entry(&entry), TrackerVerdict::Idle);
    }

    #[test]
    fn claude_interrupt_in_array_is_idle() {
        let entry: Value = serde_json::from_str(
            r#"{"type":"user","message":{"content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#,
        )
        .unwrap();
        assert_eq!(classify_claude_entry(&entry), TrackerVerdict::Idle);
    }

    #[test]
    fn claude_unknown_type() {
        let entry: Value = serde_json::from_str(r#"{"type":"system","message":"init"}"#).unwrap();
        assert_eq!(classify_claude_entry(&entry), TrackerVerdict::Unknown);
    }

    // -- Codex entry classification --

    #[test]
    fn codex_task_complete_is_idle() {
        let entry: Value =
            serde_json::from_str(r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#)
                .unwrap();
        assert_eq!(classify_codex_entry(&entry), TrackerVerdict::Idle);
    }

    #[test]
    fn codex_other_event_is_working() {
        let entry: Value = serde_json::from_str(
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[]}}"#,
        )
        .unwrap();
        assert_eq!(classify_codex_entry(&entry), TrackerVerdict::Working);
    }

    // -- Merge verdicts --

    #[test]
    fn claude_screen_takes_priority() {
        // Screen idle wins over tracker working
        assert_eq!(
            merge_verdicts(
                AgentType::Claude,
                ScreenVerdict::AgentIdle,
                TrackerVerdict::Working
            ),
            ScreenVerdict::AgentIdle,
        );
        // Screen working wins over tracker idle
        assert_eq!(
            merge_verdicts(
                AgentType::Claude,
                ScreenVerdict::AgentWorking,
                TrackerVerdict::Idle
            ),
            ScreenVerdict::AgentWorking,
        );
    }

    #[test]
    fn claude_tracker_fills_unknown_screen() {
        assert_eq!(
            merge_verdicts(
                AgentType::Claude,
                ScreenVerdict::Unknown,
                TrackerVerdict::Working
            ),
            ScreenVerdict::AgentWorking,
        );
        assert_eq!(
            merge_verdicts(
                AgentType::Claude,
                ScreenVerdict::Unknown,
                TrackerVerdict::Idle
            ),
            ScreenVerdict::AgentIdle,
        );
    }

    #[test]
    fn codex_tracker_takes_priority() {
        // Tracker idle (task_complete) wins over screen unknown
        assert_eq!(
            merge_verdicts(
                AgentType::Codex,
                ScreenVerdict::Unknown,
                TrackerVerdict::Idle
            ),
            ScreenVerdict::AgentIdle,
        );
        // Tracker working wins, shows working even if screen says idle
        assert_eq!(
            merge_verdicts(
                AgentType::Codex,
                ScreenVerdict::AgentIdle,
                TrackerVerdict::Working
            ),
            ScreenVerdict::AgentWorking,
        );
    }

    #[test]
    fn codex_context_exhausted_overrides_tracker() {
        assert_eq!(
            merge_verdicts(
                AgentType::Codex,
                ScreenVerdict::ContextExhausted,
                TrackerVerdict::Working,
            ),
            ScreenVerdict::ContextExhausted,
        );
    }

    #[test]
    fn codex_no_tracker_falls_to_screen() {
        assert_eq!(
            merge_verdicts(
                AgentType::Codex,
                ScreenVerdict::AgentIdle,
                TrackerVerdict::Unknown
            ),
            ScreenVerdict::AgentIdle,
        );
    }

    #[test]
    fn kiro_ignores_tracker() {
        assert_eq!(
            merge_verdicts(
                AgentType::Kiro,
                ScreenVerdict::AgentWorking,
                TrackerVerdict::Idle
            ),
            ScreenVerdict::AgentWorking,
        );
    }

    #[test]
    fn generic_ignores_tracker() {
        assert_eq!(
            merge_verdicts(
                AgentType::Generic,
                ScreenVerdict::Unknown,
                TrackerVerdict::Working
            ),
            ScreenVerdict::Unknown,
        );
    }

    // -- Session discovery (Claude) --

    #[test]
    fn discovers_claude_session_in_preferred_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("projects");
        let cwd = PathBuf::from("/Users/test/myproject");
        let project_dir = root.join("-Users-test-myproject");
        fs::create_dir_all(&project_dir).unwrap();

        let session = project_dir.join("abc123.jsonl");
        fs::write(&session, "{\"cwd\":\"/Users/test/myproject\"}\n").unwrap();

        let found = discover_claude_session(&root, &cwd, None).unwrap();
        assert_eq!(found.as_deref(), Some(session.as_path()));
    }

    #[test]
    fn claude_exact_session_id_lookup() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("projects");
        let cwd = PathBuf::from("/Users/test/myproject");
        let project_dir = root.join("-Users-test-myproject");
        fs::create_dir_all(&project_dir).unwrap();

        let session = project_dir.join("exact-id.jsonl");
        fs::write(&session, "{}\n").unwrap();

        let found = discover_claude_session(&root, &cwd, Some("exact-id")).unwrap();
        assert_eq!(found.as_deref(), Some(session.as_path()));

        let missing = discover_claude_session(&root, &cwd, Some("nonexistent")).unwrap();
        assert!(missing.is_none());
    }

    #[test]
    fn claude_nonexistent_root_returns_none() {
        let found =
            discover_claude_session(Path::new("/nonexistent"), Path::new("/foo"), None).unwrap();
        assert!(found.is_none());
    }

    // -- Session discovery (Codex) --

    #[test]
    fn discovers_codex_session_by_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("sessions");
        let day_dir = root.join("2026").join("03").join("23");
        fs::create_dir_all(&day_dir).unwrap();

        let cwd = PathBuf::from("/Users/test/repo");
        let session = day_dir.join("sess1.jsonl");
        fs::write(
            &session,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"cwd\":\"{}\"}}}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let found = discover_codex_session(&root, &cwd, None).unwrap();
        assert_eq!(found.as_deref(), Some(session.as_path()));
    }

    #[test]
    fn codex_exact_session_id_lookup() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("sessions");
        let day_dir = root.join("2026").join("03").join("23");
        fs::create_dir_all(&day_dir).unwrap();

        let cwd = PathBuf::from("/Users/test/repo");
        let session = day_dir.join("my-session.jsonl");
        fs::write(
            &session,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"cwd\":\"{}\"}}}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let found = discover_codex_session(&root, &cwd, Some("my-session")).unwrap();
        assert_eq!(found.as_deref(), Some(session.as_path()));

        let missing = discover_codex_session(&root, &cwd, Some("nope")).unwrap();
        assert!(missing.is_none());
    }

    #[test]
    fn codex_exact_session_id_lookup_matches_payload_id() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("sessions");
        let day_dir = root.join("2026").join("03").join("23");
        fs::create_dir_all(&day_dir).unwrap();

        let cwd = PathBuf::from("/Users/test/repo");
        let session = day_dir.join("rollout-2026-03-26T13-54-07-sample.jsonl");
        fs::write(
            &session,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d2b48-3d33-7613-bb3d-d0b4ecd45e2e\",\"cwd\":\"{}\"}}}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let found =
            discover_codex_session(&root, &cwd, Some("019d2b48-3d33-7613-bb3d-d0b4ecd45e2e"))
                .unwrap();
        assert_eq!(found.as_deref(), Some(session.as_path()));
        assert_eq!(
            codex_session_resume_id(&session).unwrap().as_deref(),
            Some("019d2b48-3d33-7613-bb3d-d0b4ecd45e2e")
        );
    }

    #[test]
    fn codex_nonexistent_root_returns_none() {
        let found =
            discover_codex_session(Path::new("/nonexistent"), Path::new("/foo"), None).unwrap();
        assert!(found.is_none());
    }

    // -- Session tracker poll --

    #[test]
    fn tracker_binds_at_eof_ignoring_history() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("projects");
        let cwd = PathBuf::from("/Users/test/proj");
        let project_dir = root.join("-Users-test-proj");
        fs::create_dir_all(&project_dir).unwrap();

        let session = project_dir.join("s1.jsonl");
        fs::write(
            &session,
            concat!(
                "{\"type\":\"user\",\"cwd\":\"/Users/test/proj\",\"message\":{\"content\":\"hi\"}}\n",
                "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"tool_use\"}}\n",
            ),
        )
        .unwrap();

        let mut tracker = SessionTracker::new(AgentType::Claude, root, cwd, None);

        // First poll discovers + binds at EOF, returns Unknown
        assert_eq!(tracker.poll().unwrap(), TrackerVerdict::Unknown);
        assert!(tracker.session_file.is_some());
    }

    #[test]
    fn tracker_reports_new_events_after_bind() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("projects");
        let cwd = PathBuf::from("/Users/test/proj2");
        let project_dir = root.join("-Users-test-proj2");
        fs::create_dir_all(&project_dir).unwrap();

        let session = project_dir.join("s2.jsonl");
        fs::write(
            &session,
            "{\"type\":\"user\",\"cwd\":\"/Users/test/proj2\"}\n",
        )
        .unwrap();

        let mut tracker = SessionTracker::new(AgentType::Claude, root, cwd, None);
        assert_eq!(tracker.poll().unwrap(), TrackerVerdict::Unknown);

        // Append new events
        let mut f = fs::OpenOptions::new().append(true).open(&session).unwrap();
        writeln!(
            f,
            "{{\"type\":\"assistant\",\"message\":{{\"stop_reason\":\"tool_use\"}}}}"
        )
        .unwrap();
        f.flush().unwrap();

        assert_eq!(tracker.poll().unwrap(), TrackerVerdict::Working);

        // Append end_turn
        writeln!(
            f,
            "{{\"type\":\"assistant\",\"message\":{{\"stop_reason\":\"end_turn\"}}}}"
        )
        .unwrap();
        f.flush().unwrap();

        assert_eq!(tracker.poll().unwrap(), TrackerVerdict::Idle);
    }

    #[test]
    fn tracker_sticky_verdict_on_no_new_events() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("projects");
        let cwd = PathBuf::from("/Users/test/proj3");
        let project_dir = root.join("-Users-test-proj3");
        fs::create_dir_all(&project_dir).unwrap();

        let session = project_dir.join("s3.jsonl");
        fs::write(
            &session,
            "{\"type\":\"user\",\"cwd\":\"/Users/test/proj3\"}\n",
        )
        .unwrap();

        let mut tracker = SessionTracker::new(AgentType::Claude, root, cwd, None);
        tracker.poll().unwrap(); // bind

        let mut f = fs::OpenOptions::new().append(true).open(&session).unwrap();
        writeln!(
            f,
            "{{\"type\":\"assistant\",\"message\":{{\"stop_reason\":\"end_turn\"}}}}"
        )
        .unwrap();
        f.flush().unwrap();

        assert_eq!(tracker.poll().unwrap(), TrackerVerdict::Idle);
        // No new events — verdict stays Idle (sticky)
        assert_eq!(tracker.poll().unwrap(), TrackerVerdict::Idle);
    }

    #[test]
    fn codex_tracker_detects_task_complete() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("sessions");
        let day_dir = root.join("2026").join("03").join("23");
        fs::create_dir_all(&day_dir).unwrap();

        let cwd = PathBuf::from("/Users/test/codex-proj");
        let session = day_dir.join("cx1.jsonl");
        fs::write(
            &session,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"cwd\":\"{}\"}}}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let mut tracker = SessionTracker::new(AgentType::Codex, root, cwd.clone(), None);
        tracker.poll().unwrap(); // bind

        let mut f = fs::OpenOptions::new().append(true).open(&session).unwrap();
        writeln!(
            f,
            "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\"}}}}"
        )
        .unwrap();
        f.flush().unwrap();

        assert_eq!(tracker.poll().unwrap(), TrackerVerdict::Idle);
    }

    #[test]
    fn tracker_handles_deleted_session_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("projects");
        let cwd = PathBuf::from("/Users/test/proj4");
        let project_dir = root.join("-Users-test-proj4");
        fs::create_dir_all(&project_dir).unwrap();

        let session = project_dir.join("s4.jsonl");
        fs::write(
            &session,
            "{\"type\":\"user\",\"cwd\":\"/Users/test/proj4\"}\n",
        )
        .unwrap();

        let mut tracker = SessionTracker::new(AgentType::Claude, root, cwd, None);
        tracker.poll().unwrap(); // bind
        assert!(tracker.session_file.is_some());

        // Delete the file
        fs::remove_file(&session).unwrap();
        assert_eq!(tracker.poll().unwrap(), TrackerVerdict::Unknown);
        assert!(tracker.session_file.is_none());
    }

    #[test]
    fn kiro_tracker_always_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let cwd = PathBuf::from("/Users/test/kiro");

        let mut tracker = SessionTracker::new(AgentType::Kiro, root, cwd, None);
        // No session discovered for non-supported types
        assert_eq!(tracker.poll().unwrap(), TrackerVerdict::Unknown);
    }

    #[test]
    fn tracker_rebinds_to_newer_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("projects");
        let cwd = PathBuf::from("/Users/test/proj5");
        let project_dir = root.join("-Users-test-proj5");
        fs::create_dir_all(&project_dir).unwrap();

        let old_session = project_dir.join("old.jsonl");
        fs::write(
            &old_session,
            "{\"type\":\"user\",\"cwd\":\"/Users/test/proj5\"}\n",
        )
        .unwrap();

        let mut tracker = SessionTracker::new(AgentType::Claude, root, cwd, None);
        tracker.poll().unwrap(); // bind to old
        assert_eq!(tracker.session_file.as_deref(), Some(old_session.as_path()));

        // Create a newer file after a brief delay
        std::thread::sleep(std::time::Duration::from_millis(20));
        let new_session = project_dir.join("new.jsonl");
        fs::write(
            &new_session,
            "{\"type\":\"user\",\"cwd\":\"/Users/test/proj5\"}\n",
        )
        .unwrap();

        // Poll should rebind to the newer file
        tracker.poll().unwrap();
        assert_eq!(tracker.session_file.as_deref(), Some(new_session.as_path()));
    }

    #[test]
    fn codex_tracker_rebind_keeps_payload_resume_id() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("sessions");
        let day_dir = root.join("2026").join("03").join("27");
        fs::create_dir_all(&day_dir).unwrap();

        let cwd = PathBuf::from("/Users/test/repo");
        let old_session = day_dir.join("rollout-2026-03-27T10-00-00-old.jsonl");
        fs::write(
            &old_session,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"old-resume-id\",\"cwd\":\"{}\"}}}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        let mut tracker = SessionTracker::new(AgentType::Codex, root, cwd.clone(), None);
        tracker.poll().unwrap();
        assert_eq!(tracker.session_file.as_deref(), Some(old_session.as_path()));
        assert_eq!(tracker.session_id.as_deref(), Some("old-resume-id"));

        std::thread::sleep(std::time::Duration::from_millis(20));
        let new_session = day_dir.join("rollout-2026-03-27T10-01-00-new.jsonl");
        fs::write(
            &new_session,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"new-resume-id\",\"cwd\":\"{}\"}}}}\n",
                cwd.display()
            ),
        )
        .unwrap();

        tracker.poll().unwrap();
        assert_eq!(tracker.session_file.as_deref(), Some(new_session.as_path()));
        assert_eq!(tracker.session_id.as_deref(), Some("new-resume-id"));
    }

    // -- parse_session_tail --

    #[test]
    fn parse_tail_handles_truncated_file() {
        let tmp = tempfile::tempdir().unwrap();
        let session = tmp.path().join("truncated.jsonl");
        fs::write(
            &session,
            "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"end_turn\"}}\n",
        )
        .unwrap();

        // Start offset beyond file length → resets to 0
        let (verdict, _) = parse_session_tail(AgentType::Claude, &session, 9999).unwrap();
        assert_eq!(verdict, TrackerVerdict::Idle);
    }

    #[test]
    fn parse_tail_skips_incomplete_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let session = tmp.path().join("incomplete.jsonl");
        // Write a complete line followed by an incomplete one (no trailing newline)
        let mut f = File::create(&session).unwrap();
        write!(
            f,
            "{{\"type\":\"assistant\",\"message\":{{\"stop_reason\":\"tool_use\"}}}}\n{{\"type\":\"partial"
        )
        .unwrap();
        f.flush().unwrap();

        let (verdict, offset) = parse_session_tail(AgentType::Claude, &session, 0).unwrap();
        assert_eq!(verdict, TrackerVerdict::Working);
        // Offset should stop before the incomplete line
        let complete_line = "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"tool_use\"}}\n";
        assert_eq!(offset, complete_line.len() as u64);
    }

    // -- Graceful degradation --

    #[test]
    fn tracker_graceful_no_session_file() {
        let tmp = tempfile::tempdir().unwrap();
        // Empty root — no session files
        let root = tmp.path().join("empty_projects");
        fs::create_dir_all(&root).unwrap();

        let mut tracker =
            SessionTracker::new(AgentType::Claude, root, PathBuf::from("/no/match"), None);

        // Should return Unknown without error
        assert_eq!(tracker.poll().unwrap(), TrackerVerdict::Unknown);
        assert!(tracker.session_file.is_none());
    }
}
