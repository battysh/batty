# Batty Issues Observed During InscopeDataAgent Decoupling Project

**Compiled:** 2026-04-23
**Source sessions:** InscopeDataAgent continuous-improvement team, 2026-04-19 → 2026-04-22
**Batty version:** 0.11.58 (custom build at `~/.local/bin/batty` with 4 local patches: multi-repo support, mainline branch detection, Opus 4.7 model, preflight skip)
**Evidence dirs:**
- `/home/zedmor/workplace/InscopeDataAgent/src/.batty/` (daemon logs, telemetry, inboxes, worktrees)
- `/home/zedmor/workplace/InscopeDataAgent/src/InscopeDataAgent/docs/batty_integration_notes.md` (hand-written field notes)
- `/home/zedmor/workplace/batty/planning/bugreport-*.md` (pre-existing bug reports, still unresolved)

Prioritized (P0 blocks usage, P1 causes real data-integrity risk, P2 is recurring noise).

---

## P0-1 — Worktree health checks assume git repo at workspace root (multi-repo regression)

**Symptom:** Daemon log floods with `failed to read worktree branch; skipping staleness check` and `failed to check uncommitted diff ... git status failed in .../worktrees/eng-1-N` every 5 seconds, forever.

**Evidence:** `/home/zedmor/workplace/InscopeDataAgent/src/.batty/daemon.log` current tail — 16,002 of these in `checks` module alone (single most common log line in the file).

**Root cause:** Batty's health check runs `git status` / `git rev-parse` against `.batty/worktrees/eng-1-N/`. In multi-repo Brazil workspaces there is NO git repo at that level — the git repos live one level deeper at `.batty/worktrees/eng-1-N/<sub-repo>/`. The multi-repo patch handles `assign`/`claim` but forgot to teach the health checker.

**Impact:**
- Staleness detection is completely broken — never fires even when branches actually diverge.
- Claim-TTL reset (`failed to recreate 'eng-main/eng-1-3' from 'main'`) can never auto-repair a rotated worktree; engineer stays broken until human fixes it.
- Shim completion detection also fails: `git diff --name-only main..HEAD failed ... Not a git repository` → review packets from engineers sometimes never transition the board.

**Fix direction:** Health check should either (a) iterate sub-repos when `multi_repo=true`, or (b) be disabled when workspace type is brazil. Probably (a) — we still want staleness signals, just per sub-repo.

---

## P0-2 — `main` vs `mainline` mismatch baked into daemon paths

**Symptom:** Multiple code paths hard-code `main` as the trunk branch name, breaking on every Amazon sub-repo (which defaults to `mainline`).

**Evidence (from daemon logs and integration notes):**
- `shim completion handling failed ... git diff --name-only main..HEAD failed`
- `claim ttl: failed to reset engineer worktree before reclaim ... failed to recreate 'eng-main/eng-1-3' from 'main'`
- Completion verifier probing `main` is a prime suspect for the phantom `done → review` regression (P1-3 below).

**Impact:**
- Completion diffs always empty → verifier can't tell if work was done.
- TTL-reset code can't recreate `eng-main/<eng>` because source branch `main` doesn't exist.
- Engineers manually work around via `origin/mainline` but the daemon keeps fighting them.

**Fix direction:** Single config knob `trunk_branch: mainline` in `team.yaml` consumed by all internal paths (shim, health, automation, verifier). The existing local patch covers *some* paths but misses at least 3 (based on log evidence).

---

## P0-3 — `eng-main/<eng>` baseline branch is silently deleted after direct-merge, blocking next assignment

**Symptom:** When manager direct-merges an engineer branch to `mainline` and deletes the `engineer/<branch>` ref, `eng-main/<eng>` in the SAME sub-repo can also disappear. Next `batty assign` for that engineer on that sub-repo fails with:
```
failed to compare worktree branch with main: permanent git error:
fatal: Not a valid object name eng-main/<eng>
```

**Evidence:** Observed 2026-04-19 on eng-1-1 / InscopeDataAgent after A1 merge. Documented in `batty_integration_notes.md` Section 6.

**Impact:** Team stalls until a human runs `git fetch origin mainline && git checkout -B eng-main/<eng> origin/mainline` manually in the right sub-repo.

**Fix direction:** Pre-check on `batty assign`; auto-recreate from `origin/<trunk>` if missing.

---

## P0-4 — Stuck messages after injection (paste-buffer race)

**Symptom:** After `inject_message()` pastes into the Claude Code pane and sends Enter, the text is visible at the `❯` prompt but Claude Code never processes it. Agent appears to ignore messages sent via Telegram/Slack.

**Evidence:** `~/workplace/batty/planning/bugreport-stuck-messages-after-injection.md` — pre-existing, still unresolved.

**Root cause:** Race between tmux `paste-buffer` and Claude Code's input polling. Text lands in terminal buffer but Claude Code's event loop misses it; subsequent Enter keystrokes get consumed before it polls. Particularly bad when the agent has been idle, messages are long, or panes are narrow.

**Impact:** `recover_stuck_messages` is a workaround, not a fix. Silent delivery failures still happen.

**Fix direction:** Replace paste-buffer with direct PTY write + timing-independent submit (e.g., send a no-op then the payload then Enter, or move to a sidecar input file Claude Code watches).

---

## P0-5 — "marker missing" cascade leaves whole team paralyzed in `starting` state

**Symptom:** Daemon reports `message marker missing after injection; resending Enter ... attempt=1/2/3`, then escalates to parent lead, whose delivery also fails. Every agent stuck in `starting` — team is live but does no work.

**Evidence:** `~/workplace/batty/planning/bugreport-delivery-marker-missing-starting-agents.md` (batty-marketing session 2026-03-22, same daemon). We did not hit the full cascade on InscopeDataAgent (smaller team, different timing), but the same injection code path is in use.

**Impact:** When it fires, the entire team becomes a black hole. No progress, no clean recovery, daemon reports healthy.

**Fix direction:** Same as P0-4 — injection reliability. Add a delivery ACK loop where the target pane echoes a known sentinel back to the daemon so we can distinguish "not yet visible" from "permanently lost."

---

## P1-1 — Lead inboxes don't trigger prompt triage (throughput bug)

**Symptom:** Engineer delivers a REVIEW packet to the lead successfully. `batty status` / `batty load` shows lead idle and team underutilized (observed 10% load = 1/10 working). Lead sits on the evidence for minutes, forcing architect/human to intervene.

**Evidence:** `~/workplace/batty/planning/bugreport-lead-inbox-triage-idle.md`. On InscopeDataAgent, our 3-eng team dodged most of this because `auto_dispatch: false` forced synchronous dispatch, but that is a workaround that caps throughput.

**Impact:** Real work sits in inboxes; compute is burned on idle loops; architect becomes the bottleneck.

**Fix direction:** Lead should be nudged immediately on inbox delivery (not on utilization-recovery timer), and the nudge should include the packet's contents, not just a "check inbox" prompt.

---

## P1-2 — Canned `batty merge` remediation is unsafe after branch rotation

**Symptom:** Daemon nudges `batty merge <engineer>` + `kanban-md move <task> done` as canned review-backlog remediation. But if the engineer has already rotated off the completed branch onto a new one, running the canned command merges the WRONG branch (the in-progress one) under the completed task's label.

**Evidence:** Near-miss 2026-04-19, task #2 (A2 SQL Library). eng-1-2 had rotated from `eng-1-2/a2-sql-example-library` (already merged) to `eng-1-2/11`. Canned remediation would have merged in-progress #11 under A2's label. Documented in `batty_integration_notes.md` Section 8.

**Impact:** **Material correctness risk.** Silent wrong-branch merges to mainline.

**Fix direction:**
- Daemon must verify `git branch --show-current` in the worktree matches the task's recorded branch BEFORE proposing `batty merge`.
- Include `git log --oneline mainline..<task-branch>` check — if empty (commits already in trunk), propose `kanban-md move` + `batty review approve` normalization instead.

---

## P1-3 — Phantom `done → review` lane regression (attribution missing)

**Symptom:** 2026-04-19T19:25:59Z — task #2 (A2) flipped from `status: done` back to `status: review` in board frontmatter with no human/agent command. `activity.jsonl` recorded the move but did not attribute it. Daemon's review-backlog nudge then fired on the normalized task.

**Evidence:** `batty_integration_notes.md` Section 7.

**Candidate root causes (unresolved):**
- (a) Daemon intervention path that auto-demotes on some signal we haven't identified.
- (b) Completion verifier probing `main` (P0-2) misinterpreting a missing ref as a completion failure.
- (c) Race between retroactive `batty review approve` and `kanban-md` state machine.

**Impact:** Board state diverges from reality. Requires human normalization via `kanban-md move --claim` + retroactive `batty review approve`.

**Fix direction:** Every lane transition in `activity.jsonl` MUST record `actor` (daemon-module / user / agent). Once attributed, the bug becomes findable.

---

## P1-4 — Orphaned review/in-progress tasks with no owner

**Symptom:** Daemon logs:
- `orphaned review task #17 has no owner — moving back to todo`
- `orphaned in-progress task #19 has no owner — moving back to todo`

**Evidence:** `daemon.log.1` on 2026-04-20. Two separate tasks, different stages.

**Root cause (likely):** When an engineer is reclaimed mid-task (e.g., after claim TTL elapses and worktree-reset fails per P0-1/P0-2/P0-3), the task is left in-lane with no owner. Daemon eventually notices and kicks it back to `todo`, losing partial work context.

**Impact:** Partial work gets silently demoted. Engineer has to rediscover where they left off next time they pick up the task.

**Fix direction:** Before kicking to `todo`, capture engineer's last commit + worktree diff into task notes so resumption has breadcrumbs.

---

## P1-5 — Harness hangs (unresolved — task #21 assigned for reproducer)

**Symptom:** Batty's test-harness / supervision-harness hangs during certain shim operations. Parked as task #21 (assigned to zedmor 2026-04-20 for morning reproducer work). Root cause never identified.

**Evidence:** `~/workplace/InscopeDataAgent/tasks/phase-5-rca.md`; memory history 2026-04-20 08:12 UTC ("task #21 (harness-hang investigation)").

**Impact:** Unknown until reproduced. Potential blocker for enabling auto_dispatch at scale.

**Fix direction:** Reproduce first. Likely candidates: tmux session state drift, stalled kiro-cli subprocess, or shim waiting on a marker that never arrives (overlap with P0-4/P0-5).

---

## P1-6 — AIM package eventId-* dirs bleed across DevSpace launches

**Symptom:** `aim agents uninstall --force` is soft-delete — removes logical AIM record but leaves `~/.aim/packages/*/eventId-*/` directories on disk. Persistent home storage causes stale packages to bleed across DevSpace launches → 4x catalog duplication in extreme cases.

**Evidence:** Learned lesson (memory 2026-04-19, episodic). Fixed workaround at commit fb3b1e1 in `blueprint_startup.sh` (keep only newest eventId dir).

**Impact:** Not strictly a batty bug, but interacts with batty-launched engineer sessions that consume the schema catalog. Users running batty on DevSpaces see phantom retrieval hits.

**Fix direction:** This is an AIM bug, not batty. Flag upstream; keep our workaround in `blueprint_startup.sh` meanwhile.

---

## P2-1 — Worktree-root cosmetic staleness warnings against `main`

**Symptom:** `batty status` emits worktree-staleness warnings against `main` from the workspace root. Cosmetic — no git repo there — but noisy.

**Evidence:** `batty_integration_notes.md` Section 1.

**Fix direction:** In multi-repo mode, suppress warnings at workspace root; only warn per sub-repo.

---

## P2-2 — Utilization-recovery / planning-cycle nudges during intentional phase gates

**Symptom:** When roadmap is phase-gated (Phase A must complete before B is dispatchable), daemon fires nudges because it sees idle engineers + undispatchable todo tasks. Leads/engineers are tempted to invent filler work or edit protected roadmap files to quiet it.

**Evidence:** `batty_integration_notes.md` Section 4 — had to explicitly document "these nudges are expected during gated windows."

**Impact:** Prompt noise; occasional agent over-reach into protected `planning/continuous_improvement/` files. Mitigated with explicit prompts, not code.

**Fix direction:** Daemon should read phase-gate config from roadmap and suppress idle-nudges when the gate explicitly blocks dispatch. Or expose a `nudge_replenish_disabled` file primitive (already partially exists as marker file) to the manager prompt.

---

## P2-3 — `brazil-build` cross-package deps don't resolve inside worktrees

**Symptom:** Heavy `brazil-build` invocations from within `.batty/worktrees/<eng>/<pkg>` sometimes can't resolve cross-package Brazil deps. Engineers must `cd ~/workplace/InscopeDataAgent/src/<package>` (original, not worktree) for heavy builds, then move artifacts back.

**Evidence:** `05_BATTY_INTEGRATION.md` "Known gotchas" #1.

**Impact:** Engineers lose isolation benefit of worktrees for Brazil-heavy tasks. Artifact shuffling is error-prone.

**Fix direction:** Batty could create worktrees as real Brazil workspace siblings (with `packageInfo`) rather than bare git worktrees. Investigate whether `brazil ws use` can register a worktree in place.

---

## P2-4 — MCP-server collisions between concurrent engineers

**Symptom:** Each engineer spawns its own MCP server set. If any server holds a DDB lock or singleton file, concurrent engineers can clash. We mitigated by running only 1 active engineer for task #1, scaling up as confidence grew.

**Evidence:** `05_BATTY_INTEGRATION.md` "Known gotchas" #3.

**Impact:** Silent test failures or corrupted shared state. Why we defaulted to `auto_dispatch: false`.

**Fix direction:** Either namespace MCP servers per engineer (port / socket / DDB prefix), or serialize access to shared resources via the daemon.

---

## P2-5 — `kanban-md` default path mismatch

**Symptom:** `kanban-md` expects `board/kanban/config.yml`; batty may look for `board/tasks/` directly. Mismatch needed manual `team.yaml` adjustment.

**Evidence:** `05_BATTY_INTEGRATION.md` "Known gotchas" #2.

**Fix direction:** Pin convention; detect on `batty validate` and fail loudly with the fix command.

---

## P2-6 — Pinned model string is deprecated in mainline batty

**Symptom:** Mainline batty sets `KIRO_DEFAULT_MODEL = "claude-opus-4.6-1m"` — deprecated per `kiro-cli chat --list-models`. We carry a local 1-line patch to `claude-opus-4.7`.

**Evidence:** `~/workplace/batty/` custom build, patch applied to `src/agent/kiro.rs:16`.

**Fix direction:** Upstream a model config knob (don't hard-code). File PR.

---

## P2-7 — Preflight-skip patch is a workaround, not a fix

**Symptom:** We carry a 4th patch disabling preflight concurrent-run check. Upstream preflight walks up the process tree looking for ancestor `batty` processes, false-positives when launched through wrapper shells.

**Evidence:** Memory note — `runner.py preflight_no_concurrent_run now walks full ancestor chain via /proc/<pid>/stat ...` — that's our MeshClaw fix; batty itself still has the naive `ppid`-only check.

**Impact:** Can't run batty inside nested shell wrappers without the skip patch.

**Fix direction:** Port our `/proc` ancestor-walk logic upstream.

---

## Cross-cutting themes

1. **Multi-repo Brazil is a first-class use case, not a patch target.** P0-1, P0-2, P0-3, P2-3 all share the same root: batty assumes a single git repo at workspace root. The right structural fix is a `workspace_type: {single_repo, multi_repo, brazil}` config that forks every git-touching path.

2. **Injection reliability is the #1 liability.** P0-4, P0-5, P1-5 all hinge on the paste-buffer → pane-polling race. One correct fix (ACK loop + direct PTY write) closes three bug reports.

3. **Daemon attribution is missing.** P1-3 (phantom lane moves) and P1-4 (orphaned tasks) would both be trivially diagnosable if `activity.jsonl` recorded `actor` consistently.

4. **Canned remediations need guardrails.** P1-2 nearly caused a wrong-branch merge. Any `batty <action>` the daemon suggests must first verify current worktree state matches the action's assumption.

---

## Suggested fix order

| Priority | Fix | Approx effort | Unblocks |
|---|---|---|---|
| 1 | P0-4 + P0-5 injection reliability (shared fix) | Medium | Entire team stability |
| 2 | P0-1 multi-repo health checks | Small-Medium | Stops log flood, fixes TTL-reset, fixes completion |
| 3 | P0-2 `main` vs `mainline` config | Small | Fixes shim completion, unblocks P1-3 diagnosis |
| 4 | P1-2 `batty merge` safety pre-check | Small | Eliminates correctness risk |
| 5 | P1-3 add actor attribution to activity.jsonl | Small | Makes all future lane bugs diagnosable |
| 6 | P0-3 `eng-main/<eng>` auto-recreate | Small | Removes manual intervention step |
| 7 | P1-1 lead inbox → immediate triage | Medium | Real throughput gain; enables auto_dispatch |
| 8 | P1-5 harness-hang reproducer | Unknown | Unblocks scale testing |

Upstream-to-mainline tasks (P2-6 model config, P2-7 preflight ancestor walk) can run in parallel — they're small and independent.

zedmor at dev-dsk-zedmor-1a-5c1a5669 in ~/workplace/batty/planning (main●)
