# Batty Agent Steering

This repository is managed with Batty.

## Execution Model: Phase as Unit of Work

The unit of supervised work is a **whole phase** (could also be a sprint, story, or milestone). You work through the phase board from start to finish in a single session. Tasks are your checklist, not your branching strategy.

This means:
- **No per-task branches.** Work on `main` (or a single phase branch if needed).
- **Commit at natural checkpoints** — after completing a task or a logical group of tasks. Don't wait until the entire phase is done if there's meaningful progress to save.
- **Manage the board as you go.** Claim tasks, move them through statuses, write statements of work.
- **The session is the unit.** One agent, one phase, start to finish.

## Workflow

The phase to work on will be specified in the prompt (e.g., `.batty/kanban/phase-2/`). All kanban-md commands must use `--dir .batty/kanban/<phase>/` to target the correct board.

> **Note:** Older projects may use `kanban/` instead of `.batty/kanban/`. Batty resolves the active location automatically.

1. Check the board: `kanban-md board --compact --dir .batty/kanban/<phase>`
2. Generate agent name: `kanban-md agent-name` (remember it for the session)
3. Review all tasks to understand the full phase scope
4. Pick the next unblocked task: `kanban-md pick --claim <agent-name> --status backlog --move in-progress --dir .batty/kanban/<phase>`
5. Read the task: `kanban-md show <ID> --dir .batty/kanban/<phase>`
6. Implement and test the work
7. Write a statement of work on the task (see Statement of Work below)
8. Mark done: `kanban-md move <ID> done --dir .batty/kanban/<phase>`
9. Commit with a detailed message (see Commit Messages below)
10. Pick next task and continue until the phase is complete

## Commit Messages

Commit at natural checkpoints — after completing a task or a coherent group of changes. Write detailed commit messages that serve as a record of what changed and why:

```
phase-<N>/<task-IDs>: <short summary>

What: <what was implemented/changed>
Why: <why this approach was chosen>
How: <key implementation details — files created, patterns used, decisions made>

Tasks completed: <list of task IDs and titles>
Files: <list of key files added or modified>
```

Keep the first line under 72 characters. The body should give enough context that someone reading `git log` understands the full scope of the change without reading the diff.

## Statement of Work

After completing each task, update the task file with a statement of work. This is the project's progress documentation — future agents and humans read it to understand what was done.

Use `kanban-md edit <ID> -a "note" -t --dir .batty/kanban/<phase>` to append a timestamped note, or edit the task file directly to add a `## Statement of Work` section:

```markdown
## Statement of Work

- **What was done:** Brief description of the deliverable
- **Files created:** List of new files with one-line purpose each
- **Files modified:** List of changed files with what changed
- **Key decisions:** Any design choices or trade-offs made
- **How to verify:** Command to run or thing to check that proves it works
- **Open issues:** Anything deferred, known limitations, or follow-up needed
```

This is not optional. Every completed task must have a statement of work before being marked done.

## Rules

- Always claim before starting work.
- Work directly on `main` — no per-task feature branches.
- Commit after each completed task or logical group of tasks. Don't accumulate too much uncommitted work.
- Run the project's test/verification suite before every commit — all tests must pass.
- Run kanban-md commands from the project root with `--dir .batty/kanban/<phase>`.
- Leave progress notes: `kanban-md edit <ID> -a "note" -t --claim <agent> --dir .batty/kanban/<phase>`
- If blocked, hand off: `kanban-md handoff <ID> --claim <agent> --note "reason" -t --release --dir .batty/kanban/<phase>`

## Guardrails

- Keep changes deterministic and idempotent where possible.
- Prefer small, composable implementations over abstractions.
- Do not skip failing tests or quality checks.
