# Scheduled Tasks and Cron Recurrence

Batty supports two scheduling mechanisms for tasks:

- **Scheduled dispatch** (`scheduled_for`) delays a task until a specific time.
- **Cron recurrence** (`cron_schedule`) makes a task repeat on a schedule.

Both are set via the `batty task schedule` command and stored as YAML frontmatter fields in the task's Markdown file.

## Scheduled Tasks

The `scheduled_for` field holds an RFC 3339 timestamp. The dispatcher treats the task as blocked until that time passes.

Set a scheduled time:

```sh
batty task schedule 42 --at '2026-03-25T09:00:00-04:00'
```

The task remains in its current status (typically `todo` or `backlog`) but the daemon will not dispatch it until the timestamp is reached. The resolver marks it as `Blocked` with a reason like `scheduled for 2026-03-25T09:00:00-04:00`.

Once the time passes, the task becomes `Runnable` and enters the normal dispatch queue.

### How dispatch gating works

The `Task::is_schedule_blocked()` method checks whether `scheduled_for` is in the future. Two places use this:

1. **Resolver** (`src/team/resolver.rs`) — marks the task as `Blocked` with a scheduling reason.
2. **Dispatch** (`src/team/dispatch.rs`) — filters out schedule-blocked tasks from the candidate list.

A task with no `scheduled_for` field is never schedule-blocked.

## Recurring Tasks (Cron)

The `cron_schedule` field holds a standard cron expression. When a cron task reaches `done`, the daemon automatically recycles it back to `todo` for the next occurrence.

Set a cron schedule:

```sh
# Run every Monday at 9 AM
batty task schedule 42 --cron '0 9 * * MON'
```

Standard 5-field cron expressions are supported. Batty auto-prepends a `0` seconds field internally (the underlying `cron` crate requires 6-7 fields), so you write normal cron syntax.

### Cron expression examples

| Expression | Meaning |
|---|---|
| `* * * * *` | Every minute |
| `0 9 * * *` | Daily at 9:00 AM |
| `0 9 * * MON` | Every Monday at 9:00 AM |
| `30 8 * * 1-5` | Weekdays at 8:30 AM |
| `0 */2 * * *` | Every 2 hours |
| `0 9 1 * *` | First of every month at 9:00 AM |

### How the cron recycler works

The recycler runs as part of the daemon poll loop. On each tick it:

1. Loads all tasks from the board.
2. Finds tasks that are `done` and have a `cron_schedule`.
3. Skips archived or in-progress tasks.
4. Parses the cron expression and determines the next trigger after `cron_last_run` (or now minus 1 day if no last run exists).
5. If the next trigger is in the past (i.e., a run is due), the recycler:
   - Sets `status` back to `todo`.
   - Sets `scheduled_for` to the next **future** occurrence.
   - Updates `cron_last_run` to now.
   - Clears transient fields: `claimed_by`, `branch`, `commit`, `worktree_path`, `blocked_on`, `review_owner`.
   - Emits a `task_recycled` event.

The recycled task then enters the normal dispatch flow. Because `scheduled_for` is set to the next future occurrence, the task is initially blocked until that time arrives.

### Missed triggers

If the daemon was stopped and a cron trigger was missed, the recycler catches up: it detects that the trigger time is in the past and recycles immediately. The new `scheduled_for` is set to the next future occurrence from now, not from the missed trigger.

## Combining Scheduled and Cron

You can set both fields at once:

```sh
batty task schedule 42 --at '2026-03-25T09:00:00-04:00' --cron '0 9 * * MON'
```

This means:
- The task is initially blocked until March 25 at 9 AM.
- After it completes (reaches `done`), the cron recycler moves it back to `todo` with the next Monday 9 AM as the new `scheduled_for`.

## Clearing a Schedule

Remove both scheduling fields:

```sh
batty task schedule 42 --clear
```

This removes `scheduled_for` and `cron_schedule` from the task frontmatter. The task becomes immediately dispatchable (assuming no other blockers).

## CLI Reference

```
batty task schedule <TASK_ID> [OPTIONS]

Arguments:
  <TASK_ID>  Task id

Options:
  --at <AT>      Scheduled datetime in RFC3339 format
  --cron <CRON>  Cron expression (e.g. '0 9 * * *')
  --clear        Clear both scheduled_for and cron_schedule
```

At least one of `--at`, `--cron`, or `--clear` is required. Invalid timestamps and cron expressions are rejected with a descriptive error.

## Task Frontmatter Fields

These fields are stored in the YAML frontmatter of each task file:

| Field | Type | Description |
|---|---|---|
| `scheduled_for` | RFC 3339 string | Dispatch is blocked until this time |
| `cron_schedule` | Cron expression | Recurrence pattern for auto-recycling |
| `cron_last_run` | RFC 3339 string | Timestamp of the last cron recycle (set by daemon) |

Example task file:

```yaml
---
id: 42
title: Weekly standup report
status: todo
priority: medium
cron_schedule: "0 9 * * MON"
scheduled_for: "2026-03-31T09:00:00Z"
cron_last_run: "2026-03-24T09:00:00Z"
---

Generate and distribute the weekly standup report.
```

## Validation

- `--at` values must be valid RFC 3339 timestamps. Example: `2026-03-25T09:00:00-04:00`.
- `--cron` values must be valid cron expressions (5 fields). Invalid expressions are rejected at input time, not at recycle time.
- Running `--clear` with no other flags removes both fields unconditionally.
