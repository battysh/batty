# PRD: Grafana Monitoring for Batty

## Problem

Batty collects rich telemetry (SQLite DB with 4 tables, JSONL event log, per-agent metrics) but the only way to view it is the terminal `batty metrics` command or manually syncing SQLite files to a remote EC2 Grafana instance. There is no real-time, visual, always-on monitoring. Problems like agent stalls, delivery failures, and pipeline starvation are only discovered when a human notices symptoms — not when they begin.

## Goal

A single local Grafana instance per host that provides real-time dashboards for all Batty projects running on that machine. Each project auto-registers its dashboard and pushes stats continuously. The operator can detect problems as they emerge, track productivity over time, and compare across projects and sessions.

## Existing Infrastructure

### What Batty Already Collects

**SQLite Telemetry DB** (`.batty/telemetry.db`, WAL mode):

| Table | Key Fields |
|---|---|
| `events` | timestamp, event_type, role, task_id, payload (JSON) |
| `agent_metrics` | role, completions, failures, restarts, total_cycle_secs, idle_polls, working_polls |
| `task_metrics` | task_id, started_at, completed_at, retries, escalations, merge_time_secs, confidence_score |
| `session_summary` | session_id, started_at, ended_at, tasks_completed, total_merges, total_events |

**Event Types** (40+): daemon lifecycle, agent spawns/restarts/crashes, task assignment/completion/escalation, message routing, delivery failures, merge confidence, pattern detection, standups, retrospectives.

**JSONL Event Log** (`.batty/team_config/events.jsonl`): All events with timestamps.

### Existing Grafana (EC2)

- Production Grafana at `https://batty.sh/dashboard` on EC2 `54.152.179.217`
- Google OAuth (zedmor@gmail.com)
- Uses `frser-sqlite-datasource` plugin to query SQLite directly
- 4 dashboards: agent telemetry (dual-team), marketing metrics, PM metrics, deployment
- Data sync via `sync-telemetry-to-ec2.sh` (manual SCP, not real-time)
- Dashboard JSON definitions in `~/batty_marketing/data/grafana-*.json`

## Architecture

### Single Host Grafana

One Grafana instance per developer machine, running as a lightweight background service. Not containerized — native binary or Homebrew install for simplicity.

```
┌─ Developer Machine ────────────────────────────────────────┐
│                                                            │
│  ┌─ Grafana (localhost:3000) ─────────────────────────┐    │
│  │  Dashboard: batty-dev (project 1)                  │    │
│  │  Dashboard: batty-marketing (project 2)            │    │
│  │  Dashboard: System Overview (all projects)         │    │
│  └────────────────────────────────────────────────────┘    │
│       ▲              ▲              ▲                      │
│       │              │              │                      │
│  ┌────┴───┐    ┌─────┴────┐   ┌────┴────┐                 │
│  │ SQLite │    │  SQLite  │   │ SQLite  │                  │
│  │ (dev)  │    │ (mktg)   │   │ (proj3) │                  │
│  └────────┘    └──────────┘   └─────────┘                  │
│       ▲              ▲              ▲                      │
│  ┌────┴───┐    ┌─────┴────┐   ┌────┴────┐                 │
│  │ batty  │    │  batty   │   │  batty  │                  │
│  │ daemon │    │  daemon  │   │  daemon │                  │
│  └────────┘    └──────────┘   └─────────┘                  │
└────────────────────────────────────────────────────────────┘
```

### Data Flow

1. **Batty daemon** writes telemetry to project-local SQLite DB (already happening)
2. **Grafana** reads SQLite directly via `frser-sqlite-datasource` plugin (no intermediary)
3. **Project registration** — `batty start` registers the project's SQLite path with the local Grafana instance (creates/updates datasource + provisions dashboard)
4. **Dashboard provisioning** — predefined dashboard JSON templates are bundled with batty and deployed via Grafana's provisioning API on registration
5. **Deregistration** — `batty stop` optionally marks the datasource as inactive (dashboard persists for historical viewing)

### Why SQLite Direct (Not Prometheus)

- Batty already writes to SQLite — zero new write-path code
- SQLite handles concurrent reads with WAL mode
- No need for a metrics server process (Prometheus, InfluxDB)
- `frser-sqlite-datasource` plugin is already proven in the EC2 deployment
- Grafana refresh interval (5-10s) provides near-real-time without push infrastructure

## Dashboard Design

### Per-Project Dashboard

**Row 1: Session Overview**
- Session uptime (gauge)
- Tasks completed this session (stat)
- Tasks in progress (stat)
- Active engineers / total engineers (stat)
- Current throughput: tasks/hour (stat)

**Row 2: Pipeline Health (time series)**
- Task state distribution over time (stacked area: backlog, todo, in-progress, review, done)
- Engineer utilization over time (line: working % vs idle %)
- Delivery success rate over time (line: target 98%+)

**Row 3: Agent Performance**
- Per-agent completions, failures, restarts (table)
- Average cycle time by agent (bar chart)
- Failure rate by agent (bar chart, threshold alert at >20%)

**Row 4: Delivery & Communication**
- Messages routed per minute (time series)
- Delivery failures (time series, alert threshold)
- Top message routes (bar chart)
- Escalation count (stat)

**Row 5: Task Lifecycle**
- Average task cycle time over time (time series)
- Retries and escalations per task (scatter plot)
- Merge confidence score distribution (histogram)
- Auto-merge vs manual merge ratio (pie chart)

**Row 6: Recent Activity**
- Last 50 events (table, auto-refresh)
- Active alerts (if any)

### System Overview Dashboard (All Projects)

- Project cards: each project shows session status, task count, throughput, health
- Aggregate throughput across all projects
- Alert summary

## Implementation

### Phase 1: Local Grafana Setup (batty grafana)

New CLI commands:

- `batty grafana setup` — install Grafana (via Homebrew on macOS, apt on Linux), install `frser-sqlite-datasource` plugin, start Grafana service, configure default admin credentials
- `batty grafana status` — show Grafana URL, registered projects, service health
- `batty grafana open` — open Grafana in default browser

Setup is idempotent — running it twice is safe.

### Phase 2: Project Registration

- `batty start` calls registration automatically (if Grafana is running)
- Registration creates a Grafana datasource pointing to the project's `.batty/telemetry.db`
- Registration provisions the per-project dashboard from a bundled JSON template
- Datasource name: `batty-<project-name>` (derived from directory name or team.yaml team name)
- Dashboard UID: `batty-<project-name>` (stable, survives re-registration)

### Phase 3: Dashboard Templates

- Bundle dashboard JSON as `include_str!()` in the binary (same pattern as prompt templates)
- Template uses variables for datasource name so the same JSON works for any project
- Dashboard JSON based on the proven `grafana-telemetry-dashboard.json` from batty_marketing, adapted for single-project use

### Phase 4: Real-Time Push (Optional Enhancement)

If SQLite polling latency (5-10s Grafana refresh) is insufficient:

- Add a lightweight WebSocket or SSE endpoint to the batty daemon
- Grafana connects via the `grafana-websocket-plugin` or `grafana-live`
- Daemon pushes events as they happen
- This is an enhancement, not a requirement — SQLite polling is sufficient for v1

### Phase 5: EC2 Sync Integration

- `batty grafana sync` pushes local telemetry to the EC2 Grafana instance
- Uses the existing `sync-telemetry-to-ec2.sh` pattern but automated
- Optional: cron-based auto-sync every N minutes
- Keeps the remote dashboard updated for mobile/remote monitoring

## Configuration

```yaml
# team.yaml additions
grafana:
  enabled: true                    # default: false
  url: "http://localhost:3000"     # local Grafana URL
  refresh_interval_secs: 10       # dashboard auto-refresh
  auto_register: true             # register on batty start
  sync_remote: false              # push to EC2 Grafana
  remote_url: "https://batty.sh/dashboard"
  sync_interval_secs: 300         # every 5 minutes
```

## Alerts

Grafana alert rules provisioned with the dashboard:

| Alert | Condition | Severity |
|---|---|---|
| Agent stall | Working agent with 0 events in 5 min | Warning |
| Delivery failure spike | >3 delivery failures in 1 min | Critical |
| Pipeline starvation | 0 todo tasks + idle engineers for 10 min | Warning |
| High failure rate | Agent failure rate >30% over 15 min | Warning |
| Context exhaustion | context_exhausted event | Info |
| Session idle | No events for 15 min during active session | Warning |

## Success Criteria

1. `batty grafana setup` installs and starts Grafana in <2 minutes
2. `batty start` auto-registers project and dashboard appears within 10 seconds
3. Dashboard shows real-time metrics with <10s latency
4. Problems (stalls, delivery failures, starvation) are visible within 30 seconds of occurrence
5. Multiple concurrent Batty projects show independent dashboards under the same Grafana instance
6. Historical data survives `batty stop` / `batty start` cycles
7. Works on macOS (Homebrew) and Linux (apt)

## Dependencies

- Grafana (open source, Homebrew/apt installable)
- `frser-sqlite-datasource` Grafana plugin (already proven in production)
- No new Rust dependencies (HTTP calls to Grafana API via existing `ureq`)

## Out of Scope (v1)

- Windows support
- Multi-host Grafana federation
- Custom dashboard editor in batty CLI
- Prometheus/InfluxDB/TimescaleDB backends
- Grafana Cloud integration
- User authentication beyond default admin (local-only, single-user)

## Risks

1. **Grafana installation friction** — mitigated by Homebrew/apt automation and idempotent setup
2. **SQLite locking under heavy write + Grafana read** — mitigated by WAL mode (already enabled)
3. **Dashboard JSON drift** — mitigated by bundling templates in the binary, versioned with batty releases
4. **Grafana version compatibility** — pin to Grafana 10.x, test plugin compatibility
