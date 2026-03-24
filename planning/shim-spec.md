# Agent Shim Specification

## 1. Overview

The **agent shim** is a standalone process that wraps a single AI coding CLI
(Claude Code, Codex, Kiro, or any interactive terminal program) behind a
message-oriented interface. It replaces the current tmux-based agent
management with a clean process abstraction.

**Core principle:** The shim is a *container* for one agent. It owns the
agent's terminal, detects the agent's state, accepts work via structured
messages, and emits structured events. The orchestrator never touches a PTY,
never screen-scrapes, never tails JSONL files.

### What moves INTO the shim

- PTY creation and management (currently `tmux create-session`, `send-keys`)
- Virtual screen buffer (replaces `tmux capture-pane`)
- State classification (currently `watcher/screen.rs` — prompt detection,
  spinner detection, context exhaustion)
- Session tracking (currently `watcher/claude.rs`, `watcher/codex.rs` — JSONL
  tailing)
- Message injection (currently `message.rs` — paste-buffer protocol)
- State machine (currently `watcher/mod.rs` — Active/Ready/Idle/Dead/Exhausted)

### What stays OUTSIDE (in the orchestrator)

- Workflow policy (dispatch rules, review gates, merge logic)
- Topology and routing (who talks to whom)
- Board operations (kanban state)
- Automation (nudges, standups, retros, interventions)
- Telegram bridge
- CLI user interface

### What the orchestrator sees

```rust
struct AgentHandle {
    id: String,
    channel: FramedChannel,   // send commands, receive events
    child: Child,             // process handle for lifecycle
}
```

No PTY, no tmux, no screen content unless explicitly requested via
`CaptureScreen` command.

---

## 2. Process Model

```
Orchestrator (batty daemon)
    │
    ├── fork/exec ──→ batty shim --id eng-1 --agent-type claude --cmd "claude ..." --cwd /path
    │   └── fd 3: socketpair(SEQPACKET) ──→ bidirectional message channel
    │
    ├── fork/exec ──→ batty shim --id eng-2 --agent-type codex --cmd "codex ..." --cwd /path
    │   └── fd 3: socketpair(SEQPACKET)
    │
    └── fork/exec ──→ batty shim --id eng-3 --agent-type claude --cmd "claude ..." --cwd /path
        └── fd 3: socketpair(SEQPACKET)
```

Each shim is a child process of the orchestrator. Communication is via an
inherited Unix socketpair (SOCK_SEQPACKET for message boundaries). No
filesystem coordination, no named sockets, no discovery protocol.

**Lifecycle:**

1. Orchestrator creates `socketpair(AF_UNIX, SOCK_SEQPACKET, 0)`
2. Orchestrator `fork/exec`'s `batty shim` with child fd inherited as fd 3
3. Shim creates PTY, spawns agent CLI on slave side
4. Shim sends `Ready` event once agent prompt is detected
5. Normal operation: commands in, events out
6. Shutdown: orchestrator sends `Shutdown`, shim terminates agent, exits
7. Crash: orchestrator detects child exit + socket EOF, can respawn

---

## 3. Shim Internal Architecture

```
┌─────────────────────────────────────────────────────────┐
│  batty shim process                                     │
│                                                         │
│  ┌──────────┐     ┌───────────────┐     ┌───────────┐  │
│  │  PTY     │────→│  vt100 Parser │────→│ Classifier│  │
│  │  master  │     │  (screen buf) │     │ (per-type)│  │
│  └──────────┘     └───────────────┘     └───────────┘  │
│       │                   │                    │        │
│       │ write             │ screen()           │ state  │
│       ▼                   ▼                    ▼        │
│  ┌──────────┐     ┌───────────────┐     ┌───────────┐  │
│  │  Agent   │     │  Screen       │     │  State    │  │
│  │  CLI     │     │  Capture      │     │  Machine  │  │
│  │ (claude) │     │  on demand    │     │           │  │
│  └──────────┘     └───────────────┘     └───────────┘  │
│                                                │        │
│  ┌──────────────────────────────────────┐      │        │
│  │  JSONL Tracker (optional sidecar)    │──────┘        │
│  │  Claude: ~/.claude/projects/...jsonl │               │
│  │  Codex:  ~/.codex/sessions/...jsonl  │               │
│  └──────────────────────────────────────┘               │
│                                                         │
│  ┌──────────────────────────────────────┐               │
│  │  Channel (fd 3, FramedChannel)       │               │
│  │  recv: Command    send: Event        │               │
│  └──────────────────────────────────────┘               │
└─────────────────────────────────────────────────────────┘
```

### 3.1 PTY Management

The shim uses `portable-pty` to create a master/slave PTY pair and spawns the
agent CLI on the slave side.

```rust
let pty_system = portable_pty::native_pty_system();
let pty_pair = pty_system.openpty(PtySize {
    rows: 50,
    cols: 220,
    pixel_width: 0,
    pixel_height: 0,
})?;

let mut cmd = CommandBuilder::new("bash");
cmd.args(["-c", &args.cmd]);
cmd.cwd(&args.cwd);
// Unset CLAUDECODE to prevent nested detection
cmd.env_remove("CLAUDECODE");

let child = pty_pair.slave.spawn_command(cmd)?;
drop(pty_pair.slave); // close slave in parent
let reader = pty_pair.master.try_clone_reader()?;
let writer = pty_pair.master.take_writer()?;
```

**PTY dimensions:** 220 columns x 50 rows (matches current tmux defaults).
Configurable via `--cols` and `--rows` flags, and dynamically via `Resize`
command.

**Environment:** The shim unsets `CLAUDECODE` (prevents Claude Code from
detecting it's inside another Claude session). Additional env vars can be
passed via `--env KEY=VALUE` flags.

### 3.2 Virtual Screen (vt100)

The `vt100` crate provides a headless terminal emulator. All bytes read from
the PTY master are fed into the parser, which maintains a cell grid identical
to what a real terminal would display.

```rust
let mut parser = vt100::Parser::new(rows, cols, scrollback_lines);

// In the PTY reader loop:
parser.process(&bytes_from_pty);

// Screen queries (equivalent to tmux capture-pane):
let full_text = parser.screen().contents();
let row_range = parser.screen().contents_between(
    start_row, 0,
    end_row, cols - 1,
);
let cursor = parser.screen().cursor_position(); // (row, col)
```

**Scrollback:** 5000 lines (configurable). Allows capturing history beyond
the visible screen.

**Change detection:** Hash the screen contents each poll cycle. Only run the
classifier when the hash changes (avoids redundant classification on
identical screens).

```rust
fn content_hash(screen: &vt100::Screen) -> u64 {
    // FNV-1a hash of visible screen contents
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in screen.contents().bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}
```

### 3.3 State Machine

```
                    ┌─────────────┐
          spawn     │             │  agent prompt detected
       ──────────→  │  Starting   │ ─────────────────────→ Ready ──→ (send Ready event)
                    │             │                           │
                    └──────┬──────┘                           │
                           │ process died                     │
                           ▼                                  ▼
                    ┌─────────────┐                    ┌─────────────┐
                    │    Dead     │ ←───────────────── │    Idle     │ ←──────────┐
                    │             │   process exited   │             │            │
                    └─────────────┘                    └──────┬──────┘            │
                           ▲                                  │                   │
                           │                                  │ message injected  │
                           │                                  ▼                   │
                    ┌──────┴──────┐                    ┌─────────────┐            │
                    │  Context    │ ←───────────────── │   Working   │ ───────────┘
                    │  Exhausted  │  exhaustion detect  │             │  completion
                    └─────────────┘                    └─────────────┘  detected
```

**States:**

| State | Description | Entry condition |
|-------|-------------|-----------------|
| `Starting` | PTY created, agent CLI launched, waiting for first prompt | Initial state after spawn |
| `Ready` | Agent prompt detected for first time after (re)start | Classifier detects idle prompt in `Starting` |
| `Idle` | Agent is at its prompt, waiting for input | Classifier detects idle prompt in `Working` state |
| `Working` | Agent is processing a message (producing output) | Message injected in `Idle` state |
| `Dead` | Agent process exited | `waitpid` returns / PTY read returns EOF |
| `ContextExhausted` | Agent reports conversation too large | Classifier detects exhaustion patterns |

**State transitions emit events.** Every transition from one state to another
causes a `StateChanged` event to be sent through the channel.

**Completion detection.** When `Working → Idle`:
1. The shim captures the screen content
2. Computes the "response" — screen content that appeared since the message
   was injected (diff between pre-injection and post-completion snapshots)
3. Emits a `Completion` event with the response text and last N lines

### 3.4 State Classifiers

Each agent type provides a classifier that maps screen content + optional
JSONL tracker state to a `ScreenVerdict`:

```rust
enum ScreenVerdict {
    AgentIdle,           // at prompt, ready for input
    AgentWorking,        // producing output, processing
    ContextExhausted,    // session too large
    Unknown,             // can't determine (keep previous state)
}

trait StateClassifier: Send {
    /// Classify the current screen content.
    fn classify_screen(&self, screen: &vt100::Screen) -> ScreenVerdict;

    /// Optional: poll a sidecar JSONL session file for state signals.
    /// Returns None if the agent type doesn't use JSONL tracking.
    fn poll_session_tracker(&mut self) -> Option<ScreenVerdict> {
        None
    }

    /// Merge screen and tracker verdicts. Default: screen wins for Claude,
    /// tracker wins for Codex completion detection.
    fn merge_verdicts(
        &self,
        screen: ScreenVerdict,
        tracker: Option<ScreenVerdict>,
    ) -> ScreenVerdict {
        // Default: screen verdict takes priority, tracker as fallback
        match screen {
            ScreenVerdict::Unknown => tracker.unwrap_or(ScreenVerdict::Unknown),
            other => other,
        }
    }

    /// Discover and bind to the agent's session file (if applicable).
    fn bind_session(&mut self, _cwd: &Path, _session_id: Option<&str>) {}
}
```

#### 3.4.1 Claude Classifier

Detects Claude Code's terminal UI patterns:

**Idle detection:**
- Line contains `❯` followed by whitespace or EOL
- AND recent 6 raw lines do NOT contain `"esc to interrupt"`
- AND no context exhaustion patterns present

**Working detection:**
- Recent lines contain `"esc to interrupt"` (Claude's active footer)
- OR spinner character (`·`, `✢`, `✳`, `✶`, `✻`, `✽`) followed by `…` or `(thinking`

**Context exhaustion:**
- Screen contains any of: `"context window exceeded"`, `"context window is full"`,
  `"conversation is too long"`, `"maximum context length"`, `"context limit reached"`,
  `"truncated due to context limit"`, `"input exceeds the model"`, `"prompt is too long"`

**JSONL tracker:**
- Discovers session file at `~/.claude/projects/<cwd_encoded>/<session_id>.jsonl`
- Tails from EOF (ignores history on first bind)
- `type: "assistant"` + `stop_reason: "end_turn"` → Idle
- `type: "assistant"` + `stop_reason: "tool_use"` → Working
- `type: "progress"` → Working

**Merge priority:** Screen > Tracker (Claude's visible spinner/prompt is more
reliable than potentially stale JSONL).

#### 3.4.2 Codex Classifier

Detects OpenAI Codex CLI patterns:

**Idle detection:**
- Line contains `›` followed by whitespace or EOL

**Working detection:**
- Active output being produced (screen hash changing)

**JSONL tracker:**
- Discovers session at `~/.codex/sessions/<year>/<month>/<day>/<session_id>.jsonl`
- `type: "event_msg"` + `payload.type: "task_complete"` → Idle (completion)
- Other events → Working

**Merge priority:** Tracker > Screen for completion detection (Codex's
`task_complete` event is ground truth; screen prompt alone is unreliable).

#### 3.4.3 Kiro Classifier

Detects Amazon Kiro CLI patterns:

**Idle detection:**
- Line matches: `Kiro>`, `kiro>`, `Kiro >`, `kiro >`, or bare `>` at line end

**Working detection:**
- Lowercase line contains (`kiro` OR `agent`) AND (`thinking` OR `planning`
  OR `applying` OR `working`)

**No JSONL tracker** — Kiro doesn't expose structured session files (as of
current knowledge).

#### 3.4.4 Generic Classifier

Fallback for unknown agent types:

**Idle detection:**
- Line ends with `$ ` (shell prompt)
- OR line ends with `> ` (generic REPL prompt)

**Working detection:**
- Screen content is changing (hash differs from previous poll)

**No JSONL tracker.**

### 3.5 Message Injection

When the shim receives a `SendMessage` command while in `Idle` state:

1. **Snapshot pre-injection screen** — save `parser.screen().contents()` hash
   to later compute the response diff
2. **Write message bytes to PTY master** — direct write, no paste-buffer
   indirection
3. **Write Enter keystroke** — `\r` or `\n` depending on agent
4. **Transition to `Working` state**
5. **Emit `StateChanged { from: Idle, to: Working }` event**

```rust
fn inject_message(&mut self, from: &str, body: &str) -> Result<()> {
    // Capture pre-injection state for response extraction
    self.pre_injection_content = self.parser.screen().contents();
    self.pre_injection_row = self.parser.screen().cursor_position().0;

    // Format the message (matches current batty convention)
    let formatted = format!(
        "\n--- Message from {} ---\n{}\n--- end message ---\n",
        from, body
    );

    // Write directly to PTY master (no paste-buffer needed!)
    self.pty_writer.write_all(formatted.as_bytes())?;
    self.pty_writer.write_all(b"\r")?; // submit
    self.pty_writer.flush()?;

    self.state = ShimState::Working;
    Ok(())
}
```

**This is dramatically simpler than the current tmux approach** which requires:
load-buffer → paste-buffer → wait-for-paste (10 polls × 200ms) →
submit-with-retry (3 attempts × 800ms). Direct PTY write is a single syscall.

**Message queuing:** If a `SendMessage` arrives while the agent is `Working`,
the shim queues it internally and delivers when the agent returns to `Idle`.
Queue depth limit: 16 messages (configurable). If exceeded, the oldest
message is dropped and an `Error` event is emitted.

### 3.6 Response Extraction

When the agent transitions from `Working → Idle`, the shim extracts what
the agent produced:

```rust
fn extract_response(&self) -> String {
    let current = self.parser.screen().contents();

    // Strategy: find content after the injected message and before the
    // new prompt. We use the scrollback to capture everything.
    let full = self.parser.screen().contents_formatted();

    // Simple approach for POC: last N lines above the cursor
    // (everything between the injected message and the new prompt)
    self.parser.screen().contents_between(
        self.pre_injection_row, 0,
        self.parser.screen().cursor_position().0, self.cols - 1,
    )
}
```

For the POC, a simpler approach: capture the last N lines of screen content
(everything visible above the current prompt line). This works because the
agent's response fills the screen between our message and its new prompt.

### 3.7 JSONL Session Tracking

The JSONL tracker runs as part of the shim's poll loop. It tails the agent's
session file and feeds state signals into the classifier's `merge_verdicts`.

**Discovery:** Each agent type knows where to find its session files:
- Claude: `~/.claude/projects/<cwd_encoded>/<newest>.jsonl`
- Codex: `~/.codex/sessions/<date>/<newest>.jsonl`
- Kiro: none (no known session file format)

**Binding semantics:**
- On first bind: seek to EOF, ignore all historical entries
- On rebind (newer file detected): re-seek to EOF of new file
- On poll: read from last offset, process new lines, advance offset
- File disappearance: log warning, keep last known state

**The tracker is advisory.** The screen classifier is always the primary
signal. The tracker resolves ambiguous screen states (e.g., Codex completion
when screen looks similar to idle-but-not-done).

---

## 4. Communication Protocol

### 4.1 Transport

Unix socketpair with `SOCK_SEQPACKET`. Each `send()` / `recv()` preserves
message boundaries — no framing needed.

**Message format:** JSON-serialized `Command` or `Event`, encoded as UTF-8.
Maximum message size: 1 MB (enforced by recv buffer).

**Why SEQPACKET over STREAM:** SEQPACKET preserves message boundaries. With
STREAM, you need length-prefix framing (4 bytes + payload). SEQPACKET gives
you this for free at the kernel level. Available on macOS and Linux.

**Fallback for portability:** If SEQPACKET is unavailable, fall back to
SOCK_STREAM with length-prefix framing (4-byte big-endian length + JSON
payload).

### 4.2 Commands (Orchestrator → Shim)

```rust
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd")]
enum Command {
    /// Inject a message into the agent's terminal.
    /// Queued if agent is Working; rejected if Dead/Exhausted.
    SendMessage {
        from: String,
        body: String,
        /// Optional: unique ID for delivery tracking
        message_id: Option<String>,
    },

    /// Capture current screen content (equivalent to tmux capture-pane).
    CaptureScreen {
        /// Number of lines to capture from bottom. None = full screen.
        last_n_lines: Option<usize>,
        /// Include scrollback buffer beyond visible screen.
        include_scrollback: bool,
    },

    /// Query current agent state.
    GetState,

    /// Resize the agent's terminal.
    Resize { rows: u16, cols: u16 },

    /// Graceful shutdown: send interrupt to agent, wait for exit, then
    /// terminate shim process.
    Shutdown {
        /// Seconds to wait for graceful exit before SIGKILL.
        timeout_secs: u32,
    },

    /// Immediately kill the agent process and exit the shim.
    Kill,

    /// Ping / keepalive. Shim responds with Pong event.
    Ping,
}
```

### 4.3 Events (Shim → Orchestrator)

```rust
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "event")]
enum Event {
    /// Shim is initialized, agent prompt detected, ready for messages.
    Ready,

    /// Agent state changed.
    StateChanged {
        from: ShimState,
        to: ShimState,
        /// Summary: last N lines of screen at transition time.
        summary: String,
    },

    /// Agent completed processing a message. Contains the response.
    Completion {
        /// The message_id of the SendMessage that triggered this, if provided.
        message_id: Option<String>,
        /// Agent's response text (screen content produced during Working state).
        response: String,
        /// Last N lines of screen at completion time.
        last_lines: String,
    },

    /// Agent process exited.
    Died {
        exit_code: Option<i32>,
        /// Last screen content before death.
        last_lines: String,
    },

    /// Agent reported context exhaustion.
    ContextExhausted {
        /// The exhaustion message detected.
        message: String,
        last_lines: String,
    },

    /// Response to CaptureScreen command.
    ScreenCapture {
        content: String,
        cursor_row: u16,
        cursor_col: u16,
        rows: u16,
        cols: u16,
    },

    /// Response to GetState command.
    State {
        state: ShimState,
        /// Seconds since last state change.
        since_secs: u64,
        /// Seconds since last screen content change.
        idle_secs: u64,
    },

    /// Response to Ping command.
    Pong,

    /// Command was rejected (e.g., SendMessage while Dead).
    Error {
        /// The command that was rejected (tag name).
        command: String,
        reason: String,
    },

    /// Non-fatal warning (e.g., message queue full, session file not found).
    Warning {
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum ShimState {
    Starting,
    Idle,
    Working,
    Dead,
    ContextExhausted,
}
```

### 4.4 Message Flow Examples

**Normal task execution:**
```
Orchestrator                    Shim
    │                             │
    │──── SendMessage ───────────→│
    │                             │ write to PTY
    │←── StateChanged(Idle→Work) ─│
    │                             │ ... agent processing ...
    │←── StateChanged(Work→Idle) ─│
    │←── Completion { response } ─│
    │                             │
```

**Screen capture (debugging):**
```
Orchestrator                    Shim
    │                             │
    │──── CaptureScreen ─────────→│
    │←── ScreenCapture { ... }  ──│
    │                             │
```

**Agent crash:**
```
Orchestrator                    Shim
    │                             │
    │                             │ PTY EOF / waitpid
    │←── StateChanged(→Dead)   ───│
    │←── Died { exit_code, ... } ─│
    │                             │ shim exits
```

---

## 5. Shim CLI Interface

```
batty shim [OPTIONS]

Required:
  --id <AGENT_ID>           Unique agent identifier (e.g., "eng-1")
  --agent-type <TYPE>       Agent backend: claude, codex, kiro, generic
  --cmd <COMMAND>           Shell command to launch the agent CLI
  --cwd <DIRECTORY>         Working directory for the agent

Optional:
  --rows <N>                Terminal rows (default: 50)
  --cols <N>                Terminal columns (default: 220)
  --scrollback <N>          Scrollback buffer lines (default: 5000)
  --env <KEY=VALUE>         Additional environment variable (repeatable)
  --session-id <UUID>       Pre-known session ID for JSONL tracking
  --poll-interval-ms <N>    Screen poll interval in milliseconds (default: 250)
  --log-file <PATH>         Write raw PTY output to file (for debugging)
  --verbose                 Enable debug logging to stderr
```

The channel fd (fd 3) is inherited from the parent process — not specified
on the command line.

---

## 6. Timing and Polling

### 6.1 Internal Poll Loop

The shim runs a single `tokio::select!` loop with three event sources:

```rust
loop {
    tokio::select! {
        // 1. PTY output (bytes from agent)
        result = pty_reader.read(&mut buf) => {
            // Feed to vt100 parser, check for state changes
        }

        // 2. Commands from orchestrator
        result = channel.recv() => {
            // Handle command, send response
        }

        // 3. Periodic poll tick (for JSONL tracker + stale detection)
        _ = poll_interval.tick() => {
            // Poll JSONL tracker
            // Check for stale state (no output change for N seconds)
        }

        // 4. Agent process exit
        result = child_exit.wait() => {
            // Transition to Dead, emit Died event, exit
        }
    }
}
```

### 6.2 Timing Constants

| Constant | Value | Purpose |
|----------|-------|---------|
| `POLL_INTERVAL_MS` | 250 | JSONL tracker poll + stale check interval |
| `READY_TIMEOUT_SECS` | 120 | Max time to wait for first agent prompt |
| `SHUTDOWN_TIMEOUT_SECS` | 30 | Default graceful shutdown timeout |
| `STALE_THRESHOLD_SECS` | 300 | No screen change = stale warning |
| `MAX_MESSAGE_QUEUE` | 16 | Pending message queue depth |
| `MAX_MESSAGE_SIZE` | 1_048_576 | 1 MB max message over channel |
| `SCREEN_HASH_DEBOUNCE_MS` | 50 | Minimum time between classifications |

### 6.3 State Change Detection

State changes are detected by the PTY reader path, not the poll timer:

1. Bytes arrive from PTY → fed to vt100 parser
2. Compute screen content hash
3. If hash changed: run classifier → check for state transition
4. If state changed: emit event immediately

The poll timer is only for:
- JSONL tracker (which reads from a file, not the PTY)
- Stale detection (no PTY output for extended period)
- Ready timeout (agent didn't show prompt within timeout)

This means state changes are detected within milliseconds of the screen
updating, not on a 5-second poll cycle like the current tmux approach.

---

## 7. Error Handling

### 7.1 Agent Crash

If the agent process exits unexpectedly:
1. PTY read returns EOF (or error)
2. `waitpid` returns exit status
3. Shim captures final screen content
4. Emits `Died { exit_code, last_lines }`
5. Emits `StateChanged { to: Dead }`
6. Shim process exits with code 0 (clean shutdown of shim itself)

### 7.2 Channel Disconnection

If the orchestrator dies or closes the socket:
1. `channel.recv()` returns EOF
2. Shim sends SIGTERM to agent, waits for exit (with timeout)
3. If agent doesn't exit within timeout, SIGKILL
4. Shim exits

### 7.3 PTY Errors

If PTY operations fail (write error, resize error):
1. Emit `Error` event with details
2. If write fails: message delivery failed, emit delivery failure
3. If persistent: transition to `Dead` state

### 7.4 JSONL Tracker Errors

Non-fatal. If the session file can't be found or read:
1. Emit `Warning` event
2. Continue with screen-only classification
3. Retry discovery on next poll cycle

---

## 8. Attach Capability

For debugging, the orchestrator (or a user command) can request to "attach"
to see the agent's live terminal output.

### 8.1 Log-Based Attach (POC approach)

The shim optionally writes all raw PTY output (including escape sequences) to
a log file specified by `--log-file`. Attaching means:

```bash
# Replay history + live tail
cat /tmp/batty/eng-1.pty.log     # shows full history with ANSI codes
tail -f /tmp/batty/eng-1.pty.log  # live follow
```

This works because the log file contains the raw byte stream that a terminal
would render. `cat`-ing it to a terminal replays the visual output.

### 8.2 Live PTY Forwarding (Future)

A more sophisticated approach for interactive attach:
1. Orchestrator sends `Attach` command
2. Shim starts forwarding PTY master output to a new Unix socket
3. Attacher connects to that socket, gets live terminal stream
4. Attacher's stdin is forwarded back to PTY master (interactive control)
5. Detach: close the socket, shim stops forwarding

This gives full interactive access identical to `tmux attach` but is more
complex to implement. The log-based approach is sufficient for the POC.

---

## 9. Testing Strategy

### 9.1 Unit Tests

- **Classifier tests:** Feed known screen content to each classifier, verify
  verdicts. Use the existing test cases from `watcher/screen.rs` as a
  starting corpus.
- **State machine tests:** Drive transitions through all paths, verify events
  emitted at each transition.
- **Protocol tests:** Serialize/deserialize all Command and Event variants.
- **Message injection tests:** Verify message formatting, PTY write content.
- **Response extraction tests:** Feed a sequence of screens (pre-injection,
  working, post-completion) and verify extracted response.

### 9.2 Integration Tests (with real PTY)

Spawn the shim with `--agent-type generic --cmd "bash"`:

- Send `echo hello` → verify Completion event contains "hello"
- Send `sleep 2` → verify Working state persists for ~2 seconds, then Idle
- Send `exit` → verify Died event with exit_code 0
- Send CaptureScreen → verify non-empty screen content
- Send Resize → verify screen dimensions change

These tests use a real PTY but no external AI tool — just bash. Fast,
deterministic, and sufficient to validate the shim's core mechanics.

### 9.3 Agent-Specific Integration Tests

With a real Claude/Codex/Kiro CLI:

- Send "say hello" → verify Completion event, response contains text
- Send "write a file /tmp/test.txt with 'hello'" → verify file exists
- Send a large prompt → verify context exhaustion detection (if applicable)

These are slower and require API keys. Gated behind a feature flag.

---

## 10. POC Scope

The POC implements the minimum viable shim:

1. **Single binary** with two modes: `shim` (agent container) and `chat`
   (interactive frontend)
2. **`chat` mode** spawns a shim subprocess, presents a readline prompt, sends
   user input as messages, displays agent responses
3. **`shim` mode** creates PTY, spawns agent, classifies state, handles
   commands
4. **Agent types:** Claude + Generic (bash) for POC
5. **No JSONL tracking** in POC — screen classification only
6. **No attach** in POC — just CaptureScreen command
7. **Transport:** SOCK_SEQPACKET socketpair with JSON messages

### POC Test Scenarios

```
$ cargo run -- chat --agent-type generic --cmd bash
> echo hello world
hello world
> cat /etc/hostname
<hostname output>
> exit
Agent exited with code 0.

$ cargo run -- chat --agent-type claude --cmd "claude --dangerously-skip-permissions"
> say Hello
Hello! How can I help you today?
> write a file /tmp/shim-test.txt containing "it works"
I'll create that file for you.
[file created]
```

The chat frontend:
1. Spawns shim with socketpair
2. Waits for `Ready` event
3. Loops: read user input → `SendMessage` → wait for `Completion` → print
   response
4. On `Died` → print exit info and quit
